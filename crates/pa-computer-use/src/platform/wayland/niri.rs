//! niri's IPC: one JSON request line per connection on `NIRI_SOCKET`, one
//! `{"Ok": ...}` / `{"Err": ...}` reply line.
//!
//! The app identity is the Wayland `app_id`, the bound id is niri's window
//! id. The computer-use niri fork answers three more requests:
//! `WindowGeometry` (any window's rendered rect, tiled ones included, and
//! whether an animation is moving it), `CaptureWindow` (the window rendered
//! alone into a PNG) and `WindowAt` (the compositor's own input hit test).
//! Upstream niri refuses them as unparseable, so each has a fallback: niri
//! 26.04 exposes a window's on-screen position only for floating windows (a
//! tiled window's scrolling-view offset is not in its IPC), so the absolute
//! rect is derivable only for a floating window on an active workspace: the
//! output's logical origin, plus the tile position, plus the window's offset
//! in its tile.

use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use crate::element::Rect;
use crate::error::{ERROR_LIMIT, Result, head, transport};

/// Sends one request line and returns the reply bytes (faked in tests).
pub(crate) trait NiriTransport: Send + Sync {
    fn exchange(&self, line: &[u8]) -> Result<Vec<u8>>;
}

/// One niri window record (the IPC's `Windows` entries).
pub(crate) type WindowRecord = serde_json::Map<String, Value>;

/// The IPC client over one transport.
pub(crate) struct Niri<N: NiriTransport> {
    pub(crate) transport: N,
}

/// One window's derivable geometry, in logical pixels.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Geometry {
    /// The absolute origin, when niri exposes it.
    pub origin: Option<(f64, f64)>,
    pub width: f64,
    pub height: f64,
    pub output: Option<String>,
    pub output_rect: Option<Rect>,
    /// Why the origin is unknown (empty when known).
    pub reason: &'static str,
}

impl Geometry {
    pub(crate) fn rect(&self) -> Option<Rect> {
        self.origin
            .map(|(x, y)| Rect::new(x, y, self.width, self.height))
    }
}

fn float(value: Option<&Value>) -> f64 {
    value.and_then(Value::as_f64).unwrap_or(0.0)
}

fn pair(value: Option<&Value>) -> Option<(f64, f64)> {
    let items = value?.as_array()?;
    Some((float(items.first()), float(items.get(1))))
}

/// niri's whole `Err` text for a request line it cannot deserialize: the
/// IPC server reports only the context, not serde's "unknown variant". A
/// well-formed fork request gets it exactly when this niri predates the
/// request.
const UNKNOWN_REQUEST: &str = "error parsing request";

/// A rect in global logical pixels (niri-ipc's `LogicalRect`).
#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
pub(crate) struct LogicalRect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

impl From<LogicalRect> for Rect {
    fn from(rect: LogicalRect) -> Self {
        Rect::new(rect.x, rect.y, rect.width, rect.height)
    }
}

/// The fields of the fork's `WindowGeometry` reply the backend reads.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub(crate) struct WindowGeometry {
    pub output: Option<String>,
    /// Its workspace is the visible one on its output and the overview is
    /// closed.
    pub on_screen: bool,
    /// The rendered window-geometry rect, animations included.
    pub window_rect: Option<LogicalRect>,
    /// `window_rect` clipped to its output, `None` when fully off screen.
    pub visible_rect: Option<LogicalRect>,
    pub animating: bool,
    pub overview_open: bool,
}

impl WindowGeometry {
    /// The rect where input reaches the window right now: on screen, the
    /// overview closed.
    pub(crate) fn live_rect(&self) -> Option<Rect> {
        self.window_rect
            .filter(|_| self.on_screen && !self.overview_open)
            .map(Rect::from)
    }
}

/// The fork's `WindowCaptured` reply: the PNG's size and where the window
/// geometry sits in it (popups and client-side decorations can grow it).
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub(crate) struct WindowCapture {
    pub scale: f64,
    pub window_offset_px: (i32, i32),
}

/// The layer-shell surface a hit test found on top.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub(crate) struct LayerHit {
    pub namespace: String,
}

/// The fork's `WindowAt` reply.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub(crate) struct PointHit {
    pub window_id: Option<i64>,
    /// Clicks reach the window (false on its border or a hidden part).
    pub is_input: bool,
    pub layer: Option<LayerHit>,
}

