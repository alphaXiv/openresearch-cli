//! Claude plan-mode gate for read-only shell commands.
//!
//! Claude Code's `--permission-mode plan` auto-approves its built-in read-only
//! tools (file reads, `grep`, …) but treats an arbitrary `Bash(orx …)` as a
//! write it must gate — and headless `--print` can't answer that prompt, so the
//! call just fails. That breaks planning, because the agent plans by
//! *inspecting* prior runs, logs, and the evidence DB via read-only `orx`
//! subcommands — and by reading experiment code that often lives only on git
//! branches (`git show <ref>:<file>`, `git ls-tree`), typically piped through
//! `head`/`grep` with a `2>&1`.
//!
//! The fix is a `PreToolUse` hook (wired only in plan mode — see
//! `claude::write_plan_settings`) that runs `orx plan-gate`: it reads the hook's
//! JSON off stdin, and if the tool is a Bash call invoking a *read-only*
//! command it prints an `"allow"` decision so the command runs. For anything
//! else — a write/launch `orx` verb (`exp run`, `instance`, `create-*`, …), a
//! git write, an unknown program, or an unparseable command — it stays silent
//! (exit 0, no stdout), so plan mode's normal gating applies and launches
//! remain blocked until the user approves the plan.
//!
//! Classification is deliberately *allowlist-only*: an unknown or ambiguous
//! command is treated as NOT read-only (gated), so a newly added write verb
//! can never leak through by default. The flip side is a maintenance
//! obligation: the `orx` allowlist is a hand-kept mirror of the read-only
//! verbs in `main.rs`'s `Command` enum. A newly added *read* verb stays gated
//! until it's added here — `readonly_verbs_are_real_commands` guards against a
//! rename silently un-gating nothing, but adding a new read verb is a manual
//! step (there's a pointer comment on the `Command` enum).

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

/// Shell no-ops allowed as glue between read-only segments in a batch —
/// separators and labels the planning agent prints, e.g. `echo ====`. They take
/// arbitrary args but can't have a side effect here (redirection/substitution
/// are rejected before we get this far), so a program-name match is enough.
const READONLY_GLUE: &[&str] = &["echo", "true", ":"];

/// Programs a read-only producer may pipe into: pure stream consumers that
/// never write without a redirect (and redirects are rejected per-token).
/// Deliberately excluded: `tee` (writes files), `xargs` (spawns commands),
/// `sed`/`awk` (in-place edits, `w`/`system()` escape hatches).
const PURE_CONSUMERS: &[&str] = &[
    "head", "tail", "grep", "egrep", "fgrep", "wc", "cat", "sort", "uniq", "cut", "tr", "nl",
    "column",
];

/// `git` verbs that are read-only in every arg form, subject to the argument
/// guards in [`is_readonly_git`] (`--output` writes a file from `log`/`show`/
/// `diff`; `-O`/`--open-files-in-pager` executes a pager from `grep`).
/// Verbs with mixed read/write forms (`branch`, `tag`, `stash`, `remote`,
/// `worktree`, `reflog`) are handled conditionally; `config` is excluded
/// entirely (its read/write grammar is too fiddly to classify safely).
const GIT_WHOLE_VERB_READS: &[&str] = &[
    "status",
    "log",
    "show",
    "diff",
    "grep",
    "ls-tree",
    "ls-files",
    "rev-parse",
    "rev-list",
    "cat-file",
    "blame",
    "shortlog",
    "describe",
    "name-rev",
    "merge-base",
    "show-ref",
    "for-each-ref",
    "count-objects",
    "diff-tree",
    "whatchanged",
];

