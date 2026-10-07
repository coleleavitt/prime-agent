//! The paste transaction's clipboard half over the general pasteboard: the
//! all-types snapshot, the payload write, and the restore. The raw
//! `NSPasteboard` calls are the [`Pasteboard`] seam (`sys` on macOS), so
//! the ordering and failure rules are tested on every host.

use crate::platform::{Clipboard, ClipboardSnapshot, PasteFormat};

/// `NSPasteboardTypeString`.
pub(crate) const STRING_TYPE: &str = "public.utf8-plain-text";
/// `NSPasteboardTypeHTML`.
pub(crate) const HTML_TYPE: &str = "public.html";

/// The raw general-pasteboard calls.
pub(crate) trait Pasteboard: Send + Sync {
    /// The declared types; `None` when the pasteboard cannot be read.
    fn types(&self) -> Option<Vec<String>>;
    fn data(&self, kind: &str) -> Option<Vec<u8>>;
    fn clear(&self);
    /// Whether the pasteboard took the data.
    fn set_data(&self, kind: &str, data: &[u8]) -> bool;
    fn set_string(&self, kind: &str, text: &str) -> bool;
    fn change_count(&self) -> Option<i64>;
}

/// The clipboard rules over one [`Pasteboard`].
pub(crate) struct PasteboardClipboard<P: Pasteboard>(pub(crate) P);

impl<P: Pasteboard> Clipboard for PasteboardClipboard<P> {
    /// Every declared type's data (a type without data is skipped); an
    /// empty pasteboard is an empty snapshot, an unreadable one `None`
    /// (the paste then aborts instead of destroying the clipboard).
    fn save(&self) -> Option<ClipboardSnapshot> {
        let types = self.0.types()?;
        Some(
            types
                .into_iter()
                .filter_map(|kind| {
                    let data = self.0.data(&kind)?;
                    Some((kind, data))
                })
                .collect(),
        )
    }

    /// Clear, then the HTML data (for `html`), then the plain string every
    /// format carries. A refused write is not an error here: the paste's
    /// `holds` check before cmd+v catches a payload that did not land.
    fn write(&self, text: &str, format: PasteFormat) -> Result<(), String> {
        self.0.clear();
        if format == PasteFormat::Html {
            let _ = self.0.set_data(HTML_TYPE, text.as_bytes());
        }
        let _ = self.0.set_string(STRING_TYPE, text);
        Ok(())
    }

    fn holds(&self, text: &str) -> bool {
        self.0
            .data(STRING_TYPE)
            .is_some_and(|data| data == text.as_bytes())
    }

    fn change_count(&self) -> Option<i64> {
        self.0.change_count()
    }