impl<N: NiriTransport> Niri<N> {
    /// Run one request: its `Ok` payload, or niri's `Err` text.
    fn exchange(&self, request: &Value) -> Result<std::result::Result<Value, String>> {
        let mut line = request.to_string().into_bytes();
        line.push(b'\n');
        let raw = self.transport.exchange(&line)?;
        let first = raw.split(|byte| *byte == b'\n').next().unwrap_or_default();
        let reply: Value = std::str::from_utf8(first)
            .ok()
            .and_then(|text| serde_json::from_str(text).ok())
            .ok_or_else(|| transport("niri IPC returned an unreadable reply"))?;
        if let Some(error) = reply.get("Err") {
            return Ok(Err(error
                .as_str()
                .map_or_else(|| error.to_string(), ToString::to_string)));
        }
        reply
            .get("Ok")
            .filter(|_| reply.is_object())
            .cloned()
            .map(Ok)
            .ok_or_else(|| transport("niri IPC returned an unexpected reply"))
    }

    /// Run one request and unwrap its `Ok` payload.
    pub(crate) fn request(&self, request: &Value) -> Result<Value> {
        self.exchange(request)?.map_err(|reason| {
            transport(format!(
                "niri IPC refused the request: {}",
                head(&reason, ERROR_LIMIT)
            ))
        })
    }

    /// Run one fork request and read its `name` payload, `None` when this
    /// niri does not know the request.
    fn extension<T: DeserializeOwned>(&self, request: &Value, name: &str) -> Result<Option<T>> {
        let ok = match self.exchange(request)? {
            Err(reason) if reason == UNKNOWN_REQUEST => return Ok(None),
            Err(reason) => {
                return Err(transport(format!(
                    "niri IPC refused the {name} request: {}",
                    head(&reason, ERROR_LIMIT)
                )));
            }
            Ok(ok) => ok,
        };
        let payload = ok
            .get(name)
            .cloned()
            .ok_or_else(|| transport(format!("niri IPC returned no {name} payload")))?;
        serde_json::from_value(payload).map(Some).map_err(|error| {
            transport(format!(
                "niri IPC returned an unreadable {name} payload: {}",
                head(&error.to_string(), ERROR_LIMIT)
            ))
        })
    }

    /// The window's rendered geometry, `None` on a niri without the request.
    pub(crate) fn window_geometry(&self, window_id: i64) -> Result<Option<WindowGeometry>> {
        self.extension(
            &json!({"WindowGeometry": {"id": window_id}}),
            "WindowGeometry",
        )
    }

    /// Render the window alone into the PNG at `path` (niri replies once the
    /// file is written), `None` on a niri without the request.
    pub(crate) fn capture_window(
        &self,
        window_id: i64,
        path: &str,
    ) -> Result<Option<WindowCapture>> {
        self.extension(
            &json!({"CaptureWindow": {"id": window_id, "path": path, "show_pointer": false}}),
            "WindowCaptured",
        )
    }

    /// The compositor's input hit test at one global logical point, `None`
    /// on a niri without the request.
    pub(crate) fn window_at(&self, (x, y): (f64, f64)) -> Result<Option<PointHit>> {
        self.extension(&json!({"WindowAt": {"x": x, "y": y}}), "WindowAt")
    }

    /// One output's logical rect, by name.
    pub(crate) fn output_rect(&self, name: &str) -> Result<Option<Rect>> {
        let outputs = self.response("Outputs")?;
        Ok(outputs
            .get(name)
            .and_then(|output| output.get("logical"))
            .filter(|logical| logical.is_object())
            .map(|logical| {
                Rect::new(
                    float(logical.get("x")),
                    float(logical.get("y")),
                    float(logical.get("width")),
                    float(logical.get("height")),
                )
            }))
    }

    /// Run one unit request (`"Windows"`, ...) and return its payload.
    pub(crate) fn response(&self, name: &str) -> Result<Value> {
        let ok = self.request(&Value::from(name))?;
        ok.get(name)
            .cloned()
            .ok_or_else(|| transport(format!("niri IPC returned no {name} payload")))
    }

    /// Every window record.
    pub(crate) fn windows(&self) -> Result<Vec<WindowRecord>> {
        Ok(match self.response("Windows")? {
            Value::Array(windows) => windows
                .into_iter()
                .filter_map(|window| match window {
                    Value::Object(window) => Some(window),
                    _ => None,
                })
                .collect(),
            _ => Vec::new(),
        })
    }

    /// One window by id, or `None` when it is gone.
    pub(crate) fn window(&self, window_id: i64) -> Result<Option<WindowRecord>> {
        Ok(self
            .windows()?
            .into_iter()
            .find(|window| window.get("id").and_then(Value::as_i64) == Some(window_id)))
    }

