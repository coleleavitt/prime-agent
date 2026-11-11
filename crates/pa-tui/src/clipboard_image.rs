//! Reading an image from the system clipboard for editor paste: prefer a supported
//! image type (PNG, JPEG, GIF, WebP) and hand back the payload plus its mime type.
//! Every reader is best-effort: a missing tool, an empty clipboard, or an unsupported
//! type returns [`ClipboardOutcome::None`] (dropped, never sent). An image past the
//! Anthropic single-image hard cap (CC `K` = 8000 px on either side,
//! `image_limits::DEFAULT_LIMITS.reject_max_*`) is surfaced as
//! [`ClipboardOutcome::Refused`] carrying the user-facing reason, so the paste
//! handler can show it instead of silently dropping.

use std::time::Duration;

use tokio::process::Command;

use crate::image_limits;
use crate::image_load::{LoadedImage, SUPPORTED_IMAGE_MIME_TYPES};

const DEFAULT_LIST_TIMEOUT: Duration = Duration::from_secs(1);
const DEFAULT_READ_TIMEOUT: Duration = Duration::from_secs(3);
const DEFAULT_POWERSHELL_TIMEOUT: Duration = Duration::from_secs(5);
const DEFAULT_MAX_BUFFER_BYTES: usize = 50 * 1024 * 1024;

/// One clipboard image: the base64 payload plus its sniffed mime type.
pub type ClipboardImage = LoadedImage;

/// Outcome of one clipboard read attempt (or of the orchestrating
/// [`read_clipboard_image`]). CC's `ust` path has the same three cases:
/// image accepted, nothing readable, or decoded-but-refused because the
/// image is past the hard cap.
#[derive(Debug, Clone)]
pub enum ClipboardOutcome {
    /// A supported image was read and downscaled (if needed) under the
    /// Anthropic many-image cap.
    Image(ClipboardImage),
    /// The clipboard offered nothing usable, no tool answered, or the
    /// bytes did not decode as one of the supported formats.
    None,
    /// The image decoded but is past the hard-reject cap. Field is the
    /// user-facing reason — show it as a paste refusal note.
    Refused(String),
}

impl ClipboardOutcome {
    #[must_use]
    fn is_none(&self) -> bool {
        matches!(self, Self::None)
    }
}

/// Strip parameters from a mime type (`image/png; charset=...`).
fn base_mime_type(mime_type: &str) -> String {
    mime_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_lowercase()
}

/// The supported image type among `types`, in the `SUPPORTED_IMAGE_MIME_TYPES` order;
/// any other `image/*` type loses to `None` (it cannot be sent without a converter).
fn select_supported_image_mime_type(types: &[String]) -> Option<String> {
    let normalized: Vec<String> = types
        .iter()
        .map(|t| t.trim())
        .filter(|t| !t.is_empty())
        .map(base_mime_type)
        .collect();
    for preferred in SUPPORTED_IMAGE_MIME_TYPES {
        if let Some(found) = normalized.iter().find(|t| t.as_str() == preferred) {
            return Some(found.clone());
        }
    }
    None
}

/// Run one command with a hard timeout; `None` on spawn failure, nonzero
/// exit, or timeout.
async fn run_command(program: &str, args: &[&str], timeout: Duration) -> Option<Vec<u8>> {
    let child = Command::new(program)
        .args(args)
        // Without the drop-kill, a hung converter keeps running past the
        // deadline — an orphaned direct child of the TUI.
        .kill_on_drop(true)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    let Ok(Ok(output)) = tokio::time::timeout(timeout, child.wait_with_output()).await else {
        return None;
    };
    if !output.status.success() {
        return None;
    }
    if output.stdout.len() > DEFAULT_MAX_BUFFER_BYTES {
        return None;
    }
    Some(output.stdout)
}