/// Decide whether a `PreToolUse` hook payload describes a read-only command
/// that plan mode should let through. Returns the JSON to print on stdout (an
/// `allow`/`ask` decision) or `None` to stay silent and defer to plan mode's
/// default gating.
///
/// The payload shape is Claude Code's hook contract: `tool_name` names the tool
/// and `tool_input.command` carries the Bash command line.
///
/// ExitPlanMode gets an explicit `ask`: headless plan mode **self-approves**
/// the call otherwise ("User has approved exiting plan mode", nobody asked —
/// verified on claude 2.1.197), letting the model exit plan mode and edit
/// files without the user's say. `ask` routes it to the permission prompt tool
/// (the `orx mcp-gate` bridge), which surfaces the plan card and blocks until
/// the user answers; without a bridge configured, `ask` in headless denies —
/// still strictly better than self-approval.
pub fn decide(payload: &Value) -> Option<Value> {
    let tool_name = payload.get("tool_name").and_then(Value::as_str)?;

    if tool_name == "ExitPlanMode" {
        return Some(json!({
            "hookSpecificOutput": {
                "hookEventName": "PreToolUse",
                "permissionDecision": "ask",
                "permissionDecisionReason":
                    "plan approval is the user's decision",
            }
        }));
    }

    // Only Bash calls carry a shell command to inspect; every other tool defers.
    if tool_name != "Bash" {
        return None;
    }
    let command = payload
        .pointer("/tool_input/command")
        .and_then(Value::as_str)?;

    if !command_is_readonly(command) {
        return None;
    }

    Some(json!({
        "hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecision": "allow",
            "permissionDecisionReason":
                "read-only inspection is allowed during plan mode",
        }
    }))
}

/// True iff `command` is a read-only inspection that plan mode may run.
///
/// A single read-only invocation (`orx <read-verb>`, `git <read-verb>`, a pure
/// consumer, or glue) is allowed. So is a *sequence* of them joined by `;` or
/// `&&` — the batching the planning agent uses to fetch several nodes in one
/// call, e.g. `orx exp desc A; echo ====; orx exp desc B` — and a *pipeline*
/// whose first stage is a read-only producer and every later stage a pure
/// consumer, e.g. `git log --oneline | head -20` or `orx runs 2>&1 | grep err`.
/// The whole line is allowed only if **every** segment and **every** pipeline
/// stage independently passes; one unknown or write stage gates the entire
/// command (allowlist-only). This keeps the security property: a read-only
/// prefix can never smuggle a write through as a later segment or stage.
///
/// Only `;` and `&&` are recognized as separators and `|` as a pipe.
/// Redirection (`>`/`<`), backticks, `$(…)`, background `&`, and newlines are
/// rejected — with one carve-out: a standalone `2>&1` token (stderr-merge into
/// the pipe) is dropped before the check, since agents habitually write
/// `cmd 2>&1 | head` and the merge itself has no side effect.
///
/// The split is not quote-aware, so a separator *inside* a quoted argument
/// (e.g. `orx query "select … where a && b"`) is still treated as a separator
/// and gates the line. This over-gates rather than under-allows — it fails
/// safe — so such a command just falls back to plan mode's normal gate; run it
/// outside a batch to have it auto-allowed.
///
/// `pub` because the MCP permission bridge reuses the same classifier for its
/// auto-allow policy.
pub fn command_is_readonly(command: &str) -> bool {
    let command = command.trim();

    // Metacharacters no per-stage scan can make safe: command substitution can
    // run anything even inside an argument, `<` reads arbitrary files into a
    // command we didn't classify, and multi-line scripts defeat the splitter.
    if command.contains(['`', '<', '\n']) || command.contains("$(") {
        return false;
    }

    // Split on the two sequencing separators. Splitting on `&&` first, then `;`,
    // yields the individual segments; each must stand on its own as read-only.
    command
        .split("&&")
        .flat_map(|part| part.split(';'))
        .all(is_readonly_segment)
}

/// True iff a single command segment (no `;`/`&&` separators) is a read-only
/// pipeline: a read-only producer optionally piped through pure consumers.
/// Empty/whitespace segments (from a leading/trailing/doubled separator) are
/// shell no-ops and allowed, matching the shell's own tolerance for `orx runs;`.
fn is_readonly_segment(segment: &str) -> bool {
    let segment = segment.trim();
    if segment.is_empty() {
        return true;
    }

    let mut stages = segment.split('|');
    // `split` always yields at least one item.
    let first = stages.next().unwrap_or_default();
    if !is_readonly_producer(first) {
        return false;
    }
    // Every later stage must be a pure consumer. An empty stage means `||`,
    // `| |`, or a trailing pipe — never legitimate read-only batching.
    stages.all(|stage| match stage_tokens(stage) {
        Some(tokens) if !tokens.is_empty() => is_pure_consumer(&tokens),
        _ => false,
    })
}

/// Tokenize one pipeline stage. Drops standalone `2>&1` tokens (a harmless
/// stderr-merge), then rejects the stage (`None`) if any remaining token still
/// carries `>` or `&` — the segment split consumed every `&&`, so a surviving
/// `&` is background execution or a malformed `&&&`, and any `>` is
/// redirection.
fn stage_tokens(stage: &str) -> Option<Vec<&str>> {
    let mut tokens = Vec::new();
    for token in stage.split_whitespace() {
        if token == "2>&1" {
            continue;
        }
        if token.contains(['>', '&']) {
            return None;
        }
        tokens.push(token);
    }
    Some(tokens)
}

