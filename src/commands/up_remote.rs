//! `orx up --remote <host>` — run `orx up` on a remote box and forward it here.
//!
//! This is the laptop-side half of remote access. Unlike a bare `orx up` on a
//! box you SSH'd into (which can only *print* an `ssh -L` command — see
//! `crate::remote`), here orx owns the SSH client, so it can set up the local
//! forward itself: it starts `orx up` on the remote, tunnels the port to this
//! machine, waits for the server to come up, and opens the browser.
//!
//! Transport is the `ssh` binary with the same ControlMaster/BatchMode options
//! the SSH job backend uses (`crate::jobs::ssh`); auth is the user's own
//! `~/.ssh/config` + agent/keys — orx never reads a key.

use std::process::Stdio;
use std::time::{Duration, Instant};

use tokio::process::{Child, Command};

use crate::error::{anyhow, Result};
use crate::jobs::ssh::{HostKeyPolicy, SshTarget};
use crate::{browser, UpArgs};

/// How long to wait for the remote server to answer through the forward.
const HEALTH_TIMEOUT: Duration = Duration::from_secs(60);

pub async fn run(host: &str, args: UpArgs) -> Result<()> {
    let port = args.port;
    let target = parse_remote_target(host);

    // One round-trip that both proves the host is reachable and checks `orx` is
    // installed there — the forward would come up but nothing would serve on it
    // otherwise. A transport error means unreachable; a clean run without the
    // marker means `orx` is missing. (No separate preflight: its git check is
    // irrelevant to `orx up`, and it'd cost an extra unmultiplexed handshake.)
    eprintln!("orx up --remote: checking {host}…");
    // The command exits 0 either way (emitting a distinct marker) so a non-zero
    // exit from ssh_run unambiguously means "transport failed / unreachable" —
    // not "orx missing". Without the `else` branch, a missing orx exits non-zero
    // and would be misreported as an unreachable host.
    match crate::jobs::ssh::ssh_run(
        &target,
        "if command -v orx >/dev/null 2>&1; then echo ORX_OK; else echo ORX_MISSING; fi",
        None,
    )
    .await
    {
        Err(e) => {
            return Err(anyhow!(
                "can't reach '{host}' over SSH: {e}. Check it's an ~/.ssh/config \
                 alias, or a user@host (add :PORT for a non-standard SSH port, \
                 e.g. root@1.2.3.4:2222) you can `ssh` into. A custom key or jump \
                 host isn't reconstructed here — put it in ~/.ssh/config. If it's a \
                 new host reached by name, `ssh` in once first to trust its key; if \
                 its key changed (a reused IP), clear it with `ssh-keygen -R`."
            ));
        }
        Ok(out) if !out.contains("ORX_OK") => {
            return Err(anyhow!(
                "`orx` isn't installed on '{host}' (or not on its non-interactive \
                 PATH). Install it there, then re-run `orx up --remote {host}`."
            ));
        }
        Ok(_) => {}
    }

    // One ssh invocation that both forwards the port and starts the remote
    // server. `-N` would suppress the remote command, so we pass the command
    // explicitly and rely on `-L` for the tunnel. The remote binds its own
    // loopback:port; `-L 127.0.0.1:port:localhost:port` maps this machine's
    // loopback to it — we pin the local bind to 127.0.0.1 (not the default
    // `localhost`, which can resolve to ::1 first on a dual-stack host) so it
    // matches the IPv4 address `wait_healthy` and the browser use.
    // `--no-browser` on the remote: there's no display there, and we open ours.
    let remote_cmd = remote_up_cmd(port);
    let forward = forward_spec(port);
    let mut child = spawn_ssh_forward(&target, &forward, &remote_cmd)?;

    // Wait until the remote server answers through the forward (or ssh dies).
    eprintln!("orx up --remote: starting orx up on {host} and forwarding port {port}…");
    if let Err(e) = wait_healthy(&mut child, port).await {
        let _ = child.start_kill();
        let _ = child.wait().await;
        return Err(e);
    }

    let url = format!("http://localhost:{port}");
    eprintln!("orx up --remote: dashboard on {url} (forwarded from {host})");
    if !args.no_browser {
        browser::open_browser(&url);
    }
    eprintln!("orx up --remote: press Ctrl-C to stop forwarding.");

    // Hold the tunnel open until the user quits or ssh exits on its own.
    tokio::select! {
        status = child.wait() => {
            let status = status.map_err(|e| anyhow!("ssh wait failed: {e}"))?;
            if !status.success() {
                return Err(anyhow!(
                    "ssh forwarding to '{host}' ended unexpectedly ({status})."
                ));
            }
        }
        _ = tokio::signal::ctrl_c() => {
            eprintln!("orx up --remote: shutting down");
            let _ = child.start_kill();
            let _ = child.wait().await;
        }
    }
    Ok(())
}

