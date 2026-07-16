//! Detecting when `orx` is running inside an SSH session, and guiding the user
//! to reach a loopback-bound server (`orx up`) from their laptop.
//!
//! `orx up` binds `127.0.0.1` with no auth (see `commands::up`), so when the
//! process is on a remote box the laptop can't reach it directly — and we must
//! NOT expose it on `0.0.0.0` (an unauthenticated dashboard on a shared host is
//! an actively-exploited attack surface). The safe, idiomatic answer is SSH
//! local port forwarding (`ssh -L`), the same pattern Jupyter/TensorBoard users
//! follow.
//!
//! The catch: an `ssh -L` forward can only be created by the SSH *client* on the
//! laptop. This process runs on the *server* side, so the most it can do is
//! print the exact command to paste. Fully automatic forwarding lives in
//! `orx up --remote <host>` (`commands::up_remote`), which owns the client side.

use std::io::IsTerminal;

/// Whether stderr can render ANSI: a real terminal and `NO_COLOR` unset. Pipes,
/// CI, and redirects get plain text — never raw escape codes. Mirrors the same
/// gate in [`crate::updates`].
fn stderr_supports_ansi() -> bool {
    std::env::var_os("NO_COLOR").is_none() && std::io::stderr().is_terminal()
}

/// Bold when `enabled`, else the text unchanged. `\x1b[22m` resets bold/faint
/// specifically (not `\x1b[0m`, which would clear surrounding styling too).
fn bold(text: &str, enabled: bool) -> String {
    if enabled {
        format!("\x1b[1m{text}\x1b[22m")
    } else {
        text.to_string()
    }
}

/// Dim (faint) when `enabled`, else the text unchanged — used for the prose
/// asides that explain the commands, so the commands themselves stand out.
fn dim(text: &str, enabled: bool) -> String {
    if enabled {
        format!("\x1b[2m{text}\x1b[22m")
    } else {
        text.to_string()
    }
}

/// Cyan when `enabled`, else the text unchanged. Used for the `<…>` fill-in-the-
/// blank placeholders inside commands: they must read as "replace me" without
/// fading out — dimming them (the old behavior) buried the one word the reader
/// most needs to see. `\x1b[39m` resets only the foreground color.
fn cyan(text: &str, enabled: bool) -> String {
    if enabled {
        format!("\x1b[36m{text}\x1b[39m")
    } else {
        text.to_string()
    }
}

/// Facts about the current SSH session, parsed from `SSH_CONNECTION`.
///
/// `SSH_CONNECTION` is set by sshd as `"clientIP clientPort serverIP serverPort"`.
/// We keep the server-side fields: `server_ip` is a best-effort reconnect hint
/// (often a private/NATed address that is NOT reachable as-is), and `sshd_port`
/// is the port sshd listens on (usually 22).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SshSession {
    /// 3rd field of `SSH_CONNECTION` — the server address as the server sees it.
    pub server_ip: String,
    /// 4th field of `SSH_CONNECTION` — the sshd port (usually 22).
    pub sshd_port: String,
}

/// Detect whether we're in an SSH session, reading only the environment.
///
/// Presence is decided by `SSH_CONNECTION`/`SSH_TTY`/`SSH_CLIENT` (any one).
/// The parsed [`SshSession`] fields come from `SSH_CONNECTION` when it is
/// well-formed; a session detected only via `SSH_TTY`/`SSH_CLIENT`, or with a
/// malformed `SSH_CONNECTION`, still returns `Some` but with empty hint fields.
pub fn detect_ssh_session() -> Option<SshSession> {
    detect_from(
        std::env::var("SSH_CONNECTION").ok().as_deref(),
        std::env::var("SSH_TTY").is_ok() || std::env::var("SSH_CLIENT").is_ok(),
    )
}

