//! `orx plan-gate` — the Claude plan-mode `PreToolUse` hook body.
//!
//! Hidden from `--help`: it's not a user command, it's the executable the
//! plan-mode settings file points its `PreToolUse` hook at (see
//! `local::harness::claude::write_plan_settings`). Claude Code pipes the hook
//! payload in on stdin; we print an `allow` decision for read-only `orx`
//! inspection and stay silent otherwise, letting plan mode gate everything else.
//!
//! Always exits 0: a hook error must not block the turn, and "no decision" is
//! expressed as empty stdout, not a failure.

use std::io::Read;

use crate::error::Result;
use crate::local::harness::plan_gate_decide;

pub async fn run() -> Result<()> {
    let mut input = String::new();
    // A read error just means "no decision" — defer to plan mode's gating.
    if std::io::stdin().read_to_string(&mut input).is_err() {
        return Ok(());
    }
    if let Ok(payload) = serde_json::from_str::<serde_json::Value>(&input) {
        if let Some(decision) = plan_gate_decide(&payload) {
            println!("{decision}");
        }
    }
    Ok(())
}