/// Turn the `--remote` value into an [`SshTarget`], supporting a trailing
/// `:PORT` so boxes on a non-standard SSH port (RunPod / openresearch dev nodes,
/// reached as `root@1.2.3.4:38455`) work with no `~/.ssh/config` entry.
///
/// - `alias` / `user@host` (no port) → a bare alias: `~/.ssh/config` alone
///   decides everything, exactly as before.
/// - `user@<hostname>:PORT` → `-p PORT` only; the user's own config/known_hosts
///   still govern host-key checking, since a name they typed may be one they've
///   pinned. We must not silently weaken verification for it.
/// - `user@<ip>:PORT` → `-p PORT` plus `StrictHostKeyChecking=accept-new`
///   (trust-on-first-use against the real `known_hosts`). A raw IP is the
///   freshly-provisioned-box case (RunPod/openresearch): nothing is pinned yet,
///   so first-use auto-accept lets `orx up --remote` connect without a prompt,
///   while a later key change is still caught. Caveat: if a provider recycles
///   that IP:port onto a *different* box, the pin now mismatches and ssh refuses
///   until the user runs `ssh-keygen -R`; that loud failure is the safe choice
///   over silently trusting whatever answers.
///
/// A trailing `:PORT` is only recognized when it's `1..=65535` and the address
/// isn't a raw (unbracketed) IPv6 literal — so IPv6 hosts and aliases containing
/// colons are left untouched rather than mis-split into host + bogus port.
fn parse_remote_target(host: &str) -> SshTarget {
    match split_host_port(host) {
        Some((dest, port)) => {
            let policy = if host_is_ip_literal(&dest) {
                HostKeyPolicy::AcceptNew
            } else {
                HostKeyPolicy::UserConfig
            };
            SshTarget::host_port(dest, port, policy)
        }
        None => SshTarget::alias(host),
    }
}

/// `(host, PORT)` when `host` ends in `:<port>` with `port` in `1..=65535` and
/// `host` is not a raw (unbracketed) IPv6 literal. Returns `None` for aliases,
/// bare `user@host`, `host:0`, out-of-range ports, and `2001:db8::1`-style
/// addresses, so those keep their exact prior behavior instead of being
/// mis-parsed into a host + spurious port.
fn split_host_port(host: &str) -> Option<(String, u16)> {
    let (head, tail) = host.rsplit_once(':')?;
    // Reject if the port isn't purely numeric and in range (0 and >65535 are
    // not usable ports, so treat them as "no port here", not a silent -p 0).
    if tail.is_empty() || !tail.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let port = tail.parse::<u16>().ok().filter(|&p| p != 0)?;
    // Disambiguate IPv6: a `:` remaining in the host means the split colon was
    // *inside* an address (raw `2001:db8::1`, or an unclosed `[…`), not a port
    // separator. Only a bracketed literal (`[2001:db8::1]`) is a valid host half
    // here — mirroring how ssh requires brackets for an IPv6 host with a port.
    // Check the part *after* any `user@`, so `user@[::1]:2222` isn't mistaken for
    // an unbracketed literal (its `head` starts with `user@`, not `[`).
    let host_only = strip_user(head);
    let bracketed_v6 = host_only.starts_with('[') && host_only.ends_with(']');
    if host_only.contains(':') && !bracketed_v6 {
        return None;
    }
    Some((head.to_string(), port))
}

/// The host portion of a destination, dropping an optional leading `user@`.
fn strip_user(dest: &str) -> &str {
    dest.rsplit_once('@').map_or(dest, |(_, h)| h)
}

/// Whether `dest` (an alias or `user@host`) resolves to a bare IP literal — the
/// signal that this is a freshly-provisioned box (nothing pinned) rather than a
/// hostname the user may already trust. Strips an optional `user@` and matched
/// IPv6 brackets before parsing.
fn host_is_ip_literal(dest: &str) -> bool {
    use std::net::IpAddr;
    let host = strip_user(dest);
    // Strip brackets only as a matched pair, so a malformed one-sided `[…`
    // doesn't parse as an IP.
    let host = match (host.strip_prefix('['), host.strip_suffix(']')) {
        (Some(_), Some(_)) => &host[1..host.len() - 1],
        _ => host,
    };
    host.parse::<IpAddr>().is_ok()
}

/// The remote command: start `orx up` bound to the remote's loopback, no
/// browser there (we open ours), on the port we forward.
fn remote_up_cmd(port: u16) -> String {
    format!("orx up --no-browser --port {port}")
}

/// The `-L` forward value. Local bind pinned to `127.0.0.1` (see the call site).
fn forward_spec(port: u16) -> String {
    format!("127.0.0.1:{port}:localhost:{port}")
}

