//! The macOS system calls behind the backend's seams, through objc2:
//! `HIServices` (AX), `AppKit` (workspace, pasteboard), `CoreGraphics`
//! (events, the window list, the session, the screen-capture preflight)
//! and Vision.
//!
//! This is the crate's one `unsafe` module. Every block is a call into an
//! Apple framework whose contract objc2 cannot check (out-parameters,
//! extern statics, generic reinterpretations of CF collections); each
//! carries its `SAFETY:` invariant. No decision is made here: the rules
//! live in the parent module and in [`super::ax`], tested on every host.
#![allow(unsafe_code)]

use std::path::Path;
use std::ptr::NonNull;
use std::time::Duration;

use objc2::rc::Retained;
use objc2::AnyThread;
use objc2_app_kit::{
    NSApplicationActivationOptions, NSApplicationActivationPolicy, NSPasteboard,
    NSPasteboardTypeHTML, NSPasteboardTypeString, NSWorkspace,
};
use objc2_application_services::{
    kAXTrustedCheckOptionPrompt, AXError, AXIsProcessTrustedWithOptions, AXUIElement,
    AXValue as CfAxValue, AXValueType,
};
use objc2_core_foundation::{
    CFArray, CFBoolean, CFCopyDescription, CFDictionary, CFIndex, CFNumber, CFRange, CFRetained,
    CFString, CFType, CGPoint, CGSize,
};
use objc2_core_graphics::{
    kCGWindowBounds, kCGWindowNumber, CGEvent, CGEventFlags, CGEventType, CGMouseButton,
    CGPreflightScreenCaptureAccess, CGScrollEventUnit, CGSessionCopyCurrentDictionary,
    CGWindowListCopyWindowInfo, CGWindowListOption,
};
use objc2_foundation::{NSArray, NSBundle, NSData, NSDictionary, NSString, NSURL};
use objc2_vision::{VNImageRequestHandler, VNRecognizeTextRequest, VNRequest};

use super::ax::{Ax, AxError as RawAxError, AxValue};
use super::events::{MacEvent, MouseKind};
use super::{Desktop, PostError, WorkspaceApp};
use crate::element::Rect;
use crate::keymap::Modifier;
use crate::platform::{
    Clipboard, ClipboardSnapshot, MouseButton, PasteFormat, RecognizedText, TextRecognizer,
};

/// `kAXErrorFailure`: a value that could not even be built.
const AX_FAILURE: RawAxError = RawAxError(-25200);

/// One `AXUIElementRef`.
#[derive(Clone)]
pub(crate) struct AxNode(CFRetained<AXUIElement>);

// SAFETY: an AXUIElementRef is an immutable CF object naming a remote
// element (a pid plus an opaque token); CF reference counting is atomic,
// and the AX client calls are Mach messages that pyobjc already issued from
// the kernel's worker threads. Nothing in the reference is thread-affine.
unsafe impl Send for AxNode {}
// SAFETY: as above; shared references only ever pass the element to AX calls.
unsafe impl Sync for AxNode {}

fn description(value: &CFType) -> String {
    CFCopyDescription(Some(value)).map_or_else(String::new, |text| text.to_string())
}

/// Decode one copied attribute value into the seam's shape.
fn decode(value: CFRetained<CFType>) -> AxValue<AxNode> {
    if let Some(text) = value.downcast_ref::<CFString>() {
        return AxValue::Text(text.to_string());
    }
    if let Some(flag) = value.downcast_ref::<CFBoolean>() {
        return AxValue::Bool(flag.as_bool());
    }
    if let Some(number) = value.downcast_ref::<CFNumber>() {
        if !number.is_float_type() {
            if let Some(integer) = number.as_i64() {
                return AxValue::Integer(integer);
            }
        }
        return number
            .as_f64()
            .map_or_else(|| AxValue::Other(description(&value)), AxValue::Float);
    }
    if let Some(wrapped) = value.downcast_ref::<CfAxValue>() {
        let mut point = CGPoint { x: 0.0, y: 0.0 };
        // SAFETY: `point` is a live CGPoint, the structure AXValueGetValue
        // writes for kAXValueTypeCGPoint; it writes nothing on a type mismatch.
        if unsafe { wrapped.value(AXValueType::CGPoint, NonNull::from(&mut point).cast()) } {
            return AxValue::Geometry {
                pair: (point.x, point.y),
                description: description(&value),
            };
        }
        let mut size = CGSize {
            width: 0.0,
            height: 0.0,
        };
        // SAFETY: as above, with a live CGSize for kAXValueTypeCGSize.
        if unsafe { wrapped.value(AXValueType::CGSize, NonNull::from(&mut size).cast()) } {
            return AxValue::Geometry {
                pair: (size.width, size.height),
                description: description(&value),
            };
        }
        return AxValue::Other(description(&value));
    }
    if value.downcast_ref::<AXUIElement>().is_some() {
        return match value.downcast::<AXUIElement>() {
            Ok(element) => AxValue::Element(AxNode(element)),
            Err(value) => AxValue::Other(description(&value)),
        };
    }
    if let Some(array) = value.downcast_ref::<CFArray>() {
        // SAFETY: every element of a CFArray is a CF object, which is all
        // `CFType` asserts.
        let items: &CFArray<CFType> = unsafe { array.cast_unchecked() };
        let elements: Option<Vec<AxNode>> = items
            .to_vec()
            .into_iter()
            .map(|item| item.downcast::<AXUIElement>().ok().map(AxNode))
            .collect();
        return elements.map_or_else(|| AxValue::Other(description(&value)), AxValue::Elements);
    }
    AxValue::Other(description(&value))
}