/// Whether this is a WSL session.
fn is_wsl() -> bool {
    if std::env::var_os("WSL_DISTRO_NAME").is_some() || std::env::var_os("WSLENV").is_some() {
        return true;
    }
    std::fs::read_to_string("/proc/version").is_ok_and(|version| {
        version.contains("microsoft") || version.contains("Microsoft") || version.contains("WSL")
    })
}

/// Whether this is a Wayland session.
fn is_wayland_session() -> bool {
    std::env::var_os("WAYLAND_DISPLAY").is_some()
        || std::env::var("XDG_SESSION_TYPE").as_deref() == Ok("wayland")
}

/// Split a command's textual stdout into trimmed lines.
fn stdout_lines(stdout: &[u8]) -> Vec<String> {
    String::from_utf8_lossy(stdout)
        .lines()
        .map(|line| line.trim().to_string())
        .filter(|line| !line.is_empty())
        .collect()
}

/// `wl-paste` (Wayland): list the offered types, pick the preferred
/// supported image type, read it without a trailing newline.
async fn read_via_wl_paste() -> ClipboardOutcome {
    let Some(types) = run_command("wl-paste", &["--list-types"], DEFAULT_LIST_TIMEOUT).await else {
        return ClipboardOutcome::None;
    };
    let types = stdout_lines(&types);
    let Some(selected) = select_supported_image_mime_type(&types) else {
        return ClipboardOutcome::None;
    };
    let Some(bytes) = run_command(
        "wl-paste",
        &["--type", &selected, "--no-newline"],
        DEFAULT_READ_TIMEOUT,
    )
    .await
    else {
        return ClipboardOutcome::None;
    };
    clipboard_image_from_bytes(bytes)
}

/// `xclip` (X11): enumerate `TARGETS`, then read the preferred supported
/// image type (trying each in order when no target matched). The first
/// supported type whose bytes decode wins; a decoded-but-refused image
/// short-circuits to [`ClipboardOutcome::Refused`] so later candidates do
/// not mask a real user-visible refusal with a generic "nothing to paste".
async fn read_via_xclip() -> ClipboardOutcome {
    let targets = run_command(
        "xclip",
        &["-selection", "clipboard", "-t", "TARGETS", "-o"],
        DEFAULT_LIST_TIMEOUT,
    )
    .await
    .map(|stdout| stdout_lines(&stdout))
    .unwrap_or_default();
    let mut candidates: Vec<String> = Vec::new();
    if let Some(preferred) = select_supported_image_mime_type(&targets) {
        candidates.push(preferred);
    }
    candidates.extend(SUPPORTED_IMAGE_MIME_TYPES.iter().map(ToString::to_string));
    for mime_type in candidates {
        let Some(bytes) = run_command(
            "xclip",
            &["-selection", "clipboard", "-t", &mime_type, "-o"],
            DEFAULT_READ_TIMEOUT,
        )
        .await
        .filter(|bytes| !bytes.is_empty()) else {
            continue;
        };
        let outcome = clipboard_image_from_bytes(bytes);
        if !outcome.is_none() {
            return outcome;
        }
    }
    ClipboardOutcome::None
}

/// Validate clipboard bytes by content (the clipboard-reported type may be wrong and
/// there is no converter to fall back on — content sniffing is this port's equivalent of
/// TS's Photon conversion), then funnel through [`image_limits::enforce`] so the
/// payload is under the Anthropic many-image cap before it reaches the API.
fn clipboard_image_from_bytes(bytes: Vec<u8>) -> ClipboardOutcome {
    use base64::Engine;
    if bytes.is_empty() {
        return ClipboardOutcome::None;
    }
    let Some(mime_type) = crate::image_load::detect_supported_image_mime_from_bytes(&bytes) else {
        return ClipboardOutcome::None;
    };
    match image_limits::enforce(&bytes, mime_type, &image_limits::DEFAULT_LIMITS) {
        Ok(enforced) => ClipboardOutcome::Image(ClipboardImage {
            data: base64::engine::general_purpose::STANDARD.encode(&enforced.data),
            mime_type: enforced.mime_type.to_string(),
        }),
        Err(image_limits::ImageError::Decode(_)) => ClipboardOutcome::None,
        Err(error) => ClipboardOutcome::Refused(error.to_string()),
    }
}

