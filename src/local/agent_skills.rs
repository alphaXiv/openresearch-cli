//! Native modular agent skills for `orx`.
//!
//! The monolithic `orx skill` overview (repo-root `SKILL.md`) is factored into a
//! handful of focused modules whose bodies live in the repo `agent-skills/`
//! directory and are embedded in the binary at compile time. Two consumers use
//! them:
//!
//! * **Local `orx up` sessions** get the [`SkillSet::Local`] modules written as
//!   native `SKILL.md` skill dirs *into the session worktree* — fresh on every
//!   turn, right beside the playbook (see [`ensure_session_skills`]). The harness
//!   picks the skills subdir (`.claude/skills`, `.opencode/skills`,
//!   `.agents/skills`), so the session's own agent auto-discovers them and never
//!   sees drift.
//! * **`orx skill <name>`** resolves a bundled module (with or without the
//!   `orx-` prefix) and prints it; the no-arg overview lists the
//!   [`SkillSet::Full`] set. `orx install-skills --full` writes the Full set into
//!   an agent's global skills dir (the dedicated cloud box).
//!
//! The two sets share the same public skill *names* so docs and references stay
//! stable; a few modules swap their **body** between a local-mode form
//! (logs-only evidence, files-dir reports) and a full/cloud form (artifacts +
//! query + chart; `orx report` upload). The `orx-` prefix on every dir name
//! makes them unmistakable in an agent's skill listing.

use std::path::Path;

use crate::error::{anyhow, Result};

/// One embedded skill module: its public name (== skill dir name == the `name:`
/// frontmatter field), a one-line description, and the markdown body (no
/// frontmatter — [`render`] generates it).
pub struct AgentSkill {
    pub name: &'static str,
    pub description: &'static str,
    pub body: &'static str,
}

/// Which set of module bodies to serve. The two sets carry the same public skill
/// names; only a few bodies differ (see the module docs).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SkillSet {
    /// `orx up` local mode: logs-only evidence, files-dir reports, no `create`.
    Local,
    /// Full/cloud surface: artifacts + query + chart evidence, `orx report`
    /// upload, and the project/experiment creation module.
    Full,
}

// --- Module bodies (embedded from the repo `agent-skills/` dir) --------------

const COMPUTE: &str = include_str!("../../agent-skills/compute.md");
const COMPUTE_K8S: &str = include_str!("../../agent-skills/compute-k8s.md");
const EXPERIMENT_TREE: &str = include_str!("../../agent-skills/experiment-tree.md");
const GIT_EDITING: &str = include_str!("../../agent-skills/git-editing.md");
const LIT: &str = include_str!("../../agent-skills/lit.md");
const CREATE: &str = include_str!("../../agent-skills/create.md");
const REPORTS_LOCAL: &str = include_str!("../../agent-skills/reports-local.md");
const REPORTS_CLOUD: &str = include_str!("../../agent-skills/reports-cloud.md");
const EVIDENCE_LOCAL: &str = include_str!("../../agent-skills/evidence-local.md");
const EVIDENCE_CLOUD: &str = include_str!("../../agent-skills/evidence-cloud.md");

// Descriptions are ≤150 chars (Codex's ambient skill budget is tight): one line,
// no leading `orx-`, phrased so an agent knows when to load the module.