/// True iff a pipeline's first stage is read-only: glue, a read-only `orx`
/// invocation, a read-only `git` invocation, or a pure consumer (so `wc -l f`
/// or `head -50 f` stand alone too).
fn is_readonly_producer(stage: &str) -> bool {
    let Some(tokens) = stage_tokens(stage) else {
        return false;
    };
    // The program is the first token. Reject a leading env-assignment or any
    // unlisted program.
    let Some(&program) = tokens.first() else {
        return false; // empty first stage: `| head` — not a real pipeline
    };
    if READONLY_GLUE.contains(&program) {
        return true;
    }
    if program == "orx" || program.ends_with("/orx") {
        return is_readonly_orx(&tokens, stage);
    }
    if program == "git" || program.ends_with("/git") {
        return is_readonly_git(&tokens);
    }
    is_pure_consumer(&tokens)
}

/// True iff the stage runs one of the [`PURE_CONSUMERS`]. Args are arbitrary:
/// redirection/substitution/background were already rejected per-token, and
/// none of these programs writes without a redirect.
fn is_pure_consumer(tokens: &[&str]) -> bool {
    match tokens.first() {
        Some(program) => PURE_CONSUMERS.contains(program),
        None => false,
    }
}

/// True iff a tokenized `orx …` invocation is read-only. `stage` is the raw
/// stage text, kept for the `--set`/`--stdin` substring scan (those flags only
/// ever appear on the write forms of `exp desc`/`exp cmd`, so a whole-stage
/// scan is a sound discriminator).
fn is_readonly_orx(tokens: &[&str], stage: &str) -> bool {
    let mut rest = tokens.iter().skip(1).copied();

    // First non-flag token after the binary is the top-level verb.
    let Some(verb) = subcommand(&mut rest) else {
        // Bare `orx` (or only flags): prints usage — harmless and read-only.
        return true;
    };

    if WHOLE_VERB_READS.contains(&verb) {
        // No subcommand can turn these into a write.
        return true;
    }

    match verb {
        // Verbs with a write subcommand: allow only the read-only subcommand(s).
        "project" => matches!(subcommand(&mut rest), Some("view")),
        "exp" => match subcommand(&mut rest) {
            // Pure reads.
            Some("status" | "wait") => true,
            // `desc`/`cmd` view the node's notes / run command, but *write*
            // them with `--set`/`--stdin`. Allow only the read (view) form.
            Some("desc" | "cmd") => !stage.contains("--set") && !stage.contains("--stdin"),
            _ => false,
        },
        "report" => matches!(subcommand(&mut rest), Some("list" | "show" | "download")),

        // Everything else — create-*, instance, login/logout, install-skills,
        // update, serve, supervise, up, and any unrecognized future verb — is
        // gated. Allowlist-only: unknown ⇒ not read-only.
        _ => false,
    }
}