/// WSL: PowerShell reads the Windows clipboard and saves it as a PNG at a
/// temp path, which WSL reads back and removes.
async fn read_via_powershell() -> ClipboardOutcome {
    let temp_dir = std::env::temp_dir();
    if std::fs::create_dir_all(&temp_dir).is_err() {
        return ClipboardOutcome::None;
    }
    let temp_path = temp_dir.join(format!("prime-agent-clip-{}.png", unique_suffix()));
    let Some(temp_path_str) = temp_path.to_str() else {
        return ClipboardOutcome::None;
    };
    let Some(wsl_output) =
        run_command("wslpath", &["-w", temp_path_str], DEFAULT_LIST_TIMEOUT).await
    else {
        return ClipboardOutcome::None;
    };
    let windows_path = String::from_utf8_lossy(&wsl_output).trim().to_string();
    if windows_path.is_empty() {
        return ClipboardOutcome::None;
    }
    let escaped_path = windows_path.replace('\'', "''");
    let script = [
        "Add-Type -AssemblyName System.Windows.Forms",
        "Add-Type -AssemblyName System.Drawing",
        format!("$path = '{escaped_path}'").as_str(),
        "$img = [System.Windows.Forms.Clipboard]::GetImage()",
        "if ($img) { $img.Save($path, [System.Drawing.Imaging.ImageFormat]::Png); Write-Output 'ok' } else { Write-Output 'empty' }",
    ]
    .join("; ");
    let Some(output) = run_command(
        "powershell.exe",
        &["-NoProfile", "-Command", &script],
        DEFAULT_POWERSHELL_TIMEOUT,
    )
    .await
    else {
        return ClipboardOutcome::None;
    };
    if String::from_utf8_lossy(&output).trim() != "ok" {
        return ClipboardOutcome::None;
    }
    let outcome = load_image_as_outcome(&temp_path);
    let _ = std::fs::remove_file(&temp_path);
    outcome
}

/// Translate a `load_image_from_path` result into a [`ClipboardOutcome`]:
/// `InvalidData` means the file-path loader rejected the image past the
/// hard cap (CC `ust`'s `rejectMax*` wall), so surface the reason; every
/// other IO failure is indistinguishable from "no readable image" for the
/// caller and collapses to [`ClipboardOutcome::None`].
fn load_image_as_outcome(path: &std::path::Path) -> ClipboardOutcome {
    match crate::image_load::load_image_from_path(path) {
        Ok(Some(image)) => ClipboardOutcome::Image(image),
        Err(error) if error.kind() == std::io::ErrorKind::InvalidData => {
            ClipboardOutcome::Refused(error.to_string())
        }
        Ok(None) | Err(_) => ClipboardOutcome::None,
    }
}

/// A unique temp-file suffix: the per-process random hasher seeded by the
/// OS plus the current nanosecond clock.
fn unique_suffix() -> String {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    let mut hasher = RandomState::new().build_hasher();
    hasher.write_u64(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos() as u64),
    );
    hasher.write_u32(std::process::id());
    format!("{:016x}", hasher.finish())
}