fn checked(error: AXError) -> Result<(), RawAxError> {
    if error == AXError::Success {
        Ok(())
    } else {
        Err(RawAxError(error.0))
    }
}

/// The live accessibility API.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct SysAx;

impl Ax for SysAx {
    type Node = AxNode;

    fn application(&self, pid: i64) -> Option<AxNode> {
        let pid = i32::try_from(pid).ok()?;
        // SAFETY: AXUIElementCreateApplication accepts any pid; an element
        // for a pid that is not running fails its reads instead.
        Some(AxNode(unsafe { AXUIElement::new_application(pid) }))
    }

    fn set_timeout(&self, node: &AxNode, timeout: Duration) {
        // SAFETY: a plain call on a live element reference.
        let _ = unsafe { node.0.set_messaging_timeout(timeout.as_secs_f32()) };
    }

    fn copy(&self, node: &AxNode, attribute: &str) -> Result<AxValue<AxNode>, RawAxError> {
        let attribute = CFString::from_str(attribute);
        let mut value: *const CFType = std::ptr::null();
        // SAFETY: `value` is a valid out-pointer for the whole call; on
        // success AX stores a +1 CF reference (or NULL) in it.
        checked(unsafe {
            node.0
                .copy_attribute_value(&attribute, NonNull::from(&mut value))
        })?;
        let Some(value) = NonNull::new(value.cast_mut()) else {
            return Ok(AxValue::Null);
        };
        // SAFETY: the Copy call handed us ownership of this +1 reference.
        Ok(decode(unsafe { CFRetained::from_raw(value) }))
    }

    fn action_names(&self, node: &AxNode) -> Result<Vec<String>, RawAxError> {
        let mut names: *const CFArray = std::ptr::null();
        // SAFETY: `names` is a valid out-pointer; success stores a +1 CFArray (or NULL).
        checked(unsafe { node.0.copy_action_names(NonNull::from(&mut names)) })?;
        let Some(names) = NonNull::new(names.cast_mut()) else {
            return Ok(Vec::new());
        };
        // SAFETY: the Copy call handed us ownership of this +1 reference.
        let names = unsafe { CFRetained::from_raw(names) };
        // SAFETY: CF elements are CF objects; non-strings are skipped below.
        let items: &CFArray<CFType> = unsafe { names.cast_unchecked() };
        Ok(items
            .to_vec()
            .into_iter()
            .map(|item| match item.downcast_ref::<CFString>() {
                Some(name) => name.to_string(),
                None => description(&item),
            })
            .collect())
    }

    fn perform(&self, node: &AxNode, action: &str) -> Result<(), RawAxError> {
        // SAFETY: a plain call on a live element with a live CFString.
        checked(unsafe { node.0.perform_action(&CFString::from_str(action)) })
    }

    fn is_settable(&self, node: &AxNode, attribute: &str) -> Result<bool, RawAxError> {
        // `Boolean` (an unsigned char) is private in objc2-core-foundation.
        let mut settable: u8 = 0;
        // SAFETY: `settable` is a valid out-pointer for the whole call.
        checked(unsafe {
            node.0
                .is_attribute_settable(&CFString::from_str(attribute), NonNull::from(&mut settable))
        })?;
        Ok(settable != 0)
    }