    /// Clear once, then set every saved type; one type the pasteboard
    /// refuses never blocks the rest.
    fn restore(&self, snapshot: &ClipboardSnapshot) {
        self.0.clear();
        for (kind, data) in snapshot {
            let _ = self.0.set_data(kind, data);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum Op {
        Clear,
        SetData(String, Vec<u8>),
        SetString(String, String),
    }

    #[derive(Default)]
    struct Fake {
        types: Option<Vec<String>>,
        data: Vec<(String, Vec<u8>)>,
        refuses: Vec<String>,
        ops: Mutex<Vec<Op>>,
    }

    impl Fake {
        fn holding(entries: &[(&str, &[u8])]) -> Self {
            Self {
                types: Some(
                    entries
                        .iter()
                        .map(|(kind, _)| (*kind).to_string())
                        .collect(),
                ),
                data: entries
                    .iter()
                    .map(|(kind, data)| ((*kind).to_string(), data.to_vec()))
                    .collect(),
                ..Self::default()
            }
        }

        fn ops(&self) -> Vec<Op> {
            self.ops.lock().unwrap().clone()
        }
    }

    impl Pasteboard for Fake {
        fn types(&self) -> Option<Vec<String>> {
            self.types.clone()
        }
        fn data(&self, kind: &str) -> Option<Vec<u8>> {
            self.data
                .iter()
                .find_map(|(name, data)| (name == kind).then(|| data.clone()))
        }
        fn clear(&self) {
            self.ops.lock().unwrap().push(Op::Clear);
        }
        fn set_data(&self, kind: &str, data: &[u8]) -> bool {
            self.ops
                .lock()
                .unwrap()
                .push(Op::SetData(kind.to_string(), data.to_vec()));
            !self.refuses.iter().any(|refused| refused == kind)
        }
        fn set_string(&self, kind: &str, text: &str) -> bool {
            self.ops
                .lock()
                .unwrap()
                .push(Op::SetString(kind.to_string(), text.to_string()));
            !self.refuses.iter().any(|refused| refused == kind)
        }
        fn change_count(&self) -> Option<i64> {
            Some(3)
        }
    }

    #[test]
    fn save_and_restore_cover_every_pasteboard_type() {
        let entries: [(&str, &[u8]); 3] = [
            (STRING_TYPE, b"hello"),
            ("public.png", b"\x89PNG"),
            ("public.file-url", b"file:///tmp/a"),
        ];
        let clipboard = PasteboardClipboard(Fake::holding(&entries));
        let saved = clipboard.save().unwrap();
        assert_eq!(
            saved,
            entries.map(|(kind, data)| (kind.to_string(), data.to_vec()))
        );
        clipboard.restore(&saved);
        let mut expected = vec![Op::Clear];
        expected.extend(entries.map(|(kind, data)| Op::SetData(kind.to_string(), data.to_vec())));
        assert_eq!(clipboard.0.ops(), expected);
    }

    #[test]
    fn a_type_without_data_is_skipped_and_an_unreadable_board_is_none() {
        let mut fake = Fake::holding(&[(STRING_TYPE, b"x")]);
        fake.types = Some(vec![STRING_TYPE.to_string(), "public.promise".to_string()]);
        let clipboard = PasteboardClipboard(fake);
        assert_eq!(
            clipboard.save().unwrap(),
            [(STRING_TYPE.to_string(), b"x".to_vec())]
        );
        let unreadable = PasteboardClipboard(Fake::default());
        assert_eq!(unreadable.save(), None);
    }

    #[test]
    fn an_empty_snapshot_restores_as_one_clear() {
        let clipboard = PasteboardClipboard(Fake::holding(&[]));
        let saved = clipboard.save().unwrap();
        assert!(saved.is_empty());
        clipboard.restore(&saved);
        assert_eq!(clipboard.0.ops(), [Op::Clear]);
    }

    #[test]
    fn one_failing_type_never_blocks_the_remaining_restore() {
        let clipboard = PasteboardClipboard(Fake {
            refuses: vec!["public.png".to_string()],
            ..Fake::default()
        });
        let snapshot = vec![
            ("public.png".to_string(), b"png".to_vec()),
            (STRING_TYPE.to_string(), b"text".to_vec()),
        ];
        clipboard.restore(&snapshot);
        assert_eq!(
            clipboard.0.ops(),
            [
                Op::Clear,
                Op::SetData("public.png".to_string(), b"png".to_vec()),
                Op::SetData(STRING_TYPE.to_string(), b"text".to_vec()),
            ]
        );
    }

    #[test]
    fn writes_clear_then_put_html_before_the_plain_string() {
        let clipboard = PasteboardClipboard(Fake::default());
        clipboard.write("<b>hi</b>", PasteFormat::Html).unwrap();
        clipboard.write("plain", PasteFormat::Markdown).unwrap();
        assert_eq!(
            clipboard.0.ops(),
            [
                Op::Clear,
                Op::SetData(HTML_TYPE.to_string(), b"<b>hi</b>".to_vec()),
                Op::SetString(STRING_TYPE.to_string(), "<b>hi</b>".to_string()),
                Op::Clear,
                Op::SetString(STRING_TYPE.to_string(), "plain".to_string()),
            ]
        );
        let held = PasteboardClipboard(Fake::holding(&[(STRING_TYPE, b"payload")]));
        assert!(held.holds("payload"));
        assert!(!held.holds("other"));
        assert_eq!(held.change_count(), Some(3));
    }
}
