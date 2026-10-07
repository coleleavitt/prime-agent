//! The frozen error taxonomy every computer-use failure reports.
//!
//! The kernel raises each one as `computer_use.ComputerUseError(code,
//! message, details)`: `code` is one of [`ErrorCode`]'s wire names, the
//! message is the model-facing recovery text, and `details` optionally
//! carries machine-readable context.

use serde_json::Value;

/// One frozen error code (the Python `errors.CODES` table).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ErrorCode {
    AppNotAllowed,
    PermissionsNotGranted,
    PermissionsPending,
    ScreenLocked,
    UserStopped,
    ElementStale,
    AmbiguousApp,
    AppNotRunning,
    AppLaunchFailed,
    ActionUnsupported,
    InjectionFailed,
    TransportError,
    InvalidArgument,
}

impl ErrorCode {
    /// The code's wire name (`APP_NOT_ALLOWED`, ...).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            ErrorCode::AppNotAllowed => "APP_NOT_ALLOWED",
            ErrorCode::PermissionsNotGranted => "PERMISSIONS_NOT_GRANTED",
            ErrorCode::PermissionsPending => "PERMISSIONS_PENDING",
            ErrorCode::ScreenLocked => "SCREEN_LOCKED",
            ErrorCode::UserStopped => "USER_STOPPED",
            ErrorCode::ElementStale => "ELEMENT_STALE",
            ErrorCode::AmbiguousApp => "AMBIGUOUS_APP",
            ErrorCode::AppNotRunning => "APP_NOT_RUNNING",
            ErrorCode::AppLaunchFailed => "APP_LAUNCH_FAILED",
            ErrorCode::ActionUnsupported => "ACTION_UNSUPPORTED",
            ErrorCode::InjectionFailed => "INJECTION_FAILED",
            ErrorCode::TransportError => "TRANSPORT_ERROR",
            ErrorCode::InvalidArgument => "INVALID_ARGUMENT",
        }
    }
}

impl std::fmt::Display for ErrorCode {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// One failed computer-use operation with a stable machine code.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
#[error("{code}: {message}")]
pub struct ComputerUseError {
    pub code: ErrorCode,
    pub message: String,
    /// A JSON object, or `None` (the Python `details=None`); an empty
    /// object stays `{}`.
    pub details: Option<Value>,
}

impl ComputerUseError {
    /// An error without details.
    #[must_use]
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            details: None,
        }
    }

    /// Attach the details object (`serde_json::json!({...})`).
    #[must_use]
    pub fn with_details(mut self, details: Value) -> Self {
        debug_assert!(
            details.is_object(),
            "computer-use error details are an object"
        );
        self.details = Some(details);
        self
    }

    /// The wire form the kernel client raises from.
    #[must_use]
    pub fn to_wire(&self) -> Value {
        serde_json::json!({
            "code": self.code.as_str(),
            "message": self.message,
            "details": self.details.clone().unwrap_or(Value::Null),
        })
    }
}

pub type Result<T, E = ComputerUseError> = std::result::Result<T, E>;

/// Shorthand constructors for the codes the backends raise most.
pub(crate) fn transport(message: impl Into<String>) -> ComputerUseError {
    ComputerUseError::new(ErrorCode::TransportError, message)
}

pub(crate) fn invalid(message: impl Into<String>) -> ComputerUseError {
    ComputerUseError::new(ErrorCode::InvalidArgument, message)
}

pub(crate) fn unsupported(message: impl Into<String>) -> ComputerUseError {
    ComputerUseError::new(ErrorCode::ActionUnsupported, message)
}

pub(crate) fn injection_failed(message: impl Into<String>) -> ComputerUseError {
    ComputerUseError::new(ErrorCode::InjectionFailed, message)
}

pub(crate) fn not_running(message: impl Into<String>) -> ComputerUseError {
    ComputerUseError::new(ErrorCode::AppNotRunning, message)
}

/// The first `limit` characters of `text` (Python's `text[:limit]`, by code point).
pub(crate) fn head(text: &str, limit: usize) -> &str {
    match text.char_indices().nth(limit) {
        Some((end, _)) => &text[..end],
        None => text,
    }
}

/// The 200-character cap every backend applies to a tool's stderr or a
/// framework's error text before it lands in a message.
pub(crate) const ERROR_LIMIT: usize = 200;

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_wire_form_keeps_absent_and_empty_details_apart() {
        let bare = ComputerUseError::new(ErrorCode::ElementStale, "index 5 is gone");
        assert_eq!(
            bare.to_wire(),
            json!({"code": "ELEMENT_STALE", "message": "index 5 is gone", "details": null})
        );
        let empty = transport("x").with_details(json!({}));
        assert_eq!(
            empty.to_wire(),
            json!({"code": "TRANSPORT_ERROR", "message": "x", "details": {}})
        );
    }

    #[test]
    fn head_slices_by_code_point() {
        assert_eq!(head("héllo", 2), "hé");
        assert_eq!(head("hi", 5), "hi");
        assert_eq!(head("", 0), "");
    }
}