    fn set_string(&self, node: &AxNode, attribute: &str, value: &str) -> Result<(), RawAxError> {
        let value = CFString::from_str(value);
        // SAFETY: a plain call with a live element and live CF arguments.
        checked(unsafe {
            node.0
                .set_attribute_value(&CFString::from_str(attribute), &value)
        })
    }

    fn set_range(
        &self,
        node: &AxNode,
        attribute: &str,
        location: usize,
        length: usize,
    ) -> Result<(), RawAxError> {
        let range = CFRange::new(
            CFIndex::try_from(location).map_err(|_| AX_FAILURE)?,
            CFIndex::try_from(length).map_err(|_| AX_FAILURE)?,
        );
        // SAFETY: `range` is a live CFRange, the structure AXValueCreate
        // reads for kAXValueTypeCFRange.
        let value = unsafe { CfAxValue::new(AXValueType::CFRange, NonNull::from(&range).cast()) }
            .ok_or(AX_FAILURE)?;
        // SAFETY: a plain call with a live element and live CF arguments.
        checked(unsafe {
            node.0
                .set_attribute_value(&CFString::from_str(attribute), &value)
        })
    }

    fn identical(&self, left: &AxNode, right: &AxNode) -> bool {
        std::ptr::eq::<AXUIElement>(&raw const *left.0, &raw const *right.0)
    }

    fn equal(&self, left: &AxNode, right: &AxNode) -> bool {
        *left.0 == *right.0
    }
}

/// CF truthiness of a session-dictionary flag.
fn truthy(value: &CFType) -> bool {
    if let Some(flag) = value.downcast_ref::<CFBoolean>() {
        return flag.as_bool();
    }
    if let Some(number) = value.downcast_ref::<CFNumber>() {
        return number.as_f64().is_some_and(|value| value != 0.0);
    }
    true
}

fn number(dictionary: &CFDictionary<CFString, CFType>, key: &CFString) -> Option<f64> {
    dictionary
        .get(key)
        .and_then(|value| value.downcast_ref::<CFNumber>().and_then(CFNumber::as_f64))
}

fn general_pasteboard() -> Retained<NSPasteboard> {
    NSPasteboard::generalPasteboard()
}

fn string_type() -> &'static NSString {
    // SAFETY: an AppKit constant, initialized when AppKit loads.
    unsafe { NSPasteboardTypeString }
}

/// `AppKit`, `CoreGraphics`, the pasteboard and Vision.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct SysDesktop;

impl Desktop for SysDesktop {
    fn running_applications(&self) -> Vec<WorkspaceApp> {
        NSWorkspace::sharedWorkspace()
            .runningApplications()
            .iter()
            .map(|app| WorkspaceApp {
                bundle_id: app.bundleIdentifier().map(|id| id.to_string()),
                name: app.localizedName().map(|name| name.to_string()),
                pid: i64::from(app.processIdentifier()),
                path: app
                    .bundleURL()
                    .and_then(|url| url.path())
                    .map(|path| path.to_string()),
                regular: app.activationPolicy() == NSApplicationActivationPolicy::Regular,
            })
            .collect()
    }

    fn frontmost_pid(&self) -> Option<i64> {
        NSWorkspace::sharedWorkspace()
            .frontmostApplication()
            .map(|app| i64::from(app.processIdentifier()))
    }

    fn activate(&self, pid: i64) -> bool {
        let Some(app) = NSWorkspace::sharedWorkspace()
            .runningApplications()
            .iter()
            .find(|app| i64::from(app.processIdentifier()) == pid)
        else {
            return false;
        };
        // Deprecated in macOS 14 in favour of cooperative activation, which
        // cannot bring a background app forward from another process; the
        // skill has always used this call.
        #[allow(deprecated)]
        let _ = app.activateWithOptions(NSApplicationActivationOptions::ActivateIgnoringOtherApps);
        true
    }

    fn bundle_identifier(&self, bundle_dir: &Path) -> Option<String> {
        let path = NSString::from_str(&bundle_dir.to_string_lossy());
        NSBundle::bundleWithPath(&path)?
            .bundleIdentifier()
            .map(|id| id.to_string())
            .filter(|id| !id.is_empty())
    }

    fn session_locked(&self) -> Option<bool> {
        let session = CGSessionCopyCurrentDictionary()?;
        // SAFETY: the session dictionary maps CFString keys to CF values.
        let session: &CFDictionary<CFString, CFType> = unsafe { session.cast_unchecked() };
        Some(
            session
                .get(&CFString::from_str("CGSSessionScreenIsLocked"))
                .is_some_and(|flag| truthy(&flag)),
        )
    }

