//! Images inside tmux: what terminal the tmux client is, and whether the
//! pane may pass escapes through to it.
//!
//! tmux draws the pane itself, so an image escape reaches the outer
//! terminal only through its passthrough (`DCS tmux; … ST`), which tmux
//! honours only while `allow-passthrough` is `on` or `all` (off by default
//! since 3.3). The pane's inherited environment cannot say what is outside:
//! tmux overwrites `TERM_PROGRAM`, and `KITTY_WINDOW_ID`/`ITERM_SESSION_ID`
//! are whatever the server's first client had. tmux itself knows its
//! client: `#{client_termname}` is the client's `TERM`, `#{client_termtype}`
//! its XTVERSION answer (`kitty(0.46.0)`, `iTerm2 3.5.0`, `ghostty 1.2.0`,
//! `WezTerm 2024…`), and `#{client_termfeatures}` what tmux negotiated
//! (`RGB` for true colour). One bounded `display-message` reads them all
//! with the pane's option and geometry (`crate::clipboard::tmux_output_within`,
//! the OSC 8 probe's 250 ms cap), once, off the paint path.

use super::{ImageProtocol, ImageTerminal, ImageTransport};

/// The one query: tab-separated so no value can split a field.
pub(crate) const TMUX_CLIENT_FORMAT: &str = "#{client_termname}\t#{client_termtype}\t\
#{client_termfeatures}\t#{allow-passthrough}\t#{pane_top}\t#{pane_left}\t#{status}\t\
#{status-position}";

/// The probe's bound (the OSC 8 tmux probe's): a slow or wedged server
/// means the text fallback, never a stalled frame.
pub(crate) const TMUX_PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(250);

/// What tmux reports about the client this pane renders to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TmuxClient {
    pub(crate) termname: String,
    pub(crate) termtype: String,
    pub(crate) features: String,
    pub(crate) passthrough: String,
    /// The pane's origin on the client's screen (a top status line pushes
    /// the window down).
    pub(crate) origin_row: u16,
    pub(crate) origin_column: u16,
}

/// Parse the [`TMUX_CLIENT_FORMAT`] answer.
pub(crate) fn parse_tmux_client(answer: &str) -> Option<TmuxClient> {
    let line = answer.lines().next()?;
    let fields: Vec<&str> = line.split('\t').collect();
    let [
        termname,
        termtype,
        features,
        passthrough,
        top,
        left,
        status,
        position,
    ] = fields.as_slice()
    else {
        return None;
    };
    let status_rows: u16 = match *status {
        "off" | "" => 0,
        "on" => 1,
        rows => rows.parse().ok()?,
    };
    let above = if *position == "top" { status_rows } else { 0 };
    Some(TmuxClient {
        termname: (*termname).to_string(),
        termtype: (*termtype).to_string(),
        features: (*features).to_string(),
        passthrough: (*passthrough).to_string(),
        origin_row: top.parse::<u16>().ok()?.saturating_add(above),
        origin_column: left.parse().ok()?,
    })
}

/// The outer terminal's image protocol, from tmux's view of its client:
/// the XTVERSION answer first (it names the emulator itself), then the
/// client's `TERM`. `WezTerm` takes the iTerm2 form here: its kitty graphics
/// have no unicode placeholders, which the tmux path needs.
fn client_protocol(client: &TmuxClient) -> Option<ImageProtocol> {
    let termtype = client.termtype.as_str();
    if termtype.starts_with("kitty(") || termtype.starts_with("ghostty ") {
        return Some(ImageProtocol::Kitty);
    }
    if termtype.starts_with("iTerm2 ") || termtype.starts_with("WezTerm ") {
        return Some(ImageProtocol::Iterm2);
    }
    if termtype.is_empty() {
        let termname = client.termname.to_ascii_lowercase();
        if termname.contains("kitty") || termname.contains("ghostty") {
            return Some(ImageProtocol::Kitty);
        }
        if termname.starts_with("wezterm") {
            return Some(ImageProtocol::Iterm2);
        }
    }
    None
}

/// Whether tmux sends this client 24-bit colour: the `RGB` feature
/// (`terminal-features`, or an XTVERSION tmux recognises), or the
/// emulator's own terminfo, whose `Tc` tmux honours without listing it
/// (kitty 0.46 answers XTVERSION as `kitty(…)`, which tmux does not map to
/// features: its `client_termfeatures` reads `bpaste,ccolour,clipboard,
/// cstyle,focus,title`, yet `xterm-kitty`'s `Tc` turns true colour on).
fn true_colour(client: &TmuxClient) -> bool {
    client
        .features
        .split(',')
        .any(|feature| feature.trim() == "RGB")
        || matches!(client.termname.as_str(), "xterm-kitty" | "xterm-ghostty")
}

/// The image terminal a tmux pane reaches, or `None` for the text
/// fallback: passthrough off (the default), an outer terminal without a
/// protocol, or — for kitty's placeholders, whose image id rides a 24-bit
/// foreground colour — a client tmux does not send true colour to.
pub(crate) fn tmux_image_terminal(client: &TmuxClient) -> Option<ImageTerminal> {
    if !matches!(client.passthrough.as_str(), "on" | "all") {
        return None;
    }
    let protocol = client_protocol(client)?;
    if protocol == ImageProtocol::Kitty && !true_colour(client) {
        return None;
    }
    Some(ImageTerminal {
        protocol,
        transport: ImageTransport::Tmux {
            origin_row: client.origin_row,
            origin_column: client.origin_column,
        },
    })
}

