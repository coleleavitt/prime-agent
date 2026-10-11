//! Secure-field rules: passwords and other secrets are the user's to enter.
//!
//! A secure field never renders its value, and keyboard input, pastes and
//! value writes into one are refused with a hand-off to the user. Every
//! check fails closed: a field or focus whose state cannot be read is
//! treated as one that might be secure.

use serde_json::json;

use crate::error::{ComputerUseError, unsupported};

/// macOS marks password inputs `AXTextField` / `AXSecureTextField`.
pub const MAC_SECURE_ROLE: &str = "AXTextField";
pub const MAC_SECURE_SUBROLE: &str = "AXSecureTextField";
/// The Wayland backend renders AT-SPI `ROLE_PASSWORD_TEXT` with this role.
pub const ATSPI_SECURE_ROLE: &str = "password text";

/// Whether an element with this role and subrole is a secure text field.
#[must_use]
pub fn is_secure_field(role: Option<&str>, subrole: Option<&str>) -> bool {
    (role == Some(MAC_SECURE_ROLE) && subrole == Some(MAC_SECURE_SUBROLE))
        || role == Some(ATSPI_SECURE_ROLE)
}

/// One live security read of a field or of the focused element.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Security {
    NotSecure,
    Secure,
    /// The read failed or could not complete: callers fail closed.
    Unverifiable,
}

impl Security {
    /// Map a backend probe: `None` is unverifiable.
    #[must_use]
    pub fn from_probe(probe: Option<bool>) -> Self {
        match probe {
            None => Security::Unverifiable,
            Some(true) => Security::Secure,
            Some(false) => Security::NotSecure,
        }
    }
}

/// Refuse keyboard entry unless the live focus is verifiably not secure.
///
/// # Errors
///
/// `ACTION_UNSUPPORTED` with `{"live": true}` for a secure focus and
/// `{"live": false}` for one that could not be verified.
pub fn refuse_secure_focus(focus: Security) -> Result<(), ComputerUseError> {
    match focus {
        Security::NotSecure => Ok(()),
        Security::Secure => Err(unsupported(
            "the focused element is a secure text field; ask the user to type passwords and \
             other secrets themselves",
        )
        .with_details(json!({"live": true}))),
        Security::Unverifiable => Err(unsupported(
            "could not verify that the focused element is not a secure text field; ask the \
             user to type passwords and other secrets themselves",
        )
        .with_details(json!({"live": false}))),
    }
}

/// The hand-off text for a write into a secure element.
pub const SECURE_HANDOFF: &str = "this element is a secure field; Prime Agent never types into \
                                  it — ask the user to enter the value";

/// Refuse a value write or selection into element `index` unless both the
/// snapshot and the live element say it is not secure.
///
/// # Errors
///
/// `ACTION_UNSUPPORTED`: the secure hand-off when either read says secure
/// (`{"element_index", "secure": true}`), the unverifiable refusal when
/// the live read failed.
pub fn refuse_secure_write(
    index: usize,
    snapshot_secure: bool,
    live: Security,
) -> Result<(), ComputerUseError> {
    if snapshot_secure || live == Security::Secure {
        return Err(unsupported(format!("element {index}: {SECURE_HANDOFF}"))
            .with_details(json!({"element_index": index, "secure": true})));
    }
    if live == Security::Unverifiable {
        return Err(unsupported(format!(
            "element {index}: could not verify that it is not a secure field; ask the user to \
             enter the value themselves"
        ))
        .with_details(json!({"element_index": index})));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::ErrorCode;

    #[test]
    fn secure_roles_cover_mac_and_atspi_password_fields() {
        assert!(is_secure_field(
            Some("AXTextField"),
            Some("AXSecureTextField")
        ));
        assert!(is_secure_field(Some("password text"), None));
        assert!(!is_secure_field(Some("AXTextField"), None));
        assert!(!is_secure_field(Some("AXSecureTextField"), None));
        assert!(!is_secure_field(None, Some("AXSecureTextField")));
    }

    #[test]
    fn focus_refusals_name_live_and_unverifiable() {
        assert_eq!(refuse_secure_focus(Security::NotSecure), Ok(()));
        let secure = refuse_secure_focus(Security::Secure).unwrap_err();
        assert_eq!(secure.code, ErrorCode::ActionUnsupported);
        assert_eq!(secure.details, Some(json!({"live": true})));
        assert!(secure.message.contains("ask the user"));
        let unknown = refuse_secure_focus(Security::Unverifiable).unwrap_err();
        assert_eq!(unknown.details, Some(json!({"live": false})));
        assert!(unknown.message.contains("could not verify"));
    }

    #[test]
    fn a_secure_snapshot_wins_over_an_unverifiable_live_read() {
        let error = refuse_secure_write(4, true, Security::Unverifiable).unwrap_err();
        assert_eq!(
            error,
            unsupported(format!("element 4: {SECURE_HANDOFF}"))
                .with_details(json!({"element_index": 4, "secure": true}))
        );
        let unverifiable = refuse_secure_write(1, false, Security::Unverifiable).unwrap_err();
        assert_eq!(unverifiable.details, Some(json!({"element_index": 1})));
        assert_eq!(refuse_secure_write(1, false, Security::NotSecure), Ok(()));
    }
}
