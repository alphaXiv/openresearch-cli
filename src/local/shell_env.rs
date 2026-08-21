//! The slice of the user's shell environment that a Finder-launched app never
//! inherits, and everything that has to agree with a terminal `orx`.
//!
//! A bundle started by launchd gets `PATH=/usr/bin:/bin:/usr/sbin:/sbin` and no
//! shell rc sourced, so without this the app finds no `codex` at all and
//! `claude`/`opencode` only at their installer drop locations, and it resolves
//! its data and config directories to the defaults while the CLI on the same
//! machine uses whatever the user exported. Two OpenResearch installs then
//! disagree about which database they are looking at.
//!
//! macOS app mode probes the shell once at startup ([`crate::commands::app`])
//! and installs the answer here; every other entry point falls through to the
//! process environment unchanged.
//!
//! Scope is orx's own resolution and the children it spawns. The other things
//! orx shells out to — `git`, `gh`, `kubectl`, `ssh`, the detached
//! `publish-branch` worker — still inherit the process environment.

use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::path::PathBuf;
use std::sync::OnceLock;

/// Deliberately short. These are the variables whose divergence makes the app
/// and the CLI behave like different installs; values adopted here reach
/// harness children through `chat::prepare_env`.
pub const IMPORTED: [&str; 5] = [
    "PATH",
    "ORX_DATA_DIR",
    "XDG_DATA_HOME",
    "XDG_CONFIG_HOME",
    "CLAUDE_CONFIG_DIR",
];

static OVERRIDE: OnceLock<HashMap<&'static str, OsString>> = OnceLock::new();

/// Like `env::var_os`, but preferring what the user's shell reported.
pub fn var(key: &str) -> Option<OsString> {
    OVERRIDE
        .get()
        .and_then(|vars| vars.get(key).cloned())
        .or_else(|| std::env::var_os(key))
}

/// The PATH to search for the binaries orx spawns, and to hand its children.
pub fn search_path() -> Option<OsString> {
    var("PATH")
}

/// Where `binary` lives, or None when this machine has no such tool. The path
/// is returned as it sits on PATH; a caller that needs the real binary behind a
/// symlink composes with `resolve_symlinks`.
pub fn find_on_path(binary: &str) -> Option<PathBuf> {
    search_in(&search_path()?, binary)
}

/// Split from the PATH lookup so the search is testable without a probe.
fn search_in(paths: &OsStr, binary: &str) -> Option<PathBuf> {
    // A relative entry (`""`, meaning cwd, or `bin`) names no fixed directory —
    // it resolves against whichever cwd is current, so it is never a place to
    // pick up a binary.
    std::env::split_paths(paths)
        .filter(|dir| dir.is_absolute())
        .map(|dir| dir.join(binary))
        .find(|candidate| candidate.is_file())
}

/// Hand the imported variables to a child process. Every `orx` child re-resolves
/// its directories from its own environment and only app mode ever probes, so
/// without this a supervisor spawned by the app would write to the default
/// store while the app read the user's. PATH is excluded — callers set it
/// themselves, some of them prepending this binary's own directory first.
pub fn export_to(mut set: impl FnMut(&'static str, &OsString)) {
    let Some(vars) = OVERRIDE.get() else {
        return;
    };
    // In `IMPORTED` order: the adopted set is logged at startup, and a stable
    // order keeps that line diffable across launches.
    for key in IMPORTED.iter().filter(|key| **key != "PATH") {
        if let Some(value) = vars.get(key) {
            set(key, value);
        }
    }
}

/// Install the probe's answer; the first call wins. Deliberately not
/// `env::set_var` — app mode enters inside an already-running tokio runtime,
/// where mutating the process environment races every live thread.
#[cfg(target_os = "macos")]
pub fn set(vars: HashMap<&'static str, OsString>) {
    let _ = OVERRIDE.set(vars);
}

/// Parse the probe's stdout: [`IMPORTED`]'s values in order, NUL-separated,
/// fenced between two `marker`s.
///
/// Requiring PATH to hold an absolute directory is what rejects a garbage or
/// empty capture — including the literal `%s` left behind if the fence ever
/// wraps the `printf` template rather than its output. Empty values mean the
/// variable was unset in the shell and are dropped, so lookups fall through to
/// the process environment.
pub fn parse_probe(stdout: &str, marker: &str) -> Option<HashMap<&'static str, OsString>> {
    let mut fences = stdout.split(marker);
    // `split` always yields a first element; the *third* is what proves the
    // closing fence arrived rather than the probe being cut short.
    let payload = fences.nth(1)?;
    fences.next()?;

    let vars: HashMap<&'static str, OsString> = IMPORTED
        .iter()
        .zip(payload.split('\0'))
        .filter(|(_, value)| !value.is_empty())
        .map(|(key, value)| (*key, OsString::from(value)))
        .collect();
    let path = vars.get("PATH")?;
    std::env::split_paths(path)
        .any(|dir| dir.is_absolute())
        .then_some(vars)
}