    /// The focused window's id, or `None` when nothing is focused.
    pub(crate) fn focused_window_id(&self) -> Result<Option<i64>> {
        Ok(self
            .response("FocusedWindow")?
            .get("id")
            .and_then(Value::as_i64))
    }

    /// Ask niri to focus one window.
    pub(crate) fn focus(&self, window_id: i64) -> Result<()> {
        self.request(&json!({"Action": {"FocusWindow": {"id": window_id}}}))
            .map(drop)
    }

    /// The window's logical geometry from its layout, its workspace and the
    /// workspace's output.
    pub(crate) fn geometry(&self, window: &WindowRecord) -> Result<Geometry> {
        let layout = window.get("layout").filter(|layout| layout.is_object());
        let field = |name: &str| {
            layout
                .and_then(|layout| layout.get(name))
                .filter(|value| !value.is_null())
        };
        let (width, height) =
            pair(field("window_size").or_else(|| field("tile_size"))).unwrap_or((0.0, 0.0));
        let tile = pair(field("tile_pos_in_workspace_view"));
        let offset = pair(field("window_offset_in_tile")).unwrap_or((0.0, 0.0));
        let workspaces = self.response("Workspaces")?;
        let workspace = workspaces.as_array().and_then(|workspaces| {
            workspaces
                .iter()
                .find(|entry| entry.is_object() && entry.get("id") == window.get("workspace_id"))
        });
        let output = workspace
            .and_then(|workspace| workspace.get("output"))
            .and_then(Value::as_str)
            .map(ToString::to_string);
        let output_rect = match output.as_deref() {
            Some(name) => self.output_rect(name)?,
            None => None,
        };
        let geometry = |origin, reason| Geometry {
            origin,
            width,
            height,
            output: output.clone(),
            output_rect,
            reason,
        };
        let Some(tile) = tile else {
            return Ok(geometry(
                None,
                "niri exposes on-screen positions only for floating windows; this window is \
                 tiled, so its screen position is unknown",
            ));
        };
        let active = workspace
            .and_then(|workspace| workspace.get("is_active"))
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if !active {
            return Ok(geometry(None, "the window's workspace is not on screen"));
        }
        let Some(out) = output_rect else {
            return Ok(geometry(None, "the window's output geometry is unknown"));
        };
        Ok(geometry(
            Some((out.x + tile.0 + offset.0, out.y + tile.1 + offset.1)),
            "",
        ))
    }
}

/// The real IPC transport over the niri socket.
#[cfg(target_os = "linux")]
pub(crate) struct SocketTransport {
    /// The environment lookup (`std::env::var` in production).
    pub(crate) env: fn(&str) -> Option<String>,
}

#[cfg(target_os = "linux")]
impl Default for SocketTransport {
    fn default() -> Self {
        Self {
            env: |key| std::env::var(key).ok(),
        }
    }
}

#[cfg(target_os = "linux")]
impl NiriTransport for SocketTransport {
    fn exchange(&self, line: &[u8]) -> Result<Vec<u8>> {
        let path = (self.env)("NIRI_SOCKET").unwrap_or_default();
        if path.is_empty() {
            return Err(transport(
                "computer use backend unavailable: NIRI_SOCKET is not set; the Wayland backend \
                 needs niri",
            ));
        }
        exchange_at(&path, line)
    }
}

/// One request line over the socket at `path`, reading the reply up to its
/// first newline (or 8 MiB), bounded by a 2 s timeout.
#[cfg(target_os = "linux")]
pub(crate) fn exchange_at(path: &str, line: &[u8]) -> Result<Vec<u8>> {
    use std::io::{Read, Write};
    const REPLY_LIMIT: usize = 8 * 1024 * 1024;
    {
        let failed = |error: std::io::Error| {
            transport(format!(
                "niri IPC failed: {}",
                head(&error.to_string(), ERROR_LIMIT)
            ))
        };
        let timeout = Some(std::time::Duration::from_secs(2));
        let mut stream = std::os::unix::net::UnixStream::connect(path).map_err(failed)?;
        stream.set_read_timeout(timeout).map_err(failed)?;
        stream.set_write_timeout(timeout).map_err(failed)?;
        stream.write_all(line).map_err(failed)?;
        let mut reply = Vec::new();
        let mut chunk = vec![0_u8; 65536];
        loop {
            let read = stream.read(&mut chunk).map_err(failed)?;
            if read == 0 {
                break;
            }
            reply.extend_from_slice(&chunk[..read]);
            if chunk[..read].contains(&b'\n') || reply.len() > REPLY_LIMIT {
                break;
            }
        }
        Ok(reply)
    }
}