/// Testable core: given the raw `SSH_CONNECTION` value (if any) and whether some
/// other SSH marker is present, decide the session.
fn detect_from(ssh_connection: Option<&str>, other_marker: bool) -> Option<SshSession> {
    match ssh_connection {
        Some(conn) => {
            // "clientIP clientPort serverIP serverPort". Take fields 3 and 4 when
            // present; tolerate a malformed value by falling back to empty hints
            // rather than reporting "not in SSH" (the var's presence is itself
            // the SSH signal).
            let fields: Vec<&str> = conn.split_whitespace().collect();
            let (server_ip, sshd_port) = match fields.as_slice() {
                [_, _, ip, port, ..] => ((*ip).to_string(), (*port).to_string()),
                _ => (String::new(), String::new()),
            };
            Some(SshSession {
                server_ip,
                sshd_port,
            })
        }
        None if other_marker => Some(SshSession {
            server_ip: String::new(),
            sshd_port: String::new(),
        }),
        None => None,
    }
}

impl SshSession {
    /// Is `server_ip` a plausibly-routable public address worth suggesting? A
    /// private/loopback/link-local/CGNAT address from `SSH_CONNECTION` is usually
    /// the box's internal interface and won't work from the laptop, so we don't
    /// offer it. This is a hint filter, not a security boundary.
    fn server_ip_hint(&self) -> Option<&str> {
        let ip = self.server_ip.trim();
        if ip.is_empty() {
            return None;
        }
        if let Ok(v4) = ip.parse::<std::net::Ipv4Addr>() {
            // `is_private()` misses CGNAT (100.64.0.0/10, RFC 6598), which is
            // common on cloud/NAT'd boxes — filter it too.
            let [a, b, ..] = v4.octets();
            let is_cgnat = a == 100 && (64..=127).contains(&b);
            if v4.is_private()
                || v4.is_loopback()
                || v4.is_link_local()
                || v4.is_unspecified()
                || is_cgnat
            {
                return None;
            }
        }
        // IPv6 (or an unparseable value): don't filter, just don't over-promise.
        Some(ip)
    }

    /// The message to print on `orx up` startup when we're in SSH. `port` is the
    /// port `orx up` bound on the remote's loopback.
    ///
    /// The reader is *inside* the SSH session, so the text is careful to say
    /// "from your laptop" — both suggested commands run on the laptop, never here.
    /// Styling is stripped on pipes/CI/`NO_COLOR` so those get clean plain text.
    pub fn instructions(&self, port: u16) -> String {
        self.render_instructions(port, stderr_supports_ansi())
    }