#[cfg(test)]
mod tests {
    use super::*;

    const M: &str = "__ORX_ENV_abc123__";

    fn fenced(payload: &str) -> String {
        format!("nvm loaded\n{M}{payload}{M}")
    }

    #[test]
    fn the_search_skips_relative_entries_and_takes_the_first_absolute_hit() {
        let root = std::env::temp_dir().join(format!("orx-path-search-{}", std::process::id()));
        let (early, late) = (root.join("early"), root.join("late"));
        std::fs::create_dir_all(&early).expect("early");
        std::fs::create_dir_all(&late).expect("late");
        std::fs::write(early.join("tool"), "").expect("early tool");
        std::fs::write(late.join("tool"), "").expect("late tool");

        let paths =
            std::env::join_paths([PathBuf::new(), PathBuf::from("bin"), early.clone(), late])
                .expect("join");
        assert_eq!(search_in(&paths, "tool"), Some(early.join("tool")));
        assert_eq!(search_in(&paths, "absent"), None);

        // A relative entry is rejected even when it does resolve: cargo runs
        // tests from the package root, so `src/main.rs` is a real hit here.
        let relative = std::env::join_paths([PathBuf::from("src")]).expect("join");
        assert_eq!(search_in(&relative, "main.rs"), None);

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn reads_every_imported_variable() {
        let vars = parse_probe(
            &fenced("/opt/homebrew/bin:/usr/bin\0/data\0/share\0/config\0/claude-config\0"),
            M,
        )
        .unwrap();
        assert_eq!(vars["PATH"], OsString::from("/opt/homebrew/bin:/usr/bin"));
        assert_eq!(vars["ORX_DATA_DIR"], OsString::from("/data"));
        assert_eq!(vars["XDG_DATA_HOME"], OsString::from("/share"));
        assert_eq!(vars["XDG_CONFIG_HOME"], OsString::from("/config"));
        assert_eq!(vars["CLAUDE_CONFIG_DIR"], OsString::from("/claude-config"));
    }

    #[test]
    fn unset_variables_are_dropped_so_lookups_fall_through() {
        let vars = parse_probe(&fenced("/usr/bin\0\0\0\0\0"), M).unwrap();
        assert_eq!(vars["PATH"], OsString::from("/usr/bin"));
        assert!(!vars.contains_key("ORX_DATA_DIR"));
        assert_eq!(vars.len(), 1);
    }

    #[test]
    fn rejects_truncated_empty_or_pathless_output() {
        assert!(parse_probe("", M).is_none());
        assert!(parse_probe(&format!("{M}/usr/bin\0"), M).is_none());
        assert!(parse_probe(&fenced("\0/data\0\0\0\0"), M).is_none());
    }

    #[test]
    fn rejects_a_fence_wrapping_the_template_instead_of_its_output() {
        let stdout = format!(r#"+ /bin/sh -c printf "{M}%s\0{M}" "$PATH""#);
        assert!(parse_probe(&stdout, M).is_none());
    }
}
