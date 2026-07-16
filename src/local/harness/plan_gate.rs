//! Claude plan-mode gate for `orx` shell commands.
//!
//! Claude Code's `--permission-mode plan` auto-approves its built-in read-only
//! tools (file reads, `grep`, `git log`, …) but treats an arbitrary `Bash(orx …)`
//! as a write it must gate — and headless `--print` can't answer that prompt, so
//! the call just fails. That breaks planning, because the agent plans by
//! *inspecting* prior runs, logs, and the evidence DB via read-only `orx`
//! subcommands.
//!
//! The fix is a `PreToolUse` hook (wired only in plan mode — see
//! `claude::write_plan_settings`) that runs `orx plan-gate`: it reads the hook's
//! JSON off stdin, and if the tool is a Bash call invoking a *read-only* `orx`
//! subcommand it prints an `"allow"` decision so the command runs. For anything
//! else — a write/launch `orx` verb (`exp run`, `instance`, `create-*`, …), a
//! non-`orx` command, or an unparseable one — it stays silent (exit 0, no
//! stdout), so plan mode's normal gating applies and launches remain blocked
//! until the user approves the plan.
//!
//! Classification is deliberately *allowlist-only*: an unknown or ambiguous
//! subcommand is treated as NOT read-only (gated), so a newly added write verb
//! can never leak through by default. The flip side is a maintenance
//! obligation: the allowlist is a hand-kept mirror of the read-only `orx` verbs
//! in `main.rs`'s `Command` enum. A newly added *read* verb stays gated until
//! it's added here — `readonly_verbs_are_real_commands` guards against a rename
//! silently un-gating nothing, but adding a new read verb is a manual step
//! (there's a pointer comment on the `Command` enum).

use serde_json::{json, Value};

/// Top-level `orx` verbs that are read-only in every form (no subcommand can
/// turn them into a write). Kept in lockstep with `main.rs`'s `Command` enum;
/// `readonly_verbs_are_real_commands` guards against a rename.
const WHOLE_VERB_READS: &[&str] = &[
    "projects",
    "explore",
    "experiments",
    "env",
    "runs",
    "logs",
    "search-logs",
    "artifacts",
    "artifact",
    "wandb",
    "query",
    "chart",
    "compute",
    "lit",
    "paper",
    "skill",
    "version",
];

/// Decide whether a `PreToolUse` hook payload describes a read-only `orx`
/// invocation that plan mode should let through. Returns the JSON to print on
/// stdout (an `allow` decision) or `None` to stay silent and defer to plan
/// mode's default gating.
///
/// The payload shape is Claude Code's hook contract: `tool_name` names the tool
/// and `tool_input.command` carries the Bash command line.
pub fn decide(payload: &Value) -> Option<Value> {
    // Only Bash calls carry a shell command to inspect; every other tool defers.
    if payload.get("tool_name").and_then(Value::as_str) != Some("Bash") {
        return None;
    }
    let command = payload
        .pointer("/tool_input/command")
        .and_then(Value::as_str)?;

    if !is_readonly_orx(command) {
        return None;
    }

    Some(json!({
        "hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecision": "allow",
            "permissionDecisionReason":
                "read-only orx inspection is allowed during plan mode",
        }
    }))
}

/// True iff `command` is a single `orx` invocation whose (sub)command is on the
/// read-only allowlist. Anything with shell metacharacters that could chain a
/// second command (`;`, `&&`, `|`, `` ` ``, `$(`, redirection) is rejected — a
/// read-only prefix must not be a smuggling vector for a write.
fn is_readonly_orx(command: &str) -> bool {
    let command = command.trim();
    if command.contains([';', '|', '&', '`', '>', '<', '\n']) || command.contains("$(") {
        return false;
    }

    // Tokenize on whitespace; the wrapper never quotes the verb itself, so a
    // simple split is enough to read the leading `orx <verb> [<sub>]`.
    let mut tokens = command.split_whitespace();

    // The binary must be `orx` (bare or a path ending in `/orx`). Reject a
    // leading env-assignment or a different program.
    match tokens.next() {
        Some(bin) if bin == "orx" || bin.ends_with("/orx") => {}
        _ => return false,
    }

    // First non-flag token after the binary is the top-level verb.
    let verb = tokens.by_ref().find(|t| !t.starts_with('-'));
    let Some(verb) = verb else {
        // Bare `orx` (or only flags): prints usage — harmless and read-only.
        return true;
    };

    if WHOLE_VERB_READS.contains(&verb) {
        // No subcommand can turn these into a write.
        return true;
    }

    match verb {
        // Verbs with a write subcommand: allow only the read-only subcommand(s).
        // The next non-flag token is the subcommand.
        "project" => matches!(subcommand(&mut tokens), Some("view")),
        "exp" => match subcommand(&mut tokens) {
            // Pure reads.
            Some("status" | "wait") => true,
            // `desc`/`cmd` view the node's notes / run command, but *write* them
            // with `--set`/`--stdin`. Allow only the read (view) form. The flags
            // only ever appear on the write form, so a whole-command scan is a
            // sound discriminator (and `desc --set` etc. are the only writers).
            Some("desc" | "cmd") => !command.contains("--set") && !command.contains("--stdin"),
            _ => false,
        },
        "report" => matches!(subcommand(&mut tokens), Some("list" | "show" | "download")),

        // Everything else — create-*, instance, login/logout, install-skills,
        // update, serve, supervise, up, and any unrecognized future verb — is
        // gated. Allowlist-only: unknown ⇒ not read-only.
        _ => false,
    }
}

