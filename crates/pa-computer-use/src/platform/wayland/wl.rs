//! The real virtual input: `zwlr_virtual_pointer_v1` and
//! `zwp_virtual_keyboard_v1` over wayland-client.
//!
//! Every session is one short-lived connection: bind, send, then a
//! `wl_display.sync` round trip so the compositor processed every request
//! before the connection closes. niri offers both managers to every client
//! outside a security-context sandbox, so no uinput device, root or daemon
//! is involved. Pointer motion is pixel-exact in logical space: a pointer
//! created for one output maps `motion_absolute` onto that output.

use std::collections::HashMap;
use std::io::{Seek, Write};
use std::os::fd::AsFd;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

use serde_json::json;
use wayland_client::backend::{ObjectId, WaylandError};
use wayland_client::globals::{GlobalList, GlobalListContents, registry_queue_init};
use wayland_client::protocol::{wl_output, wl_pointer, wl_registry, wl_seat};
use wayland_client::{
    Connection,
    Dispatch,
    DispatchError,
    EventQueue,
    Proxy,
    QueueHandle,
    delegate_noop,
};
use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1;
use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1;
use wayland_protocols_wlr::virtual_pointer::v1::client::zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1;
use wayland_protocols_wlr::virtual_pointer::v1::client::zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1;

use super::input::{
    Availability,
    FIRST_KEYCODE,
    KeyStroke,
    PointerTarget,
    VirtualInput,
    groups,
    keymap_text,
};
use crate::element::Pair;
use crate::error::{ComputerUseError, ERROR_LIMIT, Result, head, injection_failed, unsupported};
use crate::platform::{MouseButton, ScrollDirection};
use crate::pyfmt::repr_str;

const POINTER_MANAGER: &str = "zwlr_virtual_pointer_manager_v1";
const KEYBOARD_MANAGER: &str = "zwp_virtual_keyboard_manager_v1";
const TIMEOUT: Duration = Duration::from_secs(5);
const WHEEL_STEP: f64 = 15.0;
const KEYMAP_FORMAT_XKB_V1: u32 = 1;

