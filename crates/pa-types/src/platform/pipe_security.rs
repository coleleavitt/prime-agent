//! Owner-only policy for the Windows named-pipe transport, as pure string logic (compiled on every
//! platform so its tests run everywhere; the Win32 bindings live in `windows_security`).
//!
//! The `\\.\pipe\` namespace is machine-global: any account can create a pipe under any free name
//! or connect to a pipe whose DACL lets it. Every pipe this transport creates is owned by the
//! creating user and grants access to that user only, and a client refuses a pipe another
//! account owns (a squatter pre-creating the name would otherwise receive the client's
//! `create` config and launch environment).

/// The SDDL for a pipe only `user_sid` may open: the owner is the user (explicitly, so an
/// elevated creator does not default the owner to the Administrators group), and a protected DACL
/// grants that user generic-all and nobody else anything.
pub(crate) fn owner_only_pipe_sddl(user_sid: &str) -> String {
    format!("O:{user_sid}D:P(A;;GA;;;{user_sid})")
}

/// Refuse a pipe whose owner is not the connecting user.
///
/// # Errors
///
/// `PermissionDenied` naming both SIDs when they differ.
pub(crate) fn ensure_pipe_owner(
    pipe_owner_sid: &str,
    current_user_sid: &str,
) -> std::io::Result<()> {
    if pipe_owner_sid.eq_ignore_ascii_case(current_user_sid) {
        return Ok(());
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::PermissionDenied,
        format!(
            "named pipe is owned by {pipe_owner_sid}, not the current user {current_user_sid}; \
             refusing to connect"
        ),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_pipe_acl_grants_only_its_owner() {
        assert_eq!(
            owner_only_pipe_sddl("S-1-5-21-1-2-3-1001"),
            "O:S-1-5-21-1-2-3-1001D:P(A;;GA;;;S-1-5-21-1-2-3-1001)"
        );
    }

    #[test]
    fn a_pipe_owned_by_another_account_is_refused() {
        let me = "S-1-5-21-1-2-3-1001";
        let outcomes: Vec<Option<std::io::ErrorKind>> =
            [me, "s-1-5-21-1-2-3-1001", "S-1-5-21-1-2-3-1002"]
                .iter()
                .map(|owner| ensure_pipe_owner(owner, me).err().map(|error| error.kind()))
                .collect();
        assert_eq!(
            outcomes,
            vec![None, None, Some(std::io::ErrorKind::PermissionDenied)]
        );
    }
}