/// The next non-flag token, read as a subcommand name.
fn subcommand<'a>(tokens: &mut impl Iterator<Item = &'a str>) -> Option<&'a str> {
    tokens.find(|t| !t.starts_with('-'))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn payload(command: &str) -> Value {
        json!({ "tool_name": "Bash", "tool_input": { "command": command } })
    }

    fn allowed(command: &str) -> bool {
        decide(&payload(command)).is_some()
    }

    #[test]
    fn whole_verb_reads_are_allowed() {
        for c in [
            "orx runs",
            "orx logs r-123 --tail 200",
            "orx query \"select 1\"",
            "orx experiments",
            "orx artifacts r-9",
            "orx search-logs foo",
            "orx wandb r-1",
            "orx chart loss",
            "orx compute",
            "orx lit transformers",
            "orx paper 2301.00001",
            "orx skill",
            "orx projects --json",
            "/usr/local/bin/orx runs",
            "orx", // bare usage
        ] {
            assert!(allowed(c), "should allow: {c}");
        }
    }

    #[test]
    fn read_only_subcommands_are_allowed() {
        assert!(allowed("orx project view p-1"));
        assert!(allowed("orx exp status e-1"));
        // The playbook's core orientation reads (view form).
        assert!(allowed("orx exp desc e-1"));
        assert!(allowed("orx exp cmd e-1"));
        assert!(allowed("orx exp wait e-1"));
        assert!(allowed("orx report list"));
        assert!(allowed("orx report show r-1"));
        assert!(allowed("orx report download r-1 --out x.md"));
    }

    #[test]
    fn write_subcommands_are_gated() {
        // Same verb, write subcommand → not allowed (plan mode gates it).
        assert!(!allowed("orx project edit p-1 --name x"));
        assert!(!allowed("orx exp run e-1"));
        assert!(!allowed("orx exp cancel e-1"));
        assert!(!allowed("orx report upload r-1 --file x.md"));
        // `desc`/`cmd` become writes with --set/--stdin → gated.
        assert!(!allowed("orx exp desc e-1 --set \"found X\""));
        assert!(!allowed("orx exp desc e-1 --stdin"));
        assert!(!allowed("orx exp cmd e-1 --set \"python train.py\""));
    }

    #[test]
    fn launch_and_write_verbs_are_gated() {
        for c in [
            "orx exp run e-1 --backend hf",
            "orx instance create --gpu a100",
            "orx create-project --repo x/y",
            "orx create-experiment --parent e-1",
            "orx login",
            "orx logout",
            "orx install-skills",
            "orx update",
            "orx up",
            "orx serve",
        ] {
            assert!(!allowed(c), "should gate: {c}");
        }
    }

    #[test]
    fn unknown_verb_is_gated() {
        // Allowlist-only: a future verb we haven't classified defaults to gated.
        assert!(!allowed("orx teleport e-1"));
    }

    #[test]
    fn non_orx_commands_defer() {
        assert!(!allowed("rm -rf /"));
        assert!(!allowed("git push"));
        assert!(!allowed("python train.py"));
        // A program that merely ends in text containing orx is not `orx`.
        assert!(!allowed("neworx runs"));
    }

    #[test]
    fn command_chaining_is_rejected() {
        // A read-only prefix must not smuggle a second command through.
        assert!(!allowed("orx runs; orx exp run e-1"));
        assert!(!allowed("orx runs && rm -rf /"));
        assert!(!allowed("orx runs | tee /etc/passwd"));
        assert!(!allowed("orx logs r-1 > /tmp/x"));
        assert!(!allowed("orx query \"$(rm -rf /)\""));
        assert!(!allowed("orx runs `whoami`"));
    }

    #[test]
    fn non_bash_tools_defer() {
        let p = json!({ "tool_name": "Edit", "tool_input": { "command": "orx runs" } });
        assert!(decide(&p).is_none());
        // Missing command field → defer, not panic.
        let p = json!({ "tool_name": "Bash", "tool_input": {} });
        assert!(decide(&p).is_none());
    }

    #[test]
    fn allow_decision_has_the_exact_wire_shape() {
        let out = decide(&payload("orx runs")).unwrap();
        assert_eq!(
            out.pointer("/hookSpecificOutput/hookEventName")
                .and_then(Value::as_str),
            Some("PreToolUse")
        );
        assert_eq!(
            out.pointer("/hookSpecificOutput/permissionDecision")
                .and_then(Value::as_str),
            Some("allow")
        );
    }

    /// Drift guard: every whole-verb read on the allowlist must still name a
    /// real top-level `orx` command. A rename in `main.rs` breaks this test
    /// (instead of silently un-gating nothing). Catching *additions* of new read
    /// verbs is the human's job — see the pointer comment on the `Command` enum.
    #[test]
    fn readonly_verbs_are_real_commands() {
        use clap::CommandFactory;
        let cmd = crate::Cli::command();
        let real: std::collections::HashSet<&str> =
            cmd.get_subcommands().map(|s| s.get_name()).collect();
        for verb in WHOLE_VERB_READS {
            assert!(
                real.contains(verb),
                "allowlisted read verb `{verb}` is not a real orx command \
                 (renamed in main.rs?)"
            );
        }
        // The verbs with mixed read/write subcommands must also be real.
        for verb in ["project", "exp", "report"] {
            assert!(real.contains(verb), "`{verb}` is not a real orx command");
        }
    }
}