/// Dispatch state: the names of the bound outputs.
#[derive(Default)]
struct State {
    output_names: HashMap<ObjectId, String>,
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
    fn event(
        _state: &mut Self,
        _registry: &wl_registry::WlRegistry,
        _event: wl_registry::Event,
        _data: &GlobalListContents,
        _connection: &Connection,
        _queue: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_output::WlOutput, ()> for State {
    fn event(
        state: &mut Self,
        output: &wl_output::WlOutput,
        event: wl_output::Event,
        _data: &(),
        _connection: &Connection,
        _queue: &QueueHandle<Self>,
    ) {
        if let wl_output::Event::Name { name } = event {
            state.output_names.insert(output.id(), name);
        }
    }
}

delegate_noop!(State: ignore wl_seat::WlSeat);
delegate_noop!(State: ZwlrVirtualPointerManagerV1);
delegate_noop!(State: ZwlrVirtualPointerV1);
delegate_noop!(State: ZwpVirtualKeyboardManagerV1);
delegate_noop!(State: ZwpVirtualKeyboardV1);

fn wayland_failed(error: &WaylandError) -> ComputerUseError {
    match error {
        WaylandError::Protocol(protocol) => injection_failed(format!(
            "the compositor rejected a Wayland request (object {}, code {}): {}",
            protocol.object_id,
            protocol.code,
            head(&protocol.message, ERROR_LIMIT)
        )),
        WaylandError::Io(io)
            if io.kind() == std::io::ErrorKind::BrokenPipe
                || io.kind() == std::io::ErrorKind::UnexpectedEof =>
        {
            injection_failed("the compositor closed the Wayland connection")
        }
        WaylandError::Io(io) => injection_failed(format!(
            "the Wayland connection failed: {}",
            head(&io.to_string(), ERROR_LIMIT)
        )),
    }
}

fn dispatch_failed(error: &DispatchError) -> ComputerUseError {
    match error {
        DispatchError::Backend(backend) => wayland_failed(backend),
        DispatchError::BadMessage { .. } => {
            injection_failed("the compositor sent a malformed Wayland event")
        }
    }
}

fn missing(interface: &str) -> ComputerUseError {
    unsupported(format!(
        "the compositor does not offer {interface} to this client (a sandboxed security context \
         hides it); Wayland pointer and keyboard input are unavailable"
    ))
    .with_details(json!({"platform": "wayland", "interface": interface}))
}

/// One connection with its registry.
struct Session {
    queue: EventQueue<State>,
    handle: QueueHandle<State>,
    globals: GlobalList,
    state: State,
}

impl Session {
    fn open(stream: UnixStream) -> Result<Self> {
        let _ = stream.set_read_timeout(Some(TIMEOUT));
        let _ = stream.set_write_timeout(Some(TIMEOUT));
        let connection = Connection::from_socket(stream)
            .map_err(|error| injection_failed(format!("the Wayland connection failed: {error}")))?;
        let (globals, queue) =
            registry_queue_init::<State>(&connection).map_err(|error| match error {
                wayland_client::globals::GlobalError::Backend(backend) => wayland_failed(&backend),
                wayland_client::globals::GlobalError::InvalidId(_) => {
                    injection_failed("the compositor sent a malformed Wayland event")
                }
            })?;
        let handle = queue.handle();
        Ok(Self {
            queue,
            handle,
            globals,
            state: State::default(),
        })
    }

    fn advertised(&self, interface: &str) -> Option<u32> {
        self.globals.contents().with_list(|list| {
            list.iter()
                .find(|global| global.interface == interface)
                .map(|global| global.version)
        })
    }

    fn roundtrip(&mut self) -> Result<()> {
        self.queue
            .roundtrip(&mut self.state)
            .map(drop)
            .map_err(|error| dispatch_failed(&error))
    }

    fn seat(&self) -> Result<wl_seat::WlSeat> {
        self.globals
            .bind(&self.handle, 1..=1, ())
            .map_err(|_| missing("wl_seat"))
    }
}

/// The compositor socket from `WAYLAND_DISPLAY` and `XDG_RUNTIME_DIR`.
pub(crate) fn socket_path(display: Option<String>, runtime: Option<String>) -> Result<PathBuf> {
    let unavailable = |reason: &str| {
        unsupported(format!("Wayland input is unavailable: {reason}"))
            .with_details(json!({"platform": "wayland"}))
    };
    let display = display
        .filter(|display| !display.is_empty())
        .ok_or_else(|| unavailable("WAYLAND_DISPLAY is not set"))?;
    if display.starts_with('/') {
        return Ok(PathBuf::from(display));
    }
    let runtime = runtime
        .filter(|runtime| !runtime.is_empty())
        .ok_or_else(|| unavailable("XDG_RUNTIME_DIR is not set"))?;
    Ok(PathBuf::from(runtime).join(display))
}

/// A millisecond timestamp for input events (wrapping like the protocol's uint).
fn now_ms() -> u32 {
    use std::sync::OnceLock;
    static START: OnceLock<std::time::Instant> = OnceLock::new();
    #[allow(clippy::cast_possible_truncation)] // the protocol's uint wraps
    let millis = START
        .get_or_init(std::time::Instant::now)
        .elapsed()
        .as_millis() as u32;
    millis
}

/// The virtual-input client over a connector (one stream per session).
pub(crate) struct WireInput<C: Fn() -> Result<UnixStream> + Send + Sync> {
    connect: C,
    clock: fn() -> u32,
}

/// The host's virtual input: the session's compositor socket.
pub(crate) type WaylandInput = WireInput<fn() -> Result<UnixStream>>;

/// The virtual input of this process's Wayland session.
pub(crate) fn session_input() -> WaylandInput {
    WireInput {
        connect: connect_env,
        clock: now_ms,
    }
}

fn connect_env() -> Result<UnixStream> {
    let path = socket_path(
        std::env::var("WAYLAND_DISPLAY").ok(),
        std::env::var("XDG_RUNTIME_DIR").ok(),
    )?;
    UnixStream::connect(&path).map_err(|error| {
        unsupported(format!(
            "Wayland input is unavailable: cannot connect to the compositor socket ({})",
            head(&error.to_string(), ERROR_LIMIT)
        ))
        .with_details(json!({"platform": "wayland"}))
    })
}

/// One created virtual pointer.
struct Pointer<'a> {
    session: &'a mut Session,
    device: ZwlrVirtualPointerV1,
    target: PointerTarget,
    clock: fn() -> u32,
}

impl Pointer<'_> {
    fn moved(&self, (x, y): Pair) {
        let clamp = |value: f64, extent: i32| {
            #[allow(clippy::cast_possible_truncation)] // logical output sizes are far inside i64
            let rounded = value.round_ties_even() as i64;
            u32::try_from(rounded.clamp(0, i64::from(extent - 1).max(0))).unwrap_or(0)
        };
        let (width, height) = (
            u32::try_from(self.target.width).unwrap_or(0),
            u32::try_from(self.target.height).unwrap_or(0),
        );
        self.device.motion_absolute(
            (self.clock)(),
            clamp(x, self.target.width),
            clamp(y, self.target.height),
            width,
            height,
        );
        self.device.frame();
    }

