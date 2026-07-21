//! Native modular agent skills for `orx`.
//!
//! The monolithic `orx skill` overview (repo-root `SKILL.md`) is factored into a
//! handful of focused modules that live as **literal, complete skill files** in
//! the repo `agent-skills/` directory (`agent-skills/<name>/SKILL.md`,
//! frontmatter included — readable as-is on GitHub) and are embedded in the
//! binary at compile time and installed verbatim. Two consumers use them:
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
//! stable; several modules swap their **body** between a local-mode form
//! (backend-based launches, logs-only evidence, files-dir reports, worktree
//! git flow) and a full/cloud form (managed-SKU compute, artifacts + query +
//! chart, `orx report` upload). The `orx-` prefix on every dir name makes them
//! unmistakable in an agent's skill listing.

use std::path::Path;

use crate::error::{anyhow, Result};

/// One embedded skill module: its public name (== skill dir name == the `name:`
/// frontmatter field), a one-line description (mirrored in the file's
/// frontmatter — a test enforces they agree), and the complete `SKILL.md`
/// contents, installed and printed verbatim.
pub struct AgentSkill {
    pub name: &'static str,
    pub description: &'static str,
    pub content: &'static str,
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

// --- Module files (embedded verbatim from the repo `agent-skills/` dir; the
// `SKILL.local.md` siblings are the local-mode body variants under the same
// public skill name) ----------------------------------------------------------

const COMPUTE_LOCAL: &str = include_str!("../../agent-skills/orx-compute/SKILL.local.md");
const COMPUTE_CLOUD: &str = include_str!("../../agent-skills/orx-compute/SKILL.md");
const COMPUTE_K8S: &str = include_str!("../../agent-skills/orx-compute-k8s/SKILL.md");
const EXPERIMENT_TREE_LOCAL: &str =
    include_str!("../../agent-skills/orx-experiment-tree/SKILL.local.md");
const EXPERIMENT_TREE_CLOUD: &str = include_str!("../../agent-skills/orx-experiment-tree/SKILL.md");
const GIT_EDITING: &str = include_str!("../../agent-skills/orx-git/SKILL.md");
const LIT: &str = include_str!("../../agent-skills/orx-lit/SKILL.md");
const CREATE: &str = include_str!("../../agent-skills/orx-create/SKILL.md");
const REPORTS_LOCAL: &str = include_str!("../../agent-skills/orx-reports/SKILL.local.md");
const REPORTS_CLOUD: &str = include_str!("../../agent-skills/orx-reports/SKILL.md");
const EVIDENCE_LOCAL: &str = include_str!("../../agent-skills/orx-evidence/SKILL.local.md");
const EVIDENCE_CLOUD: &str = include_str!("../../agent-skills/orx-evidence/SKILL.md");

// Descriptions are the *trigger surface*: what the module covers plus explicit,
// liberal "Use when …" cues (false positives beat false negatives — an agent
// that loads a module needlessly wastes a little context; one that misses it
// works blind). Keep each ≤400 chars — Codex's ambient budget is ~8k across
// the whole set.

// The compute and experiment-tree descriptions are shared by the local and
// cloud body variants (same public name, same triggers — only the body
// changes), so they live in one const each.
const D_COMPUTE: &str = "Launch experiment runs with `orx exp run`: backends (hf, modal, k8s, ssh, slurm, openresearch, local), flavors, timeouts, images, sizing, and `orx exp wait`. Use before launching or re-launching any run, when choosing or switching a backend or GPU flavor, when a job OOMs, stalls, or times out, or when deciding GPU vs CPU.";
const D_EXPERIMENT_TREE: &str = "The experiment-tree model and the auto-research loop: shape the tree (stacked bushes), branch/launch/wait/promote, and `orx exp desc` notes. Use before creating, planning, or reorganizing experiments, when deciding what to try next, when a round of runs finishes, or whenever you're unsure how work maps onto the tree.";

const S_COMPUTE_LOCAL: AgentSkill = AgentSkill {
    name: "orx-compute",
    description: D_COMPUTE,
    content: COMPUTE_LOCAL,
};
const S_COMPUTE_CLOUD: AgentSkill = AgentSkill {
    name: "orx-compute",
    description: D_COMPUTE,
    content: COMPUTE_CLOUD,
};
const S_COMPUTE_K8S: AgentSkill = AgentSkill {
    name: "orx-compute-k8s",
    description: "Run an experiment on your own Kubernetes cluster (`orx exp run --backend k8s`): the committed-manifest contract orx enforces at submit. Use when the user names k8s, kubernetes, or a cluster, before writing or editing `.orx/k8s.yaml`, for multi-node or Indexed Jobs, or when a k8s submit is rejected.",
    content: COMPUTE_K8S,
};
const S_EXPERIMENT_TREE_LOCAL: AgentSkill = AgentSkill {
    name: "orx-experiment-tree",
    description: D_EXPERIMENT_TREE,
    content: EXPERIMENT_TREE_LOCAL,
};
const S_EXPERIMENT_TREE_CLOUD: AgentSkill = AgentSkill {
    name: "orx-experiment-tree",
    description: D_EXPERIMENT_TREE,
    content: EXPERIMENT_TREE_CLOUD,
};
const S_GIT: AgentSkill = AgentSkill {
    name: "orx-git",
    description: "Read, edit, and diff a node's code with plain git: sync, commit, and push before running. Use whenever you touch experiment code — before editing any branch, when a checkout or push fails, when comparing two nodes' code, or when a run seems to have picked up stale code.",
    content: GIT_EDITING,
};
const S_LIT: AgentSkill = AgentSkill {
    name: "orx-lit",
    description: "Search literature and read papers via alphaXiv (`orx lit` / `orx paper`). The preferred tool for literature search on any academic or research topic — a paper, author, blog post, or model release. Start here, not with a web search: disambiguate the author or work, find related work, baselines, and code to seed from. Often the corpus answers outright and no web search is needed.",
    content: LIT,
};
const S_CREATE: AgentSkill = AgentSkill {
    name: "orx-create",
    description: "Create a project (`orx create-project`), seed an empty baseline from existing code, and add experiment nodes (`orx create-experiment`). Use when starting any new project or experiment, when the tree is empty, or when unsure how to bind a repo or set the run command.",
    content: CREATE,
};
const S_REPORTS_LOCAL: AgentSkill = AgentSkill {
    name: "orx-reports",
    description: "Write research reports into the local project's files dir (tree-mirroring folder layout) so they appear in the dashboard's Files tab. Use when a line of work concludes, when the user asks for a write-up, summary, comparison, or figures, or before ending a long task — findings not written down are lost.",
    content: REPORTS_LOCAL,
};
const S_REPORTS_CLOUD: AgentSkill = AgentSkill {
    name: "orx-reports",
    description: "Write a research report and publish it with `orx report upload` (list/show/download too) so it appears on the project page. Use when a line of work concludes, when the user asks for a write-up, summary, comparison, or figures, or before ending a long task — findings not written down are lost.",
    content: REPORTS_CLOUD,
};
const S_EVIDENCE_LOCAL: AgentSkill = AgentSkill {
    name: "orx-evidence",
    description: "Analyze run results in local mode: run logs are the only evidence channel (`orx logs`). Use after any run reaches a terminal state, before declaring a run a success or failure, when metrics are missing from output, or when designing what a run command should print.",
    content: EVIDENCE_LOCAL,
};
const S_EVIDENCE_CLOUD: AgentSkill = AgentSkill {
    name: "orx-evidence",
    description: "Analyze run results: `orx logs`, `orx search-logs`, text artifacts, W&B charts (`orx chart wandb`), and the `orx query` evidence DB. Use after any run finishes, when comparing metrics across runs or experiments, when hunting a failure in logs, or when asked for numbers, tables, or charts.",
    content: EVIDENCE_CLOUD,
};

/// The modules for a given set, in a stable order. Local and Full share names;
/// `experiment-tree`/`compute`/`reports`/`evidence` swap bodies, and `create`
/// is Full-only.
pub fn skills(set: SkillSet) -> Vec<&'static AgentSkill> {
    match set {
        SkillSet::Local => vec![
            &S_EXPERIMENT_TREE_LOCAL,
            &S_GIT,
            &S_COMPUTE_LOCAL,
            &S_COMPUTE_K8S,
            &S_EVIDENCE_LOCAL,
            &S_REPORTS_LOCAL,
            &S_LIT,
        ],
        SkillSet::Full => vec![
            &S_CREATE,
            &S_EXPERIMENT_TREE_CLOUD,
            &S_GIT,
            &S_COMPUTE_CLOUD,
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
        std::fs::write(&path, skill.content)
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
                    (1..=400).contains(&len),
                    "{:?}: {} description is {} chars (want 1..=400)",
                    set,
                    s.name,
                    len
                );
            }
        }
    }

    #[test]
    fn file_frontmatter_matches_code_and_is_valid_yaml() {
        // The skill files under `agent-skills/` are the literal installed
        // artifacts (embedded verbatim), so their frontmatter must agree with
        // the code's name/description — this is the drift guard between the
        // GitHub-readable files and the indexes generated from the consts.
        for set in [SkillSet::Local, SkillSet::Full] {
            for s in skills(set) {
                let mut lines = s.content.lines();
                assert_eq!(lines.next(), Some("---"), "{} missing opening ---", s.name);
                let name_line = lines.next().unwrap_or_default();
                let desc_line = lines.next().unwrap_or_default();
                assert_eq!(
                    name_line,
                    format!("name: {}", s.name),
                    "{} name frontmatter",
                    s.name
                );

                // The description value must be YAML-safe. The files carry it
                // as a JSON-quoted scalar; strip `description: ` and JSON-decode
                // — the round-trip proves the quoting is well-formed and that
                // the file agrees with the code's description. A bare
                // (unquoted) value containing `: ` — the bug this guards —
                // would not JSON-decode.
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
                let body = s
                    .content
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
            assert_eq!(content, s.content, "{} SKILL.md content", s.name);
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
