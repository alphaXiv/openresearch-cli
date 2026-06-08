//! The `dev <open|close|status>` command.

use std::time::{Duration, Instant};

use crate::client::{dev_close, dev_open, dev_status, DevCloseBody, DevSessionState};
use crate::error::require_credentials;
use crate::error::{anyhow, Result};

const POLL_INTERVAL_MS: u64 = 3000;
const PROVISION_TIMEOUT_MS: u64 = 5 * 60 * 1000;

fn sandbox_label(id: &Option<String>) -> String {
    id.clone().unwrap_or_default()
}

pub async fn run(args: crate::DevArgs) -> Result<()> {
    let sub = args.action.as_str();
    // clap restricts action to open|close|status, but mirror the TS guard.
    if sub != "open" && sub != "close" && sub != "status" {
        eprintln!("Usage: orx dev <open|close|status> <experimentId>");
        std::process::exit(1);
    }

    let creds = require_credentials().await;
    let exp_id = args.exp_id.as_str();

    if sub == "status" {
        let s = dev_status(&creds, exp_id).await?;
        let suffix = match &s.sandbox_id {
            Some(id) => format!("  ({})", id),
            None => String::new(),
        };
        println!("state: {}{}", state_str(s.state), suffix);
        if s.state == DevSessionState::Online {
            if s.dirty.is_empty() {
                println!("working tree clean");
            } else {
                println!("{} uncommitted change(s):", s.dirty.len());
                for line in &s.dirty {
                    println!("  {}", line);
                }
            }
        }
        return Ok(());
    }

    if sub == "close" {
        let body = DevCloseBody {
            message: args.message.clone(),
            discard: if args.discard { Some(true) } else { None },
        };
        let res = dev_close(&creds, exp_id, &body).await?;
        if !res.torn_down {
            println!("No dev node was open.");
            return Ok(());
        }
        // A non-discard close that commits nothing means the session's edits
        // never landed (e.g. a str-replace that no-op'd). The node is still torn
        // down, but we surface this loudly and exit non-zero so callers chaining
        // open/edit/close don't mistake an empty edit for success.
        let mut empty_edit = false;
        if res.committed {
            let sha = match &res.commit_sha {
                Some(sha) => {
                    let short: String = sha.chars().take(7).collect();
                    format!(" ({})", short)
                }
                None => String::new(),
            };
            println!("\u{2713} Committed & pushed{}.", sha);
        } else if args.discard {
            println!("Discarded changes.");
        } else {
            empty_edit = true;
        }
        println!("\u{2713} Dev node torn down.");
        if empty_edit {
            eprintln!(
                "\u{26a0} Nothing to commit \u{2014} the working tree was clean. \
                 No edits landed this session (did a str-replace/write no-op?). \
                 Use `--discard` if an empty close was intended."
            );
            std::process::exit(1);
        }
        return Ok(());
    }

    // open: provision (or reuse), then poll until the node is online.
    let opened = dev_open(&creds, exp_id).await?;
    if opened.state == DevSessionState::Online {
        println!(
            "\u{2713} Dev node ready ({}).",
            sandbox_label(&opened.sandbox_id)
        );
        return Ok(());
    }

    use std::io::Write;
    print!("Provisioning dev node");
    std::io::stdout().flush().ok();

    let deadline = Instant::now() + Duration::from_millis(PROVISION_TIMEOUT_MS);
    while Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(POLL_INTERVAL_MS)).await;
        print!(".");
        std::io::stdout().flush().ok();
        let s = dev_status(&creds, exp_id).await?;
        if s.state == DevSessionState::Online {
            println!();
            std::io::stdout().flush().ok();
            println!(
                "\u{2713} Dev node ready ({}).",
                sandbox_label(&s.sandbox_id)
            );
            return Ok(());
        }
        if s.state == DevSessionState::None || s.state == DevSessionState::Offline {
            println!();
            std::io::stdout().flush().ok();
            eprintln!("Provisioning failed (state: {}).", state_str(s.state));
            std::process::exit(1);
        }
    }
    println!();
    std::io::stdout().flush().ok();
    eprintln!("Timed out waiting for the dev node to come online.");
    std::process::exit(1);

    #[allow(unreachable_code)]
    Err(anyhow!("unreachable"))
}

fn state_str(state: DevSessionState) -> &'static str {
    match state {
        DevSessionState::None => "none",
        DevSessionState::Provisioning => "provisioning",
        DevSessionState::Online => "online",
        DevSessionState::Offline => "offline",
    }
}