const S_COMPUTE: AgentSkill = AgentSkill {
    name: "orx-compute",
    description: "Launch experiment runs on compute with `orx exp run`: managed GPU/CPU, hf/modal/ssh/local backends, sizing, and waiting on runs.",
    body: COMPUTE,
};
const S_COMPUTE_K8S: AgentSkill = AgentSkill {
    name: "orx-compute-k8s",
    description: "Run an experiment on your Kubernetes cluster (`orx exp run --backend k8s`): the manifest contract orx enforces at submit.",
    body: COMPUTE_K8S,
};
const S_EXPERIMENT_TREE: AgentSkill = AgentSkill {
    name: "orx-experiment-tree",
    description: "The experiment-tree model and the auto-research loop: shape the tree (stacked bushes), branch/launch/wait/promote, and `orx exp desc`.",
    body: EXPERIMENT_TREE,
};
const S_GIT: AgentSkill = AgentSkill {
    name: "orx-git",
    description: "Read, edit, and diff a node's code with plain git in the cache-dir clone (or your session worktree): sync, commit, push before running.",
    body: GIT_EDITING,
};
const S_LIT: AgentSkill = AgentSkill {
    name: "orx-lit",
    description: "Search literature and read papers via alphaXiv (`orx lit` / `orx paper`) — ground hypotheses and find code to seed a baseline from.",
    body: LIT,
};
const S_CREATE: AgentSkill = AgentSkill {
    name: "orx-create",
    description: "Create a project (`orx create-project`), seed an empty baseline from existing code, and add experiment nodes (`orx create-experiment`).",
    body: CREATE,
};
const S_REPORTS_LOCAL: AgentSkill = AgentSkill {
    name: "orx-reports",
    description: "Write research reports into the local project's files dir (tree-mirroring folder layout) — they appear in the dashboard's Files tab.",
    body: REPORTS_LOCAL,
};
const S_REPORTS_CLOUD: AgentSkill = AgentSkill {
    name: "orx-reports",
    description: "Write a research report and publish it with `orx report upload` (list/show/download too) so it appears on the project page.",
    body: REPORTS_CLOUD,
};
const S_EVIDENCE_LOCAL: AgentSkill = AgentSkill {
    name: "orx-evidence",
    description: "Analyze results in local mode: run logs are the only channel (`orx logs`) — make the run print the evidence you'll need.",
    body: EVIDENCE_LOCAL,
};
const S_EVIDENCE_CLOUD: AgentSkill = AgentSkill {
    name: "orx-evidence",
    description: "Analyze results: run logs, `orx search-logs`, text artifacts, W&B charts (`orx chart wandb`), and the `orx query` evidence DB.",
    body: EVIDENCE_CLOUD,
};

/// The modules for a given set, in a stable order. Local and Full share names;
/// `reports`/`evidence` swap bodies, and `create` is Full-only.
pub fn skills(set: SkillSet) -> Vec<&'static AgentSkill> {
    match set {
        SkillSet::Local => vec![
            &S_EXPERIMENT_TREE,
            &S_GIT,
            &S_COMPUTE,
            &S_COMPUTE_K8S,
            &S_EVIDENCE_LOCAL,
            &S_REPORTS_LOCAL,
            &S_LIT,
        ],
        SkillSet::Full => vec![
            &S_CREATE,
            &S_EXPERIMENT_TREE,
            &S_GIT,
            &S_COMPUTE,
            &S_COMPUTE_K8S,
            &S_EVIDENCE_CLOUD,
            &S_REPORTS_CLOUD,
            &S_LIT,
        ],
    }
}

/// Resolve a bundled Full-set skill by name, accepting both the public name
/// (`orx-compute`) and the bare form (`compute`). `None` for an unknown name —
/// the caller falls back to the live API fetch.
pub fn find(name: &str) -> Option<&'static AgentSkill> {
    let want = name.trim();
    skills(SkillSet::Full)
        .into_iter()
        .find(|s| s.name == want || s.name.strip_prefix("orx-") == Some(want))
}

/// Render a skill to a full `SKILL.md`: generated frontmatter + body. The
/// frontmatter `name` matches the dir name; `description` is the module's line.
///
/// The `description` is emitted as a **JSON-quoted** scalar. Descriptions
/// contain colon-space (`orx exp run`: managed …) and backticks, which are
/// invalid in an unquoted YAML plain scalar — a strict frontmatter parser (the
/// Claude/Codex/OpenCode skill loaders) would error or truncate the value. A
/// JSON string is always a valid YAML flow scalar, so quoting keeps every
/// present and future description safe regardless of punctuation.
pub fn render(skill: &AgentSkill) -> String {
    let description = serde_json::to_string(skill.description)
        .unwrap_or_else(|_| format!("{:?}", skill.description));
    format!(
        "---\nname: {}\ndescription: {}\n---\n\n{}",
        skill.name,
        description,
        skill.body.trim_end(),
    )
}