    fn accessibility_trusted(&self) -> Option<bool> {
        // SAFETY: an HIServices constant, initialized when the framework loads.
        let prompt = unsafe { kAXTrustedCheckOptionPrompt };
        let options =
            CFDictionary::<CFString, CFBoolean>::from_slices(&[prompt], &[CFBoolean::new(false)]);
        // SAFETY: the options dictionary has the documented CFString key and
        // CFBoolean value; with prompting off the call only reads the grant.
        Some(unsafe { AXIsProcessTrustedWithOptions(Some(options.as_opaque())) })
    }

    fn screen_capture_allowed(&self) -> Option<bool> {
        Some(CGPreflightScreenCaptureAccess())
    }

    fn on_screen_windows(&self) -> Option<Vec<(i64, Rect)>> {
        let options =
            CGWindowListOption::OptionOnScreenOnly | CGWindowListOption::ExcludeDesktopElements;
        let list = CGWindowListCopyWindowInfo(options, 0)?;
        // SAFETY: the window list is an array of CFString-keyed dictionaries.
        let list: &CFArray<CFDictionary<CFString, CFType>> = unsafe { list.cast_unchecked() };
        // SAFETY: CoreGraphics constants, initialized when the framework loads.
        let (number_key, bounds_key) = unsafe { (kCGWindowNumber, kCGWindowBounds) };
        let keys = ["X", "Y", "Width", "Height"].map(CFString::from_str);
        Some(
            list.to_vec()
                .into_iter()
                .filter_map(|info| {
                    #[allow(clippy::cast_possible_truncation)] // window numbers are u32
                    let id = number(&info, number_key)? as i64;
                    let bounds = info.get(bounds_key)?.downcast::<CFDictionary>().ok()?;
                    // SAFETY: a window's bounds dictionary is CFString-keyed.
                    let bounds: &CFDictionary<CFString, CFType> =
                        unsafe { bounds.cast_unchecked() };
                    let [x, y, width, height] = keys
                        .each_ref()
                        .map(|key| number(bounds, key).unwrap_or(0.0));
                    Some((id, Rect::new(x, y, width, height)))
                })
                .collect(),
        )
    }

    fn post(&self, pid: i64, events: &[MacEvent]) -> Result<(), PostError> {
        let pid =
            i32::try_from(pid).map_err(|_| PostError(format!("pid {pid} is out of range")))?;
        for event in events {
            let built = build(event)
                .ok_or_else(|| PostError("CoreGraphics could not create the event".to_string()))?;
            CGEvent::post_to_pid(pid, Some(&built));
        }
        Ok(())
    }
}

fn flags(modifiers: &std::collections::BTreeSet<Modifier>) -> CGEventFlags {
    modifiers
        .iter()
        .fold(CGEventFlags::empty(), |flags, modifier| {
            flags
                | match modifier {
                    Modifier::Cmd => CGEventFlags::MaskCommand,
                    Modifier::Ctrl => CGEventFlags::MaskControl,
                    Modifier::Alt => CGEventFlags::MaskAlternate,
                    Modifier::Shift => CGEventFlags::MaskShift,
                }
        })
}

