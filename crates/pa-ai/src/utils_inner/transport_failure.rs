//! Transport-failure facts for request-send failures that carry no response: the cause chain a
//! diagnostic records, and whether the failure is a stale pooled connection.
//!
//! A keep-alive pool can hand a request a connection the peer already closed (the edge closes it
//! right after a response, before the pool sees the FIN). The request then dies with no response:
//! hyper's "connection closed before message completed", or a reset/broken pipe from the socket.
//! Claude Code treats the same class (`ECONNRESET`, `EPIPE`, `ConnectionClosed`, `ETIMEDOUT`,
//! `ECONNABORTED`, a closed socket) as a stale connection and retries on a fresh one; the HTTP
//! layer does the same through [`is_stale_connection`].

use std::error::Error as StdError;

/// Whether a request-send failure is a stale pooled connection: the peer closed or reset a
/// connection the request was written to, so no response can have been produced. Connect failures
/// (refused, unreachable, DNS) and our own deadline are not stale: a fresh connection would meet
/// the same network.
pub(crate) fn is_stale_connection(error: &reqwest::Error) -> bool {
    !error.is_connect() && !error.is_timeout() && chain_is_stale(error)
}

/// The stale-connection test over an error's source chain (the error itself excluded).
fn chain_is_stale(error: &(dyn StdError + 'static)) -> bool {
    sources(error).any(|source| {
        if let Some(hyper) = source.downcast_ref::<hyper::Error>() {
            return hyper.is_incomplete_message() || hyper.is_canceled() || hyper.is_closed();
        }
        source.downcast_ref::<std::io::Error>().is_some_and(|io| {
            matches!(
                io.kind(),
                std::io::ErrorKind::ConnectionReset
                    | std::io::ErrorKind::BrokenPipe
                    | std::io::ErrorKind::ConnectionAborted
                    | std::io::ErrorKind::NotConnected
                    | std::io::ErrorKind::UnexpectedEof
                    | std::io::ErrorKind::TimedOut
            )
        })
    })
}

/// The underlying cause of a request-send failure: each source's text, outermost first, joined
/// with `": "`. The reqwest error itself is skipped: its text names the request URL, and some
/// providers carry credentials in the query string.
pub(crate) fn cause_chain(error: &reqwest::Error) -> String {
    let chain = sources(error)
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(": ");
    if chain.is_empty() {
        // No source: the reqwest-level class is all there is.
        let class = if error.is_connect() {
            "connect error"
        } else if error.is_timeout() {
            "timed out"
        } else if error.is_request() {
            "request error"
        } else if error.is_body() {
            "body error"
        } else {
            "transport error"
        };
        return class.to_string();
    }
    chain
}

/// The source chain below `error`, outermost first.
fn sources<'a>(
    error: &'a (dyn StdError + 'static),
) -> impl Iterator<Item = &'a (dyn StdError + 'static)> + 'a {
    std::iter::successors(error.source(), |&source: &&'a (dyn StdError + 'static)| {
        source.source()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An error whose single source is `source`, as reqwest wraps its transport causes.
    #[derive(Debug)]
    struct Wrapped(std::io::Error);

    impl std::fmt::Display for Wrapped {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("error sending request for url (https://example.test/?key=secret)")
        }
    }

    impl StdError for Wrapped {
        fn source(&self) -> Option<&(dyn StdError + 'static)> {
            Some(&self.0)
        }
    }

    #[test]
    fn socket_resets_and_closes_are_stale_and_refusals_are_not() {
        for kind in [
            std::io::ErrorKind::ConnectionReset,
            std::io::ErrorKind::BrokenPipe,
            std::io::ErrorKind::ConnectionAborted,
            std::io::ErrorKind::NotConnected,
            std::io::ErrorKind::UnexpectedEof,
            std::io::ErrorKind::TimedOut,
        ] {
            assert!(
                chain_is_stale(&Wrapped(std::io::Error::from(kind))),
                "{kind:?} is a stale connection"
            );
        }
        for kind in [
            std::io::ErrorKind::ConnectionRefused,
            std::io::ErrorKind::PermissionDenied,
            std::io::ErrorKind::Other,
        ] {
            assert!(
                !chain_is_stale(&Wrapped(std::io::Error::from(kind))),
                "{kind:?} is not a stale connection"
            );
        }
    }
}