/// Ask the tmux server about this pane's client (bounded; `None` on any
/// failure).
pub(crate) fn probe_tmux_client() -> Option<TmuxClient> {
    let pane = std::env::var("TMUX_PANE").ok();
    let mut args = vec!["display-message", "-p"];
    if let Some(pane) = pane.as_deref() {
        args.extend(["-t", pane]);
    }
    args.push(TMUX_CLIENT_FORMAT);
    crate::clipboard::tmux_output_within(&args, TMUX_PROBE_TIMEOUT)
        .and_then(|answer| parse_tmux_client(&answer))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client(termtype: &str, termname: &str, features: &str, passthrough: &str) -> String {
        format!("{termname}\t{termtype}\t{features}\t{passthrough}\t3\t41\ton\ttop\n")
    }

    const KITTY_FEATURES: &str = "256,RGB,bpaste,clipboard,mouse,strikethrough,title,ccolour,\
cstyle,extkeys,focus,margins,overline,hyperlinks,osc7,sync,usstyle,progressbar";

    fn terminal(answer: &str) -> Option<ImageTerminal> {
        tmux_image_terminal(&parse_tmux_client(answer).expect("the answer parses"))
    }

    fn tmux(protocol: ImageProtocol, origin_row: u16, origin_column: u16) -> Option<ImageTerminal> {
        Some(ImageTerminal {
            protocol,
            transport: ImageTransport::Tmux {
                origin_row,
                origin_column,
            },
        })
    }

    #[test]
    fn the_answer_parses_with_the_status_line_offset() {
        assert_eq!(
            parse_tmux_client(&client("kitty(0.46.0)", "xterm-kitty", "RGB", "on")),
            Some(TmuxClient {
                termname: "xterm-kitty".to_string(),
                termtype: "kitty(0.46.0)".to_string(),
                features: "RGB".to_string(),
                passthrough: "on".to_string(),
                origin_row: 4,
                origin_column: 41,
            })
        );
        let bottom = "xterm-kitty\t\t\toff\t0\t0\t2\tbottom";
        assert_eq!(
            parse_tmux_client(bottom).map(|c| (c.origin_row, c.origin_column)),
            Some((0, 0))
        );
        let top_two = "xterm-kitty\t\t\toff\t5\t0\t2\ttop";
        assert_eq!(parse_tmux_client(top_two).map(|c| c.origin_row), Some(7));
        assert_eq!(parse_tmux_client("too\tfew"), None);
        assert_eq!(parse_tmux_client(""), None);
    }

    /// The env × reply matrix inside tmux: passthrough on/all/off, the
    /// client's XTVERSION and TERM, and true colour for placeholders. The
    /// pane's inherited variables play no part (they can be stale).
    #[test]
    fn the_tmux_terminal_follows_the_client_and_the_passthrough() {
        let kitty =
            |passthrough| client("kitty(0.46.0)", "xterm-kitty", KITTY_FEATURES, passthrough);
        assert_eq!(terminal(&kitty("on")), tmux(ImageProtocol::Kitty, 4, 41));
        assert_eq!(terminal(&kitty("all")), tmux(ImageProtocol::Kitty, 4, 41));
        // Off by default since tmux 3.3: the text fallback.
        assert_eq!(terminal(&kitty("off")), None);
        assert_eq!(terminal(&kitty("")), None);
        // Ghostty answers XTVERSION too.
        assert_eq!(
            terminal(&client("ghostty 1.2.0", "xterm-ghostty", "RGB", "on")),
            tmux(ImageProtocol::Kitty, 4, 41)
        );
        // No XTVERSION answer (an older tmux): the client's TERM decides.
        assert_eq!(
            terminal(&client("", "xterm-kitty", "RGB", "on")),
            tmux(ImageProtocol::Kitty, 4, 41)
        );
        // Placeholders need true colour to the client: kitty's own terminfo
        // carries `Tc` (the live kitty 0.46.2 answer lists no `RGB`), a
        // generic TERM without the feature gets 256 colours.
        assert_eq!(
            terminal(
                "xterm-kitty\tkitty(0.46.2)\tbpaste,ccolour,clipboard,cstyle,focus,title\ton\t0\t0\ton\tbottom"
            ),
            tmux(ImageProtocol::Kitty, 0, 0)
        );
        assert_eq!(
            terminal(&client(
                "kitty(0.46.0)",
                "xterm-256color",
                "256,title",
                "on"
            )),
            None
        );
        assert_eq!(
            terminal(&client("kitty(0.46.0)", "xterm-256color", "256,RGB", "on")),
            tmux(ImageProtocol::Kitty, 4, 41)
        );
        // iTerm2 and WezTerm take OSC 1337 through the same passthrough.
        assert_eq!(
            terminal(&client("iTerm2 3.5.4", "xterm-256color", "256", "on")),
            tmux(ImageProtocol::Iterm2, 4, 41)
        );
        assert_eq!(
            terminal(&client("WezTerm 20240203", "xterm-256color", "RGB", "on")),
            tmux(ImageProtocol::Iterm2, 4, 41)
        );
        // An emulator without a protocol, whatever TERM says.
        assert_eq!(
            terminal(&client("XTerm(390)", "xterm-kitty", "RGB", "on")),
            None
        );
        assert_eq!(terminal(&client("", "xterm-256color", "RGB", "on")), None);
    }

    /// ssh into a host and run tmux there: the client's TERM crosses ssh
    /// in the pty request and its XTVERSION answer rides the connection
    /// back, so the remote tmux reports the local kitty the same way.
    #[test]
    fn ssh_then_tmux_reports_the_local_terminal() {
        assert_eq!(
            terminal("xterm-kitty\tkitty(0.46.0)\t256,RGB,title\ton\t0\t0\ton\tbottom"),
            tmux(ImageProtocol::Kitty, 0, 0)
        );
    }
}