/// Write the [`SkillSet::Local`] modules as `<worktree>/<skills_dir_rel>/<name>/SKILL.md`,
/// overwriting every file on every call (same freshness semantics as the
/// playbook — zero drift). Returns `Err` on the first write failure; the caller
/// treats it like a playbook-write error.
pub fn ensure_session_skills(worktree: &Path, skills_dir_rel: &str) -> Result<()> {
    let base = worktree.join(skills_dir_rel);
    for skill in skills(SkillSet::Local) {
        let dir = base.join(skill.name);
        std::fs::create_dir_all(&dir)
            .map_err(|e| anyhow!("Could not create {}: {}", dir.display(), e))?;
        let path = dir.join("SKILL.md");
        std::fs::write(&path, render(skill))
            .map_err(|e| anyhow!("Could not write {}: {}", path.display(), e))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn is_valid_name(name: &str) -> bool {
        // ^[a-z0-9]+(-[a-z0-9]+)*$
        !name.is_empty()
            && name.split('-').all(|seg| {
                !seg.is_empty()
                    && seg
                        .chars()
                        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
            })
    }

    #[test]
    fn names_are_valid_unique_and_prefixed() {
        for set in [SkillSet::Local, SkillSet::Full] {
            let mut seen = HashSet::new();
            for s in skills(set) {
                assert!(
                    is_valid_name(s.name),
                    "{:?}: invalid name {:?}",
                    set,
                    s.name
                );
                assert!(
                    s.name.starts_with("orx-"),
                    "{:?}: name {:?} not orx- prefixed",
                    set,
                    s.name
                );
                assert!(
                    seen.insert(s.name),
                    "{:?}: duplicate name {:?}",
                    set,
                    s.name
                );
            }
        }
    }

    #[test]
    fn descriptions_are_within_bounds() {
        for set in [SkillSet::Local, SkillSet::Full] {
            for s in skills(set) {
                let len = s.description.chars().count();
                assert!(
                    (1..=150).contains(&len),
                    "{:?}: {} description is {} chars (want 1..=150)",
                    set,
                    s.name,
                    len
                );
            }
        }
    }

    #[test]
    fn rendered_frontmatter_is_valid_yaml() {
        for set in [SkillSet::Local, SkillSet::Full] {
            for s in skills(set) {
                let rendered = render(s);
                let mut lines = rendered.lines();
                assert_eq!(lines.next(), Some("---"), "{} missing opening ---", s.name);
                let name_line = lines.next().unwrap_or_default();
                let desc_line = lines.next().unwrap_or_default();
                assert_eq!(
                    name_line,
                    format!("name: {}", s.name),
                    "{} name frontmatter",
                    s.name
                );

                // The description value must be YAML-safe. `render` emits it as a
                // JSON-quoted scalar; strip `description: ` and JSON-decode it —
                // this round-trip both proves the quoting is well-formed and
                // recovers the exact description. A bare (unquoted) value
                // containing `: ` — the bug this guards — would not JSON-decode.
                let value = desc_line
                    .strip_prefix("description: ")
                    .unwrap_or_else(|| panic!("{} description frontmatter shape", s.name));
                let decoded: String = serde_json::from_str(value).unwrap_or_else(|e| {
                    panic!("{} description is not a quoted scalar: {e}", s.name)
                });
                assert_eq!(decoded, s.description, "{} description round-trip", s.name);
                // A quoted scalar is a single physical line — no embedded newline.
                assert!(
                    !s.description.contains('\n'),
                    "{} description has a newline",
                    s.name
                );

                assert_eq!(lines.next(), Some("---"), "{} missing closing ---", s.name);
                // A non-empty body follows the closing frontmatter fence
                // (`\n---\n\n` separates the frontmatter block from the body).
                let body = rendered
                    .split_once("\n---\n\n")
                    .map(|(_, body)| body)
                    .unwrap_or("");
                assert!(!body.trim().is_empty(), "{} has an empty body", s.name);
            }
        }
    }

    #[test]
    fn find_resolves_prefixed_and_bare() {
        assert_eq!(find("orx-compute").map(|s| s.name), Some("orx-compute"));
        assert_eq!(find("compute").map(|s| s.name), Some("orx-compute"));
        assert_eq!(find("orx-create").map(|s| s.name), Some("orx-create"));
        assert_eq!(find("create").map(|s| s.name), Some("orx-create"));
        assert!(find("does-not-exist").is_none());
        assert!(find("project-query").is_none());
    }

    #[test]
    fn ensure_session_skills_writes_local_set_idempotently() {
        let tmp = std::env::temp_dir().join(format!(
            "orx-agent-skills-test-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        let rel = ".claude/skills";
        ensure_session_skills(&tmp, rel).unwrap();

        let base = tmp.join(rel);
        let expected: HashSet<&str> = skills(SkillSet::Local).iter().map(|s| s.name).collect();
        let got: HashSet<String> = std::fs::read_dir(&base)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        let got_refs: HashSet<&str> = got.iter().map(String::as_str).collect();
        assert_eq!(got_refs, expected, "wrote exactly the Local-set dirs");

        for s in skills(SkillSet::Local) {
            let path = base.join(s.name).join("SKILL.md");
            let content = std::fs::read_to_string(&path).unwrap();
            assert_eq!(content, render(s), "{} SKILL.md content", s.name);
        }

        // Idempotent: a second call overwrites in place and changes nothing.
        ensure_session_skills(&tmp, rel).unwrap();
        let got2: HashSet<String> = std::fs::read_dir(&base)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(got2, got, "second call is idempotent");

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
