//! The completion fence: the status script wrapped around every command and
//! the in-band completion marker read back from its output.
//!
//! The command's foreground status travels on a socket the child receives as
//! stdin (remapped to [`STATUS_FD`]), so `cmd &` does not hang the await: the
//! result is final at foreground completion, while the shell then `wait`s for
//! its background jobs and keeps the process group alive. A random marker
//! written to the output just before the status marks where the command's own
//! output ends; halves of the token travel separately so a passive echo of
//! the wrapper (`set -x`, `/proc/$$/cmdline`) never forms it.

/// Child-side fd of the status channel. POSIX shells (notably dash) accept
/// only single-digit fds in redirections.
pub(crate) const STATUS_FD: u8 = 9;
/// Child-side duplicate of the output pipe the marker is written to.
pub(crate) const OUTPUT_FD: u8 = 8;
pub(crate) const COMPLETION_PREFIX: &[u8] = b"\x1eprime-agent-complete:";
pub(crate) const COMPLETION_SUFFIX: &[u8] = b"\x1f";

/// The default utility search path (`confstr(_CS_PATH)`), where `printf` is
/// resolved so a user function or alias named `command`/`printf` cannot
/// swallow the fence frames.
#[cfg(target_os = "macos")]
const SYSTEM_UTILITY_PATH: &str = "/usr/bin:/bin:/usr/sbin:/sbin";
#[cfg(not(target_os = "macos"))]
const SYSTEM_UTILITY_PATH: &str = "/bin:/usr/bin";

/// The `printf` the fence uses: slash-qualified (bypassing function and alias
/// lookup) when it can be found and quoted, else `\command -p printf`.
fn fence_printf() -> String {
    match crate::shell::which("printf", Some(SYSTEM_UTILITY_PATH)) {
        Some(path) if !path.to_string_lossy().contains('\'') => {
            format!("'{}'", path.to_string_lossy())
        }
        Some(_) | None => "\\command -p printf".to_string(),
    }
}

/// The script the shell runs for `command`: remap the status channel, wait for
/// the gate byte (sent once the pid is journaled), run the command with the
/// control fds closed, then write the marker and the status.
pub(crate) fn status_script(command: &str, token_a: &str, token_b: &str) -> String {
    let emit = fence_printf();
    format!(
        "exec {STATUS_FD}>&0 {OUTPUT_FD}>&1 0</dev/null\n\
         read -r _prime_agent_gate <&{STATUS_FD} || exit 127\n\
         {{\n\
         {command}\n\
         }} {OUTPUT_FD}>&- {STATUS_FD}>&-\n\
         __prime_status=$?\n\
         \\set +x\n\
         {emit} '\\036prime-agent-complete:%s%s\\037' '{token_a}' '{token_b}' >&{OUTPUT_FD} || exit \"$__prime_status\"\n\
         {emit} '%s\\n' \"$__prime_status\" >&{STATUS_FD}\n\
         exec {OUTPUT_FD}>&- {STATUS_FD}>&-\n\
         wait\n\
         exit \"$__prime_status\"\n"
    )
}

/// What one chunk of output means for the fence.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Scanned {
    /// Bytes that belong to the output stream now.
    pub before: Vec<u8>,
    /// The marker was found: `after` follows it (output written past the
    /// fence, e.g. by an EXIT trap or a background job).
    pub fence: Option<Vec<u8>>,
}

/// Finds the marker in the output stream, holding back a tail that could be
/// the start of a marker split across reads.
#[derive(Debug)]
pub(crate) struct MarkerScanner {
    marker: Vec<u8>,
    pending: Vec<u8>,
}

impl MarkerScanner {
    pub(crate) fn new(token: &str) -> Self {
        let mut marker = COMPLETION_PREFIX.to_vec();
        marker.extend_from_slice(token.as_bytes());
        marker.extend_from_slice(COMPLETION_SUFFIX);
        Self {
            marker,
            pending: Vec::new(),
        }
    }

    /// Feed one chunk read before the marker was seen.
    pub(crate) fn feed(&mut self, chunk: &[u8]) -> Scanned {
        let mut data = std::mem::take(&mut self.pending);
        data.extend_from_slice(chunk);
        if let Some(at) = find(&data, &self.marker) {
            let after = data[at + self.marker.len()..].to_vec();
            data.truncate(at);
            return Scanned {
                before: data,
                fence: Some(after),
            };
        }
        let longest = data.len().min(self.marker.len() - 1);
        let retained = (1..=longest)
            .rev()
            .find(|size| data.ends_with(&self.marker[..*size]))
            .unwrap_or(0);
        self.pending = data.split_off(data.len() - retained);
        Scanned {
            before: data,
            fence: None,
        }
    }

    /// The held-back bytes, released when the stream ends without a marker.
    pub(crate) fn take_pending(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.pending)
    }
}

/// The first occurrence of `needle`, scanning for its first byte (the
/// marker starts with a control byte that output rarely carries).
pub(crate) fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    let (&first, _) = needle.split_first()?;
    let mut from = 0;
    while let Some(offset) = haystack.get(from..)?.iter().position(|byte| *byte == first) {
        let at = from + offset;
        if haystack.get(at..at + needle.len()) == Some(needle) {
            return Some(at);
        }
        from = at + 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOKEN: &str = "0123456789abcdef0123456789abcdef";

    fn marker() -> Vec<u8> {
        [COMPLETION_PREFIX, TOKEN.as_bytes(), COMPLETION_SUFFIX].concat()
    }

    /// Python `test_completion_marker_split_across_reads_is_removed` (7-byte reads).
    #[test]
    fn a_marker_split_across_reads_is_removed() {
        let mut stream = b"exact-pre-fence-output".to_vec();
        stream.extend_from_slice(&marker());
        stream.extend_from_slice(b"late");
        let mut marker_scanner = MarkerScanner::new(TOKEN);
        let mut output = Vec::new();
        let mut after = None;
        for chunk in stream.chunks(7) {
            if let Some(rest) = after.as_mut() {
                let rest: &mut Vec<u8> = rest;
                rest.extend_from_slice(chunk);
                continue;
            }
            let scanned = marker_scanner.feed(chunk);
            output.extend_from_slice(&scanned.before);
            after = scanned.fence;
        }
        assert_eq!(output, b"exact-pre-fence-output");
        assert_eq!(after, Some(b"late".to_vec()));
    }

    #[test]
    fn a_lookalike_marker_stays_output() {
        let mut marker_scanner = MarkerScanner::new(TOKEN);
        let lookalike = b"\x1eprime-agent-complete:not-this-invocation\x1f";
        let scanned = marker_scanner.feed(lookalike);
        assert_eq!(
            scanned,
            Scanned {
                before: lookalike.to_vec(),
                fence: None
            }
        );
        assert!(marker_scanner.take_pending().is_empty());
    }

    #[test]
    fn the_script_never_names_the_raw_token() {
        let script = status_script("echo hi", "aaaa", "bbbb");
        assert!(script.contains("'aaaa' 'bbbb' >&8"));
        assert!(!script.contains("aaaabbbb"));
        assert!(script.starts_with("exec 9>&0 8>&1 0</dev/null\nread -r _prime_agent_gate <&9 || exit 127\n{\necho hi\n} 8>&- 9>&-\n"));
    }
}
