//! The paste transaction (`test_api`'s paste cases, `test_w1`'s clipboard
//! snapshot, write-failure and change-count cases).

use serde_json::json;

use super::*;
use crate::keymap::parse_chord;
use crate::platform::PasteFormat;
use crate::session::fake::{Call, ClipboardCall, PID};

fn paste(text: &str, format: PasteFormat) -> AppCall {
    AppCall::Paste {
        text: text.to_string(),
        format,
    }
}

fn saved() -> ClipboardCall {
    ClipboardCall::Restore(vec![(
        "public.utf8-plain-text".to_string(),
        b"saved".to_vec(),
    )])
}

#[test]
fn paste_saves_writes_presses_cmd_v_and_restores() {
    let env = Env::new();
    let app = bound(&env);
    call(&env, &app, paste("rich text", PasteFormat::Markdown)).unwrap();
    assert_eq!(
        env.fake().clipboard_calls,
        [
            ClipboardCall::Save,
            ClipboardCall::Write {
                format: PasteFormat::Markdown,
                text: "rich text".to_string()
            },
            saved()
        ]
    );
    assert_eq!(
        env.fake().calls,
        [Call::PressKey {
            pid: PID,
            chord: parse_chord("cmd+v").unwrap()
        }]
    );
}

#[test]
fn paste_refuses_a_focused_secure_field_before_touching_the_clipboard() {
    let env = Env::new();
    env.fake().secure_focus = Some(true);
    let app = bound(&env);
    assert_eq!(
        code(call(&env, &app, paste("secret", PasteFormat::Text))),
        ErrorCode::ActionUnsupported
    );
    assert!(env.fake().clipboard_calls.is_empty());
}

#[test]
fn a_failed_snapshot_aborts_before_touching_the_clipboard() {
    let env = Env::new();
    env.fake().clipboard_saved = None;
    let app = bound(&env);
    let error = error(call(&env, &app, paste("payload", PasteFormat::Text)));
    assert_eq!(error.code, ErrorCode::TransportError);
    assert!(error.message.contains("snapshot"));
    assert!(env.fake().clipboard_calls.is_empty());
    assert!(env.fake().calls.is_empty());
}

#[test]
fn a_copy_during_the_paste_window_survives_and_nothing_is_pasted() {
    let env = Env::new();
    env.fake().clipboard_holds = false;
    env.fake().change_counts = vec![None];
    let app = bound(&env);
    let error = error(call(&env, &app, paste("payload", PasteFormat::Text)));
    assert_eq!(
        error,
        crate::error::transport(
            "the clipboard changed during the paste; the payload was not pasted"
        )
        .with_details(json!({}))
    );
    assert!(env.fake().calls.is_empty());
    assert_eq!(
        env.fake().clipboard_calls,
        [
            ClipboardCall::Save,
            ClipboardCall::Write {
                format: PasteFormat::Text,
                text: "payload".to_string()
            }
        ]
    );
}

#[test]
fn a_same_text_copy_with_different_rich_data_is_kept() {
    // The change count moved during the paste: rich data changed.
    let env = Env::new();
    env.fake().change_counts = vec![Some(3), Some(4)];
    let app = bound(&env);
    call(&env, &app, paste("payload", PasteFormat::Text)).unwrap();
    assert!(!env.fake().clipboard_calls.contains(&saved()));
}

#[test]
fn a_failed_write_restores_the_snapshot() {
    let env = Env::new();
    env.fake().write_error = Some("pasteboard refused the write".to_string());
    let app = bound(&env);
    let error = error(call(&env, &app, paste("payload", PasteFormat::Text)));
    assert_eq!(
        error.message,
        "could not write the paste payload: pasteboard refused the write"
    );
    assert!(env.fake().clipboard_calls.contains(&saved()));
}