fn build(event: &MacEvent) -> Option<CFRetained<CGEvent>> {
    match event {
        MacEvent::Mouse {
            kind,
            button,
            point,
        } => {
            let (kind, button) = match (button, kind) {
                (MouseButton::Left, MouseKind::Down) => {
                    (CGEventType::LeftMouseDown, CGMouseButton::Left)
                }
                (MouseButton::Left, MouseKind::Up) => {
                    (CGEventType::LeftMouseUp, CGMouseButton::Left)
                }
                (_, MouseKind::Dragged) => (CGEventType::LeftMouseDragged, CGMouseButton::Left),
                (MouseButton::Right, MouseKind::Down) => {
                    (CGEventType::RightMouseDown, CGMouseButton::Right)
                }
                (MouseButton::Right, MouseKind::Up) => {
                    (CGEventType::RightMouseUp, CGMouseButton::Right)
                }
                (MouseButton::Middle, MouseKind::Down) => {
                    (CGEventType::OtherMouseDown, CGMouseButton::Center)
                }
                (MouseButton::Middle, MouseKind::Up) => {
                    (CGEventType::OtherMouseUp, CGMouseButton::Center)
                }
            };
            CGEvent::new_mouse_event(
                None,
                kind,
                CGPoint {
                    x: point.0,
                    y: point.1,
                },
                button,
            )
        }
        MacEvent::Scroll {
            vertical,
            horizontal,
            location,
        } => {
            let event = CGEvent::new_scroll_wheel_event2(
                None,
                CGScrollEventUnit::Pixel,
                2,
                *vertical,
                *horizontal,
                0,
            )?;
            CGEvent::set_location(
                Some(&event),
                CGPoint {
                    x: location.0,
                    y: location.1,
                },
            );
            Some(event)
        }
        MacEvent::Key {
            keycode,
            down,
            modifiers,
        } => {
            let event = CGEvent::new_keyboard_event(None, *keycode, *down)?;
            CGEvent::set_flags(Some(&event), flags(modifiers));
            Some(event)
        }
        MacEvent::Unicode { down, units } => {
            let event = CGEvent::new_keyboard_event(None, 0, *down)?;
            // SAFETY: `units` is a live buffer of exactly `units.len()` UTF-16 units.
            unsafe {
                CGEvent::keyboard_set_unicode_string(
                    Some(&event),
                    units.len() as _,
                    units.as_ptr(),
                );
            }
            Some(event)
        }
    }
}

impl Clipboard for SysDesktop {
    fn save(&self) -> Option<ClipboardSnapshot> {
        let pasteboard = general_pasteboard();
        let types = pasteboard
            .types()
            .map(|types| types.to_vec())
            .unwrap_or_default();
        Some(
            types
                .into_iter()
                .filter_map(|kind| {
                    Some((kind.to_string(), pasteboard.dataForType(&kind)?.to_vec()))
                })
                .collect(),
        )
    }

    fn write(&self, text: &str, format: PasteFormat) -> Result<(), String> {
        let pasteboard = general_pasteboard();
        pasteboard.clearContents();
        if format == PasteFormat::Html {
            // SAFETY: an AppKit constant, initialized when AppKit loads.
            let html = unsafe { NSPasteboardTypeHTML };
            pasteboard.setData_forType(Some(&NSData::with_bytes(text.as_bytes())), html);
        }
        pasteboard.setString_forType(&NSString::from_str(text), string_type());
        Ok(())
    }

    fn holds(&self, text: &str) -> bool {
        general_pasteboard()
            .dataForType(string_type())
            .is_some_and(|data| data.to_vec() == text.as_bytes())
    }

    fn change_count(&self) -> Option<i64> {
        i64::try_from(general_pasteboard().changeCount()).ok()
    }

    fn restore(&self, snapshot: &ClipboardSnapshot) {
        let pasteboard = general_pasteboard();
        pasteboard.clearContents();
        for (kind, data) in snapshot {
            pasteboard.setData_forType(Some(&NSData::with_bytes(data)), &NSString::from_str(kind));
        }
    }
}

impl TextRecognizer for SysDesktop {
    fn recognize(&self, path: &str) -> Result<Vec<RecognizedText>, String> {
        let url = NSURL::fileURLWithPath(&NSString::from_str(path));
        // SAFETY: plain `init` of a freshly allocated request.
        let request = unsafe { VNRecognizeTextRequest::init(VNRecognizeTextRequest::alloc()) };
        let options = NSDictionary::new();
        // SAFETY: an empty options dictionary is of the documented key and value types.
        let handler = unsafe {
            VNImageRequestHandler::initWithURL_options(
                VNImageRequestHandler::alloc(),
                &url,
                &options,
            )
        };
        let as_request: Retained<VNRequest> =
            Retained::into_super(Retained::into_super(request.clone()));
        handler
            .performRequests_error(&NSArray::from_retained_slice(&[as_request]))
            .map_err(|error| error.localizedDescription().to_string())?;
        let observations = request
            .results()
            .map(|results| results.to_vec())
            .unwrap_or_default();
        Ok(observations
            .into_iter()
            .filter_map(|observation| {
                let candidate = observation.topCandidates(1).firstObject()?;
                // SAFETY: a plain property read on a live observation.
                let rect = unsafe { observation.boundingBox() };
                Some(RecognizedText {
                    text: candidate.string().to_string(),
                    confidence: f64::from(candidate.confidence()),
                    bbox: (
                        rect.origin.x,
                        rect.origin.y,
                        rect.size.width,
                        rect.size.height,
                    ),
                })
            })
            .collect())
    }
}