    fn button(&self, code: u32, pressed: bool) {
        let state = if pressed {
            wl_pointer::ButtonState::Pressed
        } else {
            wl_pointer::ButtonState::Released
        };
        self.device.button((self.clock)(), code, state);
        self.device.frame();
    }

    fn wheel(&self, direction: ScrollDirection, clicks: u32) {
        let axis = match direction {
            ScrollDirection::Up | ScrollDirection::Down => wl_pointer::Axis::VerticalScroll,
            ScrollDirection::Left | ScrollDirection::Right => wl_pointer::Axis::HorizontalScroll,
        };
        let sign = match direction {
            ScrollDirection::Down | ScrollDirection::Right => 1,
            ScrollDirection::Up | ScrollDirection::Left => -1,
        };
        for _ in 0..clicks {
            self.device.axis_source(wl_pointer::AxisSource::Wheel);
            self.device
                .axis_discrete((self.clock)(), axis, f64::from(sign) * WHEEL_STEP, sign);
            self.device.frame();
        }
    }

    fn finish(self) -> Result<()> {
        self.device.destroy();
        self.session.roundtrip()
    }
}

fn button_code(button: MouseButton) -> u32 {
    match button {
        MouseButton::Left => 0x110,
        MouseButton::Right => 0x111,
        MouseButton::Middle => 0x112,
    }
}

impl<C: Fn() -> Result<UnixStream> + Send + Sync> WireInput<C> {
    #[cfg(test)]
    pub(crate) fn new(connect: C, clock: fn() -> u32) -> Self {
        Self { connect, clock }
    }

    fn session(&self) -> Result<Session> {
        Session::open((self.connect)()?)
    }

    /// Run `send` on one virtual pointer mapped onto the target's output.
    fn with_pointer(&self, target: &PointerTarget, send: impl FnOnce(&Pointer<'_>)) -> Result<()> {
        let mut session = self.session()?;
        let manager_version = session
            .advertised(POINTER_MANAGER)
            .ok_or_else(|| missing(POINTER_MANAGER))?;
        let seat = session.seat()?;
        let manager: ZwlrVirtualPointerManagerV1 = session
            .globals
            .bind(&session.handle, 1..=2, ())
            .map_err(|_| missing(POINTER_MANAGER))?;
        let device = match &target.output {
            Some(name) if manager_version >= 2 => {
                let outputs: Vec<u32> = session.globals.contents().with_list(|list| {
                    list.iter()
                        .filter(|global| global.interface == "wl_output" && global.version >= 4)
                        .map(|global| global.name)
                        .collect()
                });
                let bound: Vec<wl_output::WlOutput> = outputs
                    .into_iter()
                    .map(|global| {
                        session
                            .globals
                            .registry()
                            .bind(global, 4, &session.handle, ())
                    })
                    .collect();
                session.roundtrip()?;
                let output = bound
                    .into_iter()
                    .find(|output| session.state.output_names.get(&output.id()) == Some(name))
                    .ok_or_else(|| {
                        unsupported(format!(
                            "the compositor did not advertise output {}; cannot map the pointer onto it",
                            repr_str(name)
                        ))
                        .with_details(json!({"platform": "wayland"}))
                    })?;
                manager.create_virtual_pointer_with_output(
                    Some(&seat),
                    Some(&output),
                    &session.handle,
                    (),
                )
            }
            Some(_) => {
                return Err(unsupported(
                    "the compositor's virtual pointer manager predates per-output pointers (v2); \
                     coordinate input cannot be mapped exactly",
                )
                .with_details(json!({"platform": "wayland"})));
            }
            None => manager.create_virtual_pointer(Some(&seat), &session.handle, ()),
        };
        let pointer = Pointer {
            session: &mut session,
            device,
            target: target.clone(),
            clock: self.clock,
        };
        send(&pointer);
        pointer.finish()
    }
}

/// The keymap for one group, in a sealed memfd.
fn keymap_fd(text: &str) -> Result<std::os::fd::OwnedFd> {
    let failed = |error: &dyn std::fmt::Display| {
        injection_failed(format!("could not stage the keymap: {error}"))
    };
    let fd = rustix::fs::memfd_create("prime-agent-keymap", rustix::fs::MemfdFlags::CLOEXEC)
        .map_err(|error| failed(&error))?;
    let mut file = std::fs::File::from(fd);
    file.write_all(text.as_bytes())
        .map_err(|error| failed(&error))?;
    file.write_all(b"\0").map_err(|error| failed(&error))?;
    file.rewind().map_err(|error| failed(&error))?;
    Ok(std::os::fd::OwnedFd::from(file))
}

impl<C: Fn() -> Result<UnixStream> + Send + Sync> VirtualInput for WireInput<C> {
    fn available(&self) -> Result<Availability> {
        let session = self.session()?;
        let seat = session.advertised("wl_seat").is_some();
        Ok(Availability {
            pointer: seat && session.advertised(POINTER_MANAGER).is_some(),
            keyboard: seat && session.advertised(KEYBOARD_MANAGER).is_some(),
        })
    }