/// The full ssh argv (minus the `ssh` program name). Pure, so it can be tested.
fn ssh_forward_args(target: &SshTarget, forward: &str, remote_cmd: &str) -> Vec<String> {
    let mut args = base_ssh_opts();
    args.push("-L".into());
    args.push(forward.into());
    args.extend(target.extra_opts.iter().cloned());
    args.push("--".into());
    args.push(target.dest.clone());
    args.push(remote_cmd.into());
    args
}

/// Spawn `ssh <opts> -L <forward> -- <dest> <remote_cmd>` with the shared
/// connection options, detaching stdin so ssh never blocks on a password prompt.
fn spawn_ssh_forward(target: &SshTarget, forward: &str, remote_cmd: &str) -> Result<Child> {
    let mut cmd = Command::new("ssh");
    cmd.args(ssh_forward_args(target, forward, remote_cmd))
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    cmd.spawn().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            anyhow!("`ssh` not found on PATH — remote access needs the OpenSSH client.")
        } else {
            anyhow!("could not run ssh: {e}")
        }
    })
}

/// Connection options for the tunnel. Notable choices:
/// - `ExitOnForwardFailure=yes`: if the local port is already taken (e.g. your
///   own `orx up` on the same port), ssh exits instead of running the remote
///   server anyway and leaving us to health-check the *wrong* local server.
/// - `BatchMode=yes`: never prompt — fail fast rather than hang.
/// - keepalives so a silent forward isn't reaped by a NAT/idle timeout.
///
/// We don't multiplex here (this is a long-lived foreground session, not the
/// job backend's many short polls), so no ControlMaster.
fn base_ssh_opts() -> Vec<String> {
    [
        "ExitOnForwardFailure=yes",
        "BatchMode=yes",
        "ConnectTimeout=10",
        "ServerAliveInterval=30",
        "ServerAliveCountMax=3",
    ]
    .iter()
    .flat_map(|o| ["-o".to_string(), (*o).to_string()])
    .collect()
}

