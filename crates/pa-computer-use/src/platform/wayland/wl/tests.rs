//! The wire client against an in-process fake compositor over a socket pair
//! (the skill's `test_wayland.py` `WireClientTests` and its `FakeCompositor`).

use std::collections::HashMap;
use std::io::IoSliceMut;
use std::mem::MaybeUninit;
use std::os::fd::OwnedFd;
use std::os::unix::fs::FileExt;
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};

use rustix::net::{RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags};

use super::*;
use crate::error::ErrorCode;

/// One recorded request: interface, opcode, its uint words.
type Request = (String, u16, Vec<u32>);

#[derive(Default)]
struct Recorded {
    requests: Vec<Request>,
    keymaps: Vec<Vec<u8>>,
    objects: HashMap<u32, String>,
}

fn string_arg(value: &str) -> Vec<u8> {
    let mut raw = value.as_bytes().to_vec();
    raw.push(0);
    let length = u32::try_from(raw.len()).unwrap();
    while !raw.len().is_multiple_of(4) {
        raw.push(0);
    }
    let mut out = length.to_ne_bytes().to_vec();
    out.extend(raw);
    out
}

fn read_string(payload: &[u8], offset: usize) -> (String, usize) {
    let length = u32::from_ne_bytes(payload[offset..offset + 4].try_into().unwrap()) as usize;
    let start = offset + 4;
    let text = String::from_utf8_lossy(&payload[start..start + length])
        .trim_end_matches('\0')
        .to_string();
    (text, start + length.div_ceil(4) * 4)
}

fn word(payload: &[u8], offset: usize) -> u32 {
    u32::from_ne_bytes(payload[offset..offset + 4].try_into().unwrap())
}

/// The fake compositor: advertises globals, answers sync, records requests.
struct Compositor {
    recorded: Arc<Mutex<Recorded>>,
}

impl Compositor {
    fn start(
        globals: &[(&str, u32)],
        outputs: &[&str],
        error_on: Option<(&str, u16)>,
    ) -> (Self, UnixStream) {
        let (server, client) = UnixStream::pair().unwrap();
        let recorded = Arc::new(Mutex::new(Recorded::default()));
        recorded
            .lock()
            .unwrap()
            .objects
            .insert(1, "wl_display".to_string());
        let globals: Vec<(String, u32)> = globals
            .iter()
            .map(|(name, version)| ((*name).to_string(), *version))
            .collect();
        let outputs: Vec<String> = outputs.iter().map(ToString::to_string).collect();
        let error_on = error_on.map(|(interface, opcode)| (interface.to_string(), opcode));
        let shared = Arc::clone(&recorded);
        std::thread::spawn(move || serve(&server, &shared, &globals, &outputs, error_on.as_ref()));
        (Self { recorded }, client)
    }

    fn calls(&self, interface: &str) -> Vec<(u16, Vec<u32>)> {
        self.recorded
            .lock()
            .unwrap()
            .requests
            .iter()
            .filter(|(name, ..)| name == interface)
            .map(|(_, opcode, words)| (*opcode, words.clone()))
            .collect()
    }

    fn object(&self, interface: &str) -> Vec<u32> {
        let recorded = self.recorded.lock().unwrap();
        let mut ids: Vec<u32> = recorded
            .objects
            .iter()
            .filter(|(_, name)| *name == interface)
            .map(|(id, _)| *id)
            .collect();
        ids.sort_unstable();
        ids
    }
}

fn send(stream: &UnixStream, object: u32, opcode: u16, payload: &[u8]) {
    use std::io::Write;
    let size = u32::try_from(8 + payload.len()).unwrap();
    let mut message = object.to_ne_bytes().to_vec();
    message.extend(((size << 16) | u32::from(opcode)).to_ne_bytes());
    message.extend(payload);
    let _ = (&*stream).write_all(&message);
}