    fn click(
        &self,
        target: &PointerTarget,
        point: Pair,
        button: MouseButton,
        count: u32,
    ) -> Result<()> {
        let code = button_code(button);
        self.with_pointer(target, |pointer| {
            pointer.moved(point);
            for _ in 0..count {
                pointer.button(code, true);
                pointer.button(code, false);
            }
        })
    }

    fn drag(&self, target: &PointerTarget, start: Pair, end: Pair) -> Result<()> {
        const STEPS: u32 = 12;
        let code = button_code(MouseButton::Left);
        self.with_pointer(target, |pointer| {
            pointer.moved(start);
            pointer.button(code, true);
            for step in 1..=STEPS {
                let fraction = f64::from(step) / f64::from(STEPS);
                pointer.moved((
                    start.0 + (end.0 - start.0) * fraction,
                    start.1 + (end.1 - start.1) * fraction,
                ));
            }
            pointer.button(code, false);
        })
    }

    fn scroll(
        &self,
        target: &PointerTarget,
        point: Pair,
        direction: ScrollDirection,
        clicks: u32,
    ) -> Result<()> {
        self.with_pointer(target, |pointer| {
            pointer.moved(point);
            pointer.wheel(direction, clicks);
        })
    }

    fn send_keys(&self, strokes: &[KeyStroke]) -> Result<()> {
        if strokes.is_empty() {
            return Ok(());
        }
        let mut session = self.session()?;
        let manager: ZwpVirtualKeyboardManagerV1 = session
            .globals
            .bind(&session.handle, 1..=1, ())
            .map_err(|_| missing(KEYBOARD_MANAGER))?;
        let seat = session.seat()?;
        let keyboard = manager.create_virtual_keyboard(&seat, &session.handle, ());
        let mut delivered = Ok(());
        for group in groups(strokes) {
            let mut keysyms: Vec<String> = Vec::new();
            for stroke in group {
                if !keysyms.contains(&stroke.keysym) {
                    keysyms.push(stroke.keysym.clone());
                }
            }
            let text = keymap_text(&keysyms);
            let fd = keymap_fd(&text)?;
            let size = u32::try_from(text.len() + 1).unwrap_or(u32::MAX);
            keyboard.keymap(KEYMAP_FORMAT_XKB_V1, fd.as_fd(), size);
            drop(fd);
            for stroke in group {
                let index = keysyms
                    .iter()
                    .position(|keysym| *keysym == stroke.keysym)
                    .unwrap_or(0);
                let code = u32::try_from(index + FIRST_KEYCODE - 8).unwrap_or(0);
                if stroke.modifiers != 0 {
                    keyboard.modifiers(stroke.modifiers, 0, 0, 0);
                }
                keyboard.key((self.clock)(), code, 1);
                keyboard.key((self.clock)(), code, 0);
                if stroke.modifiers != 0 {
                    keyboard.modifiers(0, 0, 0, 0);
                }
            }
            delivered = session.roundtrip();
            if delivered.is_err() {
                break;
            }
        }
        keyboard.destroy();
        let closed = session.roundtrip();
        delivered.and(closed)
    }
}

#[cfg(test)]
mod tests;