/// Poll the forwarded `/api/health` until the remote server answers, giving up
/// after [`HEALTH_TIMEOUT`] or if the ssh child exits first.
async fn wait_healthy(child: &mut Child, port: u16) -> Result<()> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()?;
    let url = format!("http://127.0.0.1:{port}/api/health");
    let deadline = Instant::now() + HEALTH_TIMEOUT;
    loop {
        if let Some(status) = child.try_wait()? {
            return Err(anyhow!(
                "ssh exited before the remote server came up ({status}). \
                 Is `orx` runnable on the remote's non-interactive shell?"
            ));
        }
        if let Ok(resp) = client.get(&url).send().await {
            if resp.status().is_success() {
                return Ok(());
            }
        }
        if Instant::now() >= deadline {
            return Err(anyhow!(
                "remote orx up didn't answer on the forwarded port {port} within {}s.",
                HEALTH_TIMEOUT.as_secs()
            ));
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forward_and_remote_cmd_use_the_same_port() {
        assert_eq!(forward_spec(4899), "127.0.0.1:4899:localhost:4899");
        assert_eq!(remote_up_cmd(4899), "orx up --no-browser --port 4899");
    }

    #[test]
    fn base_opts_exit_on_forward_failure() {
        // Without this, a busy local port lets ssh run the remote server anyway
        // and we'd health-check the wrong (local) server. Load-bearing.
        let opts = base_ssh_opts();
        let joined = opts.join(" ");
        assert!(joined.contains("-o ExitOnForwardFailure=yes"));
        assert!(joined.contains("-o BatchMode=yes"));
    }

    #[test]
    fn ssh_args_are_ordered_opts_then_forward_then_dest_then_cmd() {
        let target = SshTarget::alias("mybox");
        let args = ssh_forward_args(&target, "127.0.0.1:7:localhost:7", "orx up");
        // -L and its value are adjacent and precede the `--` separator.
        let l = args.iter().position(|a| a == "-L").unwrap();
        assert_eq!(args[l + 1], "127.0.0.1:7:localhost:7");
        let sep = args.iter().position(|a| a == "--").unwrap();
        assert!(l < sep, "-L must come before --");
        // dest then the remote command follow the separator, in that order.
        assert_eq!(args[sep + 1], "mybox");
        assert_eq!(args[sep + 2], "orx up");
    }

    #[test]
    fn extra_opts_land_before_the_separator() {
        let target = SshTarget {
            dest: "mybox".into(),
            extra_opts: vec!["-p".into(), "2222".into()],
        };
        let args = ssh_forward_args(&target, "127.0.0.1:7:localhost:7", "orx up");
        let sep = args.iter().position(|a| a == "--").unwrap();
        let p = args.iter().position(|a| a == "-p").unwrap();
        assert!(p < sep, "extra_opts must precede the -- separator");
        assert_eq!(args[p + 1], "2222");
    }

    #[test]
    fn bare_alias_and_userhost_stay_plain_aliases() {
        // No port, no imposed opts — `~/.ssh/config` keeps full control.
        for h in ["mybox", "root@example.com"] {
            let t = parse_remote_target(h);
            assert_eq!(t.dest, h);
            assert!(t.extra_opts.is_empty(), "{h} should get no extra opts");
        }
    }

    #[test]
    fn ip_with_port_gets_accept_new_tofu_not_devnull() {
        // The RunPod / openresearch case: root@<ip>:38455 → -p 38455 plus genuine
        // trust-on-first-use (accept-new against real known_hosts). It must NOT
        // use UserKnownHostsFile=/dev/null (that's accept-every-time, reserved
        // for provider-managed boxes).
        let t = parse_remote_target("root@38.128.232.245:38455");
        assert_eq!(t.dest, "root@38.128.232.245");
        let joined = t.extra_opts.join(" ");
        assert!(joined.contains("-p 38455"));
        assert!(joined.contains("StrictHostKeyChecking=accept-new"));
        assert!(
            !joined.contains("/dev/null"),
            "user-typed host must keep its real known_hosts"
        );
        // And it flows all the way into the ssh argv, before the `--`.
        let args = ssh_forward_args(&t, "127.0.0.1:7:localhost:7", "orx up");
        let sep = args.iter().position(|a| a == "--").unwrap();
        let p = args.iter().position(|a| a == "-p").unwrap();
        assert!(p < sep && args[p + 1] == "38455");
        assert_eq!(args[sep + 1], "root@38.128.232.245");
    }

    #[test]
    fn hostname_with_port_gets_dash_p_only_no_hostkey_override() {
        // A *name* the user typed may be one they've pinned — appending :PORT
        // must not silently downgrade host-key verification. Only `-p` is added.
        let t = parse_remote_target("root@example.com:2222");
        assert_eq!(t.dest, "root@example.com");
        assert_eq!(t.extra_opts, vec!["-p".to_string(), "2222".to_string()]);
    }

    #[test]
    fn ipv6_and_odd_trailing_colons_are_not_treated_as_ports() {
        // Bracketed IPv6 without a port: colon is inside the literal.
        assert_eq!(parse_remote_target("[::1]").dest, "[::1]");
        assert!(parse_remote_target("[::1]").extra_opts.is_empty());
        // RAW (unbracketed) IPv6 must NOT be mis-split into host + port.
        assert_eq!(parse_remote_target("2001:db8::5").dest, "2001:db8::5");
        assert!(parse_remote_target("2001:db8::5").extra_opts.is_empty());
        // A trailing colon with no digits, non-numeric, or :0 isn't a port.
        assert!(parse_remote_target("host:").extra_opts.is_empty());
        assert!(parse_remote_target("host:abc").extra_opts.is_empty());
        assert!(parse_remote_target("host:0").extra_opts.is_empty());
        assert_eq!(parse_remote_target("host:0").dest, "host:0");
        // >u16 falls through to a bare alias (ssh will surface the bad host).
        assert!(parse_remote_target("host:99999").extra_opts.is_empty());
        // Bracketed IPv6 *with* a port is honored (and an IP literal → accept-new).
        let t = parse_remote_target("[2001:db8::1]:2222");
        assert_eq!(t.dest, "[2001:db8::1]");
        let joined = t.extra_opts.join(" ");
        assert!(joined.contains("-p 2222"));
        assert!(joined.contains("StrictHostKeyChecking=accept-new"));
    }

    #[test]
    fn user_prefixed_bracketed_ipv6_keeps_its_port() {
        // Regression: `user@[::1]:2222` must NOT drop the port (the bracket check
        // has to look past the `user@` prefix, or it silently connects on :22).
        let t = parse_remote_target("root@[::1]:2222");
        assert_eq!(t.dest, "root@[::1]");
        let joined = t.extra_opts.join(" ");
        assert!(joined.contains("-p 2222"), "port must survive: {joined}");
        // `[::1]` is a loopback IP literal → accept-new TOFU.
        assert!(joined.contains("StrictHostKeyChecking=accept-new"));
    }

    #[test]
    fn host_is_ip_literal_classifies_forms() {
        // IPs in every shape we can be handed → true.
        for d in [
            "1.2.3.4",
            "root@1.2.3.4",
            "[2001:db8::1]",
            "root@[::1]",
            "::ffff:1.2.3.4",
        ] {
            assert!(host_is_ip_literal(d), "{d} should be an IP literal");
        }
        // Hostnames (incl. a numeric-looking one) → false.
        for d in ["example.com", "root@example.com", "1.2.3.4.example.com"] {
            assert!(!host_is_ip_literal(d), "{d} should be a hostname");
        }
        // A malformed one-sided bracket must NOT parse as an IP.
        assert!(!host_is_ip_literal("[1.2.3.4"));
    }
}