#[allow(clippy::too_many_lines)] // one protocol loop, as the Python fake
fn serve(
    stream: &UnixStream,
    recorded: &Mutex<Recorded>,
    globals: &[(String, u32)],
    outputs: &[String],
    error_on: Option<&(String, u16)>,
) {
    let mut buffer: Vec<u8> = Vec::new();
    let mut fds: Vec<OwnedFd> = Vec::new();
    let mut output_index = 0;
    loop {
        let mut chunk = vec![0_u8; 65536];
        let mut space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(16))];
        let mut ancillary = RecvAncillaryBuffer::new(&mut space);
        let Ok(received) = rustix::net::recvmsg(
            stream,
            &mut [IoSliceMut::new(&mut chunk)],
            &mut ancillary,
            RecvFlags::empty(),
        ) else {
            return;
        };
        for message in ancillary.drain() {
            if let RecvAncillaryMessage::ScmRights(rights) = message {
                fds.extend(rights);
            }
        }
        if received.bytes == 0 {
            return;
        }
        buffer.extend_from_slice(&chunk[..received.bytes]);
        while buffer.len() >= 8 {
            let object = word(&buffer, 0);
            let header = word(&buffer, 4);
            let size = (header >> 16) as usize;
            if buffer.len() < size {
                break;
            }
            let opcode = u16::try_from(header & 0xFFFF).unwrap();
            let payload: Vec<u8> = buffer[8..size].to_vec();
            buffer.drain(..size);
            let interface = recorded
                .lock()
                .unwrap()
                .objects
                .get(&object)
                .cloned()
                .unwrap_or_else(|| "?".to_string());
            let words = if interface == "wl_registry" {
                vec![]
            } else {
                payload
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .map(|bytes| u32::from_ne_bytes(*bytes))
                    .collect()
            };
            if interface == "zwp_virtual_keyboard_v1" && opcode == 0 {
                let fd = fds.remove(0);
                let size = word(&payload, 4) as usize;
                let mut keymap = vec![0_u8; size];
                std::fs::File::from(fd)
                    .read_exact_at(&mut keymap, 0)
                    .unwrap();
                recorded.lock().unwrap().keymaps.push(keymap);
            }
            recorded
                .lock()
                .unwrap()
                .requests
                .push((interface.clone(), opcode, words));
            if error_on.is_some_and(|(failing, code)| *failing == interface && *code == opcode) {
                let mut error = object.to_ne_bytes().to_vec();
                error.extend(3_u32.to_ne_bytes());
                error.extend(string_arg("bad request"));
                send(stream, 1, 0, &error);
                continue;
            }
            match (interface.as_str(), opcode) {
                ("wl_display", 1) => {
                    let registry = word(&payload, 0);
                    recorded
                        .lock()
                        .unwrap()
                        .objects
                        .insert(registry, "wl_registry".to_string());
                    for (name, (global, version)) in globals.iter().enumerate() {
                        let mut event = u32::try_from(name + 1).unwrap().to_ne_bytes().to_vec();
                        event.extend(string_arg(global));
                        event.extend(version.to_ne_bytes());
                        send(stream, registry, 0, &event);
                    }
                }
                ("wl_display", 0) => send(stream, word(&payload, 0), 0, &0_u32.to_ne_bytes()),
                ("wl_registry", 0) => {
                    let (bound, offset) = read_string(&payload, 4);
                    let new_id = word(&payload, offset + 4);
                    recorded
                        .lock()
                        .unwrap()
                        .objects
                        .insert(new_id, bound.clone());
                    recorded.lock().unwrap().requests.last_mut().unwrap().2 =
                        vec![word(&payload, 0), new_id];
                    if bound == "wl_output" {
                        send(stream, new_id, 4, &string_arg(&outputs[output_index]));
                        output_index += 1;
                    }
                }
                ("zwlr_virtual_pointer_manager_v1", 0 | 2) => {
                    let new_id = word(&payload, payload.len() - 4);
                    recorded
                        .lock()
                        .unwrap()
                        .objects
                        .insert(new_id, "zwlr_virtual_pointer_v1".to_string());
                }
                ("zwp_virtual_keyboard_manager_v1", 0) => {
                    let new_id = word(&payload, 4);
                    recorded
                        .lock()
                        .unwrap()
                        .objects
                        .insert(new_id, "zwp_virtual_keyboard_v1".to_string());
                }
                _ => {}
            }
        }
    }
}