/// macOS: a JavaScript-for-Automation script reads the pasteboard's PNG data and
/// writes it to a temp file, which we read back and remove. Non-PNG flavors are not read.
async fn read_via_osascript() -> ClipboardOutcome {
    let temp_dir = std::env::temp_dir();
    if std::fs::create_dir_all(&temp_dir).is_err() {
        return ClipboardOutcome::None;
    }
    let temp_path = temp_dir.join(format!("prime-agent-clip-{}.png", unique_suffix()));
    let Some(path_str) = temp_path.to_str().map(str::to_string) else {
        return ClipboardOutcome::None;
    };
    let script = format!(
        r"ObjC.import('Cocoa');
const pb = $.NSPasteboard.generalPasteboard;
const types = pb.types.allObjects;
if (!types.containsObject($.NSPasteboardTypePNG)) {{
    'empty'
}} else {{
    const data = pb.dataForType($.NSPasteboardTypePNG);
    const text = $.NSString.alloc.initWithDataEncoding(data, $.NSISOLatin1StringEncoding);
    text.writeToFileAtomicallyEncodingError('{path_str}', false, $.NSISOLatin1StringEncoding, null);
    'ok'
}}"
    );
    let Some(output) = run_command(
        "osascript",
        &["-l", "JavaScript", "-e", &script],
        DEFAULT_READ_TIMEOUT,
    )
    .await
    else {
        return ClipboardOutcome::None;
    };
    if String::from_utf8_lossy(&output).trim() != "ok" {
        return ClipboardOutcome::None;
    }
    let outcome = load_image_as_outcome(&temp_path);
    let _ = std::fs::remove_file(&temp_path);
    outcome
}

/// Read an image from the system clipboard, if one of the supported types is present.
/// Wayland tries `wl-paste` then `xclip`; WSL additionally falls through to PowerShell;
/// X11 uses `xclip`; macOS reads through osascript. Termux exposes no clipboard image path.
pub async fn read_clipboard_image() -> ClipboardOutcome {
    // Verification seam (never set by the product): a harness without a display server
    // cannot drive the real clipboard readers, so a fixture file stands in for the
    // clipboard and travels the exact paste path from there.
    if let Some(path) = std::env::var_os("PRIME_AGENT_TEST_CLIPBOARD_IMAGE") {
        return match std::fs::read(path) {
            Ok(bytes) => clipboard_image_from_bytes(bytes),
            Err(_) => ClipboardOutcome::None,
        };
    }
    if std::env::var_os("TERMUX_VERSION").is_some() {
        return ClipboardOutcome::None;
    }
    match std::env::consts::OS {
        "linux" => {
            let wsl = is_wsl();
            let wayland = is_wayland_session();
            let mut outcome = ClipboardOutcome::None;
            if wayland || wsl {
                outcome = read_via_wl_paste().await;
                if outcome.is_none() {
                    outcome = read_via_xclip().await;
                }
            }
            if outcome.is_none() && wsl {
                outcome = read_via_powershell().await;
            }
            if outcome.is_none() && !wayland {
                outcome = read_via_xclip().await;
            }
            outcome
        }
        "macos" => read_via_osascript().await,
        _ => ClipboardOutcome::None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_mime_strips_parameters() {
        assert_eq!(base_mime_type(" image/PNG; charset=utf8 "), "image/png");
    }

    #[test]
    fn supported_type_selection_prefers_png_over_other_images() {
        let types = vec!["text/plain".to_string(), "image/webp".to_string()];
        assert_eq!(
            select_supported_image_mime_type(&types),
            Some("image/webp".to_string())
        );
        let mixed = vec![
            "image/webp".to_string(),
            "image/png".to_string(),
            "image/jpeg".to_string(),
        ];
        assert_eq!(
            select_supported_image_mime_type(&mixed),
            Some("image/png".to_string())
        );
        // An unsupported image type is never selected.
        assert_eq!(
            select_supported_image_mime_type(&["image/bmp".to_string()]),
            None
        );
        assert_eq!(
            select_supported_image_mime_type(&["text/plain".to_string()]),
            None
        );
        assert_eq!(select_supported_image_mime_type(&[]), None);
    }

    #[test]
    fn stdout_lines_trim_and_drop_empty() {
        let stdout = b"image/png\r\ntext/plain\n\n\r\n";
        assert_eq!(stdout_lines(stdout), vec!["image/png", "text/plain"]);
    }

    #[test]
    fn unique_suffix_is_unique() {
        let a = unique_suffix();
        let b = unique_suffix();
        assert_ne!(a, b);
    }
}