    /// Core of [`instructions`], with ANSI styling as an explicit arg so it's
    /// testable without touching the environment.
    fn render_instructions(&self, port: u16, ansi: bool) -> String {
        // The two commands are the whole point, so they're the brightest thing
        // on screen: bold command text, a `$` prompt so they read as "type
        // this", the connection target in cyan (readable, obviously a
        // fill-in-the-blank), and prose dimmed so it frames rather than competes.
        //
        // Adaptive target: many boxes (RunPod / openresearch dev nodes) are
        // reached by raw `user@host -p PORT`, with no `~/.ssh/config` alias — so
        // "<ssh-alias>" is a dead end there. We anchor on the one thing every
        // reader has: the `user@host` (and any `-p PORT`) they just typed to get
        // in. When `SSH_CONNECTION` hands us a usable *public* server IP we splice
        // it straight in so the command is nearly copy-paste; otherwise we show a
        // `<user@host>` placeholder and point at their own `ssh` line.
        let prompt = dim("$", ansi);
        let hosted_ip = self.server_ip_hint();
        // What goes in the connection-target slot of each command: the box's own
        // public IP when `SSH_CONNECTION` gives us a usable one (nearly
        // copy-paste), else a `<user@host>` placeholder — the one thing every
        // reader has, alias or not.
        let target = match hosted_ip {
            Some(ip) => cyan(ip, ansi),
            None => cyan("<user@host>", ansi),
        };
        // Custom SSH ports (RunPod / openresearch dev nodes connect on e.g.
        // :38455) can't be recovered here: `SSH_CONNECTION`'s port field is
        // sshd's own listen port, not the one the client dialed. Only the manual
        // `ssh` command can carry `-p PORT`; `orx up --remote` has no port flag,
        // so we deliberately keep the port slot OFF the option-1 line.
        let port_slot = cyan("[-p PORT]", ansi);

        // Each command: bold command-text + cyan target + dim trailing comment,
        // so within a line the part you type stays bright and asides recede.
        let primary = format!("{} {target}", bold("orx up --remote", ansi));
        let manual = format!(
            "{} {port_slot} {target}   {}",
            bold(&format!("ssh -N -L {port}:localhost:{port}"), ansi),
            dim(&format!("# then open http://localhost:{port}"), ansi),
        );

        // Point the reader at where the target comes from — their own `ssh`
        // line — and flag the custom-port caveat that bites RunPod users:
        // option 1 only works on the default port 22 / a config alias, so on a
        // non-standard port option 2 is the one to use.
        let target_note = match hosted_ip {
            Some(ip) => format!(
                "{ip} is this box's address. If it doesn't connect, use the same \
                 user@host (and -p PORT) from the ssh command you ran to get here."
            ),
            None => "<user@host> = whatever you typed to ssh into this box \
                     (e.g. root@203.0.113.5), no ~/.ssh/config alias needed. \
                     [-p PORT] = the port you connect on; drop it if that's 22."
                .to_string(),
        };
        let port_caveat = "On a non-default SSH port, use option 2 — orx up --remote \
                           can't pass -p yet.";

        format!(
            "{header}\n\n\
             {intro}\n\n\
             {opt1}\n\
             \x20 {prompt} {primary}\n\n\
             {opt2}\n\
             \x20 {prompt} {manual}\n\n\
             {target_note}\n\
             {port_caveat}\n",
            header = bold(
                &format!("orx up: serving on http://127.0.0.1:{port} (this remote host)"),
                ansi,
            ),
            intro = dim(
                "This URL only works here on the box. To open it from your laptop,\n\
                 run one of these in a laptop terminal — not in this SSH session:",
                ansi,
            ),
            opt1 = bold(
                "1) Let orx forward it for you (default port 22 / an alias):",
                ansi
            ),
            opt2 = bold("2) Or forward it yourself (works on any port):", ansi),
            target_note = dim(&target_note, ansi),
            port_caveat = dim(port_caveat, ansi),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_well_formed_ssh_connection() {
        let s = detect_from(Some("203.0.113.7 51344 198.51.100.5 22"), false).unwrap();
        assert_eq!(s.server_ip, "198.51.100.5");
        assert_eq!(s.sshd_port, "22");
    }

    #[test]
    fn absent_everything_is_none() {
        assert_eq!(detect_from(None, false), None);
    }

    #[test]
    fn ssh_tty_only_detects_with_empty_hints() {
        let s = detect_from(None, true).unwrap();
        assert!(s.server_ip.is_empty());
        assert!(s.sshd_port.is_empty());
    }

    #[test]
    fn malformed_ssh_connection_still_detects() {
        // Present but not 4 fields: we're clearly in SSH, just no usable hint.
        let s = detect_from(Some("garbage"), false).unwrap();
        assert!(s.server_ip.is_empty());
        let s2 = detect_from(Some("10.0.0.1 5000"), false).unwrap();
        assert!(s2.server_ip.is_empty());
    }

    #[test]
    fn private_server_ip_is_not_hinted() {
        let s = detect_from(Some("203.0.113.7 51344 10.0.0.5 22"), false).unwrap();
        // Parsed and stored, but filtered out of the suggestion.
        assert_eq!(s.server_ip, "10.0.0.5");
        assert!(s.server_ip_hint().is_none());
        assert!(!s.render_instructions(4791, false).contains("10.0.0.5"));
    }

    #[test]
    fn cgnat_server_ip_is_not_hinted() {
        // 100.64.0.0/10 (RFC 6598) is carrier NAT, not reachable from the laptop.
        let s = detect_from(Some("203.0.113.7 51344 100.64.1.2 22"), false).unwrap();
        assert_eq!(s.server_ip, "100.64.1.2");
        assert!(s.server_ip_hint().is_none());
    }

    #[test]
    fn public_server_ip_is_hinted() {
        let s = detect_from(Some("203.0.113.7 51344 198.51.100.5 22"), false).unwrap();
        assert_eq!(s.server_ip_hint(), Some("198.51.100.5"));
        assert!(s.render_instructions(4791, false).contains("198.51.100.5"));
    }

    #[test]
    fn instructions_mention_both_paths_and_port() {
        // Private server IP → the placeholder branch. Plain (no ANSI) so
        // assertions match the literal text.
        let msg = detect_from(Some("203.0.113.7 51344 10.0.0.5 22"), false)
            .unwrap()
            .render_instructions(4899, false);
        assert!(msg.contains("orx up --remote"));
        assert!(msg.contains("ssh -N -L 4899:localhost:4899"));
        assert!(msg.contains("http://localhost:4899"));
        // We anchor on `user@host` (works with no ~/.ssh/config alias), not on a
        // bare "alias" the reader may not have.
        assert!(msg.contains("<user@host>"));
        assert!(msg.contains("no ~/.ssh/config alias needed"));
    }

    #[test]
    fn custom_port_slot_is_only_on_the_manual_command() {
        // `orx up --remote` has no `-p` flag, so the port slot must NOT appear on
        // option 1 — only on the manual `ssh` line, which does accept `-p`.
        let msg = detect_from(Some("203.0.113.7 51344 10.0.0.5 22"), false)
            .unwrap()
            .render_instructions(4791, false);
        let primary_line = msg
            .lines()
            .find(|l| l.contains("$ orx up --remote"))
            .expect("option-1 command line present");
        assert!(!primary_line.contains("-p PORT"));
        let manual_line = msg
            .lines()
            .find(|l| l.contains("$ ssh -N -L"))
            .expect("option-2 command line present");
        assert!(manual_line.contains("[-p PORT]"));
        // And the caveat spells out why option 1 can't do custom ports.
        assert!(msg.contains("orx up --remote can't pass -p yet"));
    }

    #[test]
    fn public_server_ip_is_inlined_into_the_commands() {
        // A usable public IP is spliced straight into both commands so they're
        // nearly copy-paste, instead of leaving a placeholder.
        let msg = detect_from(Some("203.0.113.7 51344 198.51.100.5 22"), false)
            .unwrap()
            .render_instructions(4791, false);
        assert!(msg.contains("orx up --remote 198.51.100.5"));
        assert!(msg.contains("198.51.100.5   # then open"));
        // No leftover placeholder when we filled the IP in.
        assert!(!msg.contains("<user@host>"));
    }

    #[test]
    fn styling_is_stripped_when_ansi_disabled() {
        // No raw escape codes leak into non-terminal output.
        let msg = detect_from(Some("203.0.113.7 51344 198.51.100.5 22"), false)
            .unwrap()
            .render_instructions(4791, false);
        assert!(!msg.contains('\x1b'));
    }

    #[test]
    fn styling_wraps_the_primary_command_when_ansi_enabled() {
        let msg = detect_from(Some("203.0.113.7 51344 10.0.0.5 22"), false)
            .unwrap()
            .render_instructions(4791, true);
        // The primary command is bolded; the whole thing carries escape codes.
        assert!(msg.contains("\x1b[1morx up --remote\x1b[22m"));
    }

    #[test]
    fn both_commands_are_bold_and_placeholders_are_cyan_not_dim() {
        // Readability fix: the commands you type must be the brightest thing on
        // screen, and the `<user@host>` you fill in must be legible cyan — not
        // dimmed into the background like it used to be.
        let msg = detect_from(Some("203.0.113.7 51344 10.0.0.5 22"), false)
            .unwrap()
            .render_instructions(4791, true);
        // Both commands bold.
        assert!(msg.contains("\x1b[1morx up --remote\x1b[22m"));
        assert!(msg.contains("\x1b[1mssh -N -L 4791:localhost:4791\x1b[22m"));
        // The placeholder is cyan (\x1b[36m…\x1b[39m), and never dimmed.
        assert!(msg.contains("\x1b[36m<user@host>\x1b[39m"));
        assert!(!msg.contains("\x1b[2m<user@host>\x1b[22m"));
    }
}