const FULL: [(&str, u32); 5] = [
    ("wl_seat", 9),
    ("wl_output", 4),
    ("wl_output", 4),
    ("zwlr_virtual_pointer_manager_v1", 2),
    ("zwp_virtual_keyboard_manager_v1", 1),
];

fn input(
    client: UnixStream,
    clock: fn() -> u32,
) -> WireInput<impl Fn() -> Result<UnixStream> + Send + Sync> {
    let client = Mutex::new(Some(client));
    WireInput::new(
        move || {
            Ok(client
                .lock()
                .unwrap()
                .take()
                .expect("one session per compositor"))
        },
        clock,
    )
}

fn target(output: &str, width: i32, height: i32) -> PointerTarget {
    PointerTarget {
        output: Some(output.to_string()),
        width,
        height,
    }
}

#[test]
fn a_click_creates_a_pointer_on_the_named_output_and_clicks() {
    let (compositor, client) = Compositor::start(&FULL, &["eDP-1", "HDMI-A-1"], None);
    input(client, || 7)
        .click(
            &target("HDMI-A-1", 2560, 1440),
            (112.4, 73.0),
            MouseButton::Right,
            2,
        )
        .unwrap();
    let manager = compositor.calls("zwlr_virtual_pointer_manager_v1");
    let seat = compositor.object("wl_seat")[0];
    let hdmi = compositor.object("wl_output")[1];
    assert_eq!(manager[0].0, 2, "create_virtual_pointer_with_output");
    assert_eq!(manager[0].1[..2], [seat, hdmi]);
    assert_eq!(
        compositor.calls("zwlr_virtual_pointer_v1"),
        [
            (1, vec![7, 112, 73, 2560, 1440]),
            (4, vec![]),
            (2, vec![7, 0x111, 1]),
            (4, vec![]),
            (2, vec![7, 0x111, 0]),
            (4, vec![]),
            (2, vec![7, 0x111, 1]),
            (4, vec![]),
            (2, vec![7, 0x111, 0]),
            (4, vec![]),
            (8, vec![]),
        ]
    );
}

#[test]
fn a_scroll_sends_discrete_wheel_frames() {
    let (compositor, client) = Compositor::start(&FULL, &["eDP-1", "HDMI-A-1"], None);
    input(client, || 1)
        .scroll(
            &target("eDP-1", 1920, 1200),
            (5.0, 6.0),
            ScrollDirection::Up,
            1,
        )
        .unwrap();
    let requests = compositor.calls("zwlr_virtual_pointer_v1");
    assert_eq!(requests[0], (1, vec![1, 5, 6, 1920, 1200]));
    assert_eq!(requests[2], (5, vec![0]));
    let (opcode, args) = &requests[3];
    assert_eq!(*opcode, 7);
    assert_eq!(args[..2], [1, 0]);
    #[allow(clippy::cast_possible_wrap)] // the wire's signed fixed and int, read back
    let signed = (args[2] as i32, args[3] as i32);
    assert_eq!(signed, (-15 * 256, -1));
}