/// True iff a tokenized `git …` invocation is read-only. Allowlist-only, with
/// argument guards on the escape hatches a read verb can carry.
fn is_readonly_git(tokens: &[&str]) -> bool {
    // Global flags between `git` and the verb: only the harmless pager/cwd
    // ones. Anything else — `-c key=val`, `--exec-path`, `--git-dir`,
    // `--config-env`, … — can change what git *executes* (pager, alias, and
    // hook overrides are arbitrary-command vectors), so it gates.
    let mut i = 1;
    while i < tokens.len() && tokens[i].starts_with('-') {
        match tokens[i] {
            "-P" | "--no-pager" => i += 1,
            "-C" => i += 2, // consumes its <path> argument
            _ => return false,
        }
    }
    let Some(&verb) = tokens.get(i) else {
        // Bare `git` prints usage, but there's no reason to batch it — gate.
        return false;
    };
    let args = &tokens[i + 1..];

    if GIT_WHOLE_VERB_READS.contains(&verb) {
        // The write escapes a read verb can carry: `--output[=<file>]` writes a
        // file from log/show/diff, and grep's `-O`/`--open-files-in-pager`
        // executes an arbitrary pager.
        return !args
            .iter()
            .any(|a| a.starts_with("--output") || a.starts_with("-O") || a.starts_with("--open-"));
    }

    // A positional arg on `branch`/`tag` creates unless a list-query flag makes
    // positionals mean patterns; the write flags always gate.
    let has_positional = |args: &[&str]| args.iter().any(|a| !a.starts_with('-'));
    let has_flag = |args: &[&str], flags: &[&str]| {
        args.iter().any(|a| {
            flags
                .iter()
                .any(|f| a == f || a.starts_with(&format!("{f}=")))
        })
    };

    match verb {
        "branch" => {
            const WRITES: &[&str] = &[
                "-d",
                "-D",
                "-m",
                "-M",
                "-c",
                "-C",
                "-f",
                "-u",
                "--delete",
                "--move",
                "--copy",
                "--force",
                "--edit-description",
                "--set-upstream-to",
                "--unset-upstream",
                "--create-reflog",
                "--track",
                "--no-track",
            ];
            const LISTS: &[&str] = &[
                "--list",
                "--show-current",
                "--contains",
                "--no-contains",
                "--merged",
                "--no-merged",
                "--points-at",
            ];
            !has_flag(args, WRITES) && (!has_positional(args) || has_flag(args, LISTS))
        }
        "tag" => {
            const WRITES: &[&str] = &[
                "-a",
                "-s",
                "-u",
                "-F",
                "-m",
                "-d",
                "-f",
                "-e",
                "--annotate",
                "--sign",
                "--file",
                "--message",
                "--delete",
                "--force",
                "--edit",
            ];
            const LISTS: &[&str] = &[
                "-l",
                "--list",
                "--contains",
                "--no-contains",
                "--merged",
                "--no-merged",
                "--points-at",
            ];
            !has_flag(args, WRITES) && (!has_positional(args) || has_flag(args, LISTS))
        }
        "stash" => matches!(subcommand(&mut args.iter().copied()), Some("list" | "show")),
        "remote" => matches!(
            subcommand(&mut args.iter().copied()),
            None | Some("show") | Some("get-url")
        ),
        "worktree" => matches!(subcommand(&mut args.iter().copied()), Some("list")),
        "reflog" => matches!(subcommand(&mut args.iter().copied()), None | Some("show")),

        // Everything else — commit, push, checkout, fetch, config, … — gates.
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
    fn dangerous_chaining_is_rejected() {
        // Sequencing is validated per-segment (see `readonly_sequences_are_allowed`),
        // but any segment that is a write, or any metacharacter a per-segment scan
        // can't reason about, gates the whole line. A read-only prefix must never
        // smuggle a second command through.
        assert!(!allowed("orx runs; orx exp run e-1")); // write segment
        assert!(!allowed("orx runs && rm -rf /")); // non-orx segment
        assert!(!allowed("orx runs; echo hi; orx create-project --repo x/y")); // write in a batch
        assert!(!allowed("orx runs | tee /etc/passwd")); // pipe into a writer
        assert!(!allowed("orx logs r-1 > /tmp/x")); // redirection
        assert!(!allowed("orx query \"$(rm -rf /)\"")); // command substitution
        assert!(!allowed("orx runs `whoami`")); // backtick substitution
        assert!(!allowed("orx runs &")); // background execution
        assert!(!allowed("orx runs & rm -rf /")); // single-& chaining
    }

    #[test]
    fn readonly_sequences_are_allowed() {
        // The planning agent batches several read-only lookups into one call,
        // punctuated by `echo` separators — the exact pattern plan mode was
        // failing to run. Each segment is independently read-only, so the whole
        // line is allowed.
        assert!(allowed("orx exp desc e-1; echo ====; orx exp desc e-2"));
        assert!(allowed("orx runs && orx logs r-1"));
        assert!(allowed("orx exp desc e-1 && orx exp cmd e-1"));
        // Mixed `&&` and `;` in one line exercises the two-level split.
        assert!(allowed("orx runs && orx logs r-1; echo done"));
        // Harmless glue on its own, and shell-tolerated trailing/empty segments.
        assert!(allowed("echo hello"));
        assert!(allowed("orx runs;"));
        assert!(allowed("orx runs ; ; orx logs r-1"));
        // A read/view form batched with its own write form still gates: the
        // `--set` segment is a write.
        assert!(!allowed("orx exp desc e-1; orx exp desc e-1 --set \"x\""));
    }

    #[test]
    fn readonly_pipelines_are_allowed() {
        // The other pattern plan mode kept failing on: reads piped through pure
        // consumers, with the customary stderr-merge.
        assert!(allowed("orx runs 2>&1 | head -50"));
        assert!(allowed("orx logs r-1 2>&1 | grep -i error | tail -5"));
        assert!(allowed("orx runs r-1 2>&1"));
        assert!(allowed("git log --oneline | head -20"));
        assert!(allowed("git ls-tree -r HEAD | grep -i py"));
        assert!(allowed("git show origin/b:mem2gen/orx_run.py | head -100"));
        // Consumers stand alone and pipe among themselves.
        assert!(allowed("wc -l README.md"));
        assert!(allowed("cat notes.md | grep TODO | sort | uniq"));
        // Pipelines and sequences compose.
        assert!(allowed("orx runs | head -5; echo ok && git status"));
        // No-space pipes parse the same way the shell parses them.
        assert!(allowed("orx runs 2>&1|head -5"));
    }

    #[test]
    fn nonreadonly_pipelines_are_gated() {
        assert!(!allowed("orx runs | xargs rm")); // consumer that spawns commands
        assert!(!allowed("orx runs | sed -i s/x/y/ f")); // sed excluded (writes)
        assert!(!allowed("orx runs | awk '{system(\"id\")}'")); // awk excluded
        assert!(!allowed("orx runs | head > /tmp/x")); // redirect in a stage
        assert!(!allowed("orx runs || rm -rf /")); // `||` is not a pipe
        assert!(!allowed("orx runs |")); // trailing pipe
        assert!(!allowed("| head")); // no producer
        assert!(!allowed("cargo metadata | head")); // unknown producer
        assert!(!allowed("head -1 f | orx runs")); // orx is not a consumer
    }

    #[test]
    fn git_reads_are_allowed() {
        for c in [
            "git status",
            "git log --oneline -20",
            "git show HEAD~1:src/main.rs",
            "git diff --stat main..feature",
            "git grep -n TODO",
            "git ls-tree -r --name-only origin/main",
            "git ls-files",
            "git rev-parse HEAD",
            "git rev-list --count HEAD",
            "git cat-file -p HEAD:README.md",
            "git blame src/main.rs",
            "git describe --tags",
            "git merge-base main feature",
            "git -P log -5",
            "git --no-pager diff",
            "git -C /some/repo log --oneline",
            "git branch -a",
            "git branch --list 'daniel/*'",
            "git branch --show-current",
            "git tag",
            "git tag -l 'v*'",
            "git stash list",
            "git stash show",
            "git remote -v",
            "git remote show origin",
            "git remote get-url origin",
            "git worktree list",
            "git reflog",
            "/usr/bin/git status",
        ] {
            assert!(allowed(c), "should allow: {c}");
        }
    }

    #[test]
    fn git_writes_are_gated() {
        for c in [
            "git push",
            "git commit -m x",
            "git checkout main",
            "git switch -c feat",
            "git add .",
            "git reset --hard HEAD~1",
            "git fetch origin",
            "git pull",
            "git merge feature",
            "git rebase main",
            "git branch foo",                           // creates
            "git branch -d foo",                        // deletes
            "git branch -m old new",                    // renames
            "git branch --set-upstream-to=origin/main", // config write
            "git tag v1.0",                             // creates
            "git tag -d v1.0",                          // deletes
            "git tag -a v1 -m msg",                     // creates annotated
            "git stash",                                // bare stash = push
            "git stash pop",
            "git remote add origin url",
            "git remote set-url origin url",
            "git worktree add /tmp/wt",
            "git worktree remove /tmp/wt",
            "git reflog expire --all",
            "git config user.name x", // excluded wholesale
            "git",                    // bare usage: no reason to allow
            // Escape hatches on otherwise-read verbs.
            "git -c core.pager='!id' log", // -c injects executable config
            "git --git-dir=/x/.git log",   // repo redirection
            "git log --output=/tmp/exfil", // writes a file
            "git grep -Oless TODO",        // executes a pager
            "git grep --open-files-in-pager TODO",
        ] {
            assert!(!allowed(c), "should gate: {c}");
        }
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
    fn exit_plan_mode_asks_instead_of_self_approving() {
        let p = json!({ "tool_name": "ExitPlanMode", "tool_input": { "plan": "do X" } });
        let out = decide(&p).expect("ExitPlanMode must get a decision");
        assert_eq!(
            out.pointer("/hookSpecificOutput/permissionDecision")
                .and_then(Value::as_str),
            Some("ask")
        );
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
