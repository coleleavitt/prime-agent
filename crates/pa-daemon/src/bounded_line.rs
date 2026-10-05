//! Newline-delimited command reads with a per-line byte cap: the supervisor's client sockets,
//! RPC stdin, and ACP stdin. A peer that streams bytes without a newline must not grow this
//! process's memory without limit.

use tokio::io::{AsyncBufRead, AsyncBufReadExt};

/// Upper bound for one command line from a local peer (a unix-socket client, or the process
/// driving RPC/ACP stdin). Larger than the TCP cap
/// ([`crate::tcp::DAEMON_TCP_MAX_LINE_CHARS`], 1 MiB) because local prompts legitimately carry
/// pasted images inline as base64: the TUI accepts a pasted image up to 64 MiB (about 85 MiB
/// encoded), and a prompt can carry several. 256 MiB admits that, and still stops a newline-free
/// stream.
pub(crate) const LOCAL_COMMAND_MAX_LINE_BYTES: usize = 256 * 1024 * 1024;

/// The outcome of one bounded line read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BoundedLine {
    /// A complete line (with its newline) was appended to the caller's `line`.
    Line,
    /// The stream ended. Bytes of an unterminated final line stay in `line_bytes`.
    Eof,
    /// The line outgrew the cap. The reader stays mid-line; the caller closes the stream or
    /// calls [`skip_rest_of_line`].
    Overflow,
}

/// Read the next newline-terminated line into `line`, refusing to buffer more than `max` bytes.
/// Bytes accumulate raw in `line_bytes`, so a multi-byte UTF-8 character split across reads
/// cannot corrupt the line and a read cancelled mid-line (a `select!` arm) resumes where it
/// stopped. A line that outgrows the cap reports [`BoundedLine::Overflow`] without draining the
/// peer's stream.
///
/// # Errors
///
/// Returns the reader's I/O error.
pub(crate) async fn read_bounded_line<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    line: &mut String,
    line_bytes: &mut Vec<u8>,
    max: usize,
) -> std::io::Result<BoundedLine> {
    loop {
        let available = match reader.fill_buf().await {
            Ok(available) => available,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        };
        if available.is_empty() {
            return Ok(BoundedLine::Eof);
        }
        let Some(newline) = available.iter().position(|byte| *byte == b'\n') else {
            if line_bytes.len() + available.len() > max {
                line_bytes.clear();
                return Ok(BoundedLine::Overflow);
            }
            line_bytes.extend_from_slice(available);
            let used = available.len();
            reader.consume(used);
            continue;
        };
        if line_bytes.len() + newline + 1 > max {
            line_bytes.clear();
            return Ok(BoundedLine::Overflow);
        }
        line_bytes.extend_from_slice(&available[..=newline]);
        reader.consume(newline + 1);
        line.push_str(&String::from_utf8_lossy(line_bytes));
        // The next read starts a fresh line.
        line_bytes.clear();
        return Ok(BoundedLine::Line);
    }
}

/// Discard the rest of an overflowed line through its newline, buffering nothing. Returns `false`
/// when the stream ended first.
///
/// # Errors
///
/// Returns the reader's I/O error.
pub(crate) async fn skip_rest_of_line<R: AsyncBufRead + Unpin>(
    reader: &mut R,
) -> std::io::Result<bool> {
    loop {
        let available = match reader.fill_buf().await {
            Ok(available) => available,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        };
        if available.is_empty() {
            return Ok(false);
        }
        if let Some(newline) = available.iter().position(|byte| *byte == b'\n') {
            reader.consume(newline + 1);
            return Ok(true);
        }
        let used = available.len();
        reader.consume(used);
    }
}

/// [`read_bounded_line`] for a stdin-style command stream that skips an oversized line instead of
/// closing: the overflow is reported through `on_overflow` and reading continues with the next
/// line. Returns `false` at end of stream or on a read error.
pub(crate) async fn next_command_line<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    line: &mut String,
    max: usize,
    on_overflow: impl Fn(),
) -> bool {
    let mut line_bytes = Vec::new();
    loop {
        match read_bounded_line(reader, line, &mut line_bytes, max).await {
            Ok(BoundedLine::Line) => return true,
            Ok(BoundedLine::Eof) => {
                // An unterminated final line still counts (like `read_line`).
                if line_bytes.is_empty() {
                    return false;
                }
                line.push_str(&String::from_utf8_lossy(&line_bytes));
                return true;
            }
            Ok(BoundedLine::Overflow) => {
                on_overflow();
                match skip_rest_of_line(reader).await {
                    Ok(true) => {}
                    Ok(false) | Err(_) => return false,
                }
            }
            Err(_) => return false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An oversized line is skipped whole (never buffered past the cap) and the stream resumes
    /// at the next line; an unterminated final line still arrives, like `read_line`.
    #[tokio::test]
    async fn an_oversized_command_line_is_skipped_and_reading_continues() {
        let input = format!("short\n{}\nnext\ntail", "x".repeat(64));
        let mut reader = tokio::io::BufReader::with_capacity(8, input.as_bytes());
        let overflows = std::cell::Cell::new(0);
        let mut lines = Vec::new();
        let mut line = String::new();
        while next_command_line(&mut reader, &mut line, 16, || {
            overflows.set(overflows.get() + 1);
        })
        .await
        {
            lines.push(std::mem::take(&mut line));
        }
        assert_eq!(
            (lines, overflows.get()),
            (
                vec![
                    "short\n".to_string(),
                    "next\n".to_string(),
                    "tail".to_string()
                ],
                1
            )
        );
    }

    /// The cap counts raw bytes across partial reads, newline included.
    #[tokio::test]
    async fn the_cap_admits_a_line_of_exactly_max_bytes() {
        let mut reader = tokio::io::BufReader::with_capacity(3, "abcdefg\nabcdefgh\n".as_bytes());
        let mut line = String::new();
        let mut line_bytes = Vec::new();
        let first = read_bounded_line(&mut reader, &mut line, &mut line_bytes, 8)
            .await
            .unwrap();
        let second = read_bounded_line(&mut reader, &mut String::new(), &mut line_bytes, 8)
            .await
            .unwrap();
        assert_eq!(
            (first, line.as_str(), second),
            (BoundedLine::Line, "abcdefg\n", BoundedLine::Overflow)
        );
    }
}