#[test]
fn keys_upload_a_keymap_and_press_with_modifiers() {
    let (compositor, client) = Compositor::start(&FULL, &["eDP-1", "HDMI-A-1"], None);
    let stroke = |keysym: &str, modifiers| KeyStroke {
        keysym: keysym.to_string(),
        modifiers,
    };
    input(client, || 3)
        .send_keys(&[stroke("s", 4), KeyStroke::plain("U00E9"), stroke("s", 4)])
        .unwrap();
    let keymaps = compositor.recorded.lock().unwrap().keymaps.clone();
    let keymap = String::from_utf8(keymaps[0].clone()).unwrap();
    assert!(keymap.ends_with('\0'));
    assert!(keymap.contains("key <K0> {[ s ]};"));
    assert!(keymap.contains("key <K1> {[ U00E9 ]};"));
    assert!(!keymap.contains("<K2>"));
    let length = u32::try_from(keymaps[0].len()).unwrap();
    assert_eq!(
        compositor.calls("zwp_virtual_keyboard_v1"),
        [
            (0, vec![1, length]),
            (2, vec![4, 0, 0, 0]),
            (1, vec![3, 1, 1]),
            (1, vec![3, 1, 0]),
            (2, vec![0, 0, 0, 0]),
            (1, vec![3, 2, 1]),
            (1, vec![3, 2, 0]),
            (2, vec![4, 0, 0, 0]),
            (1, vec![3, 1, 1]),
            (1, vec![3, 1, 0]),
            (2, vec![0, 0, 0, 0]),
            (3, vec![]),
        ]
    );
}

#[test]
fn missing_globals_refuse_naming_the_protocol() {
    let (_compositor, client) =
        Compositor::start(&[("wl_seat", 9), ("wl_output", 4)], &["eDP-1"], None);
    let error = input(client, || 0)
        .click(
            &target("eDP-1", 1920, 1200),
            (1.0, 1.0),
            MouseButton::Left,
            1,
        )
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::ActionUnsupported);
    assert!(error.message.contains("zwlr_virtual_pointer_manager_v1"));
    let globals = [("wl_seat", 9), ("zwlr_virtual_pointer_manager_v1", 2)];
    let (_compositor, client) = Compositor::start(&globals, &[], None);
    assert_eq!(
        input(client, || 0).available().unwrap(),
        Availability {
            pointer: true,
            keyboard: false
        }
    );
    let (_compositor, client) = Compositor::start(&globals, &[], None);
    let error = input(client, || 0)
        .send_keys(&[KeyStroke::plain("a")])
        .unwrap_err();
    assert!(error.message.contains("zwp_virtual_keyboard_manager_v1"));
}

#[test]
fn an_unknown_output_refuses() {
    let (_compositor, client) = Compositor::start(&FULL, &["eDP-1", "HDMI-A-1"], None);
    let error = input(client, || 0)
        .click(&target("DP-9", 100, 100), (1.0, 1.0), MouseButton::Left, 1)
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::ActionUnsupported);
    assert!(error.message.contains("'DP-9'"), "{}", error.message);
}

#[test]
fn a_protocol_error_is_injection_failed() {
    let (_compositor, client) = Compositor::start(
        &FULL,
        &["eDP-1", "HDMI-A-1"],
        Some(("zwlr_virtual_pointer_v1", 2)),
    );
    let error = input(client, || 0)
        .click(
            &target("eDP-1", 1920, 1200),
            (1.0, 1.0),
            MouseButton::Left,
            1,
        )
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::InjectionFailed);
    assert!(error.message.contains("bad request"), "{}", error.message);
}

#[test]
fn the_socket_resolves_from_the_display_and_the_runtime_dir() {
    assert_eq!(
        socket_path(
            Some("wayland-1".to_string()),
            Some("/run/user/5".to_string())
        )
        .unwrap(),
        PathBuf::from("/run/user/5/wayland-1")
    );
    assert_eq!(
        socket_path(Some("/abs/sock".to_string()), None).unwrap(),
        PathBuf::from("/abs/sock")
    );
    assert_eq!(
        socket_path(Some(String::new()), None).unwrap_err().code,
        ErrorCode::ActionUnsupported
    );
    assert_eq!(
        socket_path(Some("w".to_string()), None).unwrap_err().code,
        ErrorCode::ActionUnsupported
    );
}
