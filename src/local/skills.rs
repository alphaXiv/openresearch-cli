//! Slash-skills for the `orx up` chat — canned prompt templates the user
//! invokes as `/name <args>` from the composer. The UI lists them via
//! `/api/skills`; expansion happens server-side in `ChatHost::send_message`
//! so the transcript keeps the short `/name` form the user typed while the
//! harness receives the full prompt. Works identically across all harnesses.

/// One built-in skill. `template` may contain `{args}`, replaced with the text
/// after the command.
pub struct Skill {
    pub name: &'static str,
    pub description: &'static str,
    /// Shown greyed-out in the picker after the name (e.g. "<topic>").
    pub arg_hint: &'static str,
    pub template: &'static str,
    /// Substituted for `{args}` when the user gives none.
    pub no_args: &'static str,
}

const LIT_REVIEW_TEMPLATE: &str = r#"Perform a multi-hop literature review using alphaXiv.

Topic: {args}

Use the `orx` CLI (already installed; public endpoints, no login needed):
- `orx lit "<query>"` — full-text search across papers; returns ids, titles, abstracts, and page-anchored snippets (`--json` for machine-readable output).
- `orx paper <id>` — a paper's structured overview report (~10 KB); `--full` for the raw extracted text when the report lacks a detail.

Method — iterate; do not stop after one search:
1. Hop 1: run `orx lit` with 2-3 distinct phrasings of the topic. Skim titles/abstracts/snippets and pick the 3-5 most relevant papers.
2. Read them: `orx paper <id>` for each pick.
3. Next hop: from those reports, extract cited papers, author names, benchmark/method names, and field terminology you did not start with. Turn these into new `orx lit` queries and search again.
4. Repeat until a hop surfaces nothing relevantly new (typically 2-4 hops). Track which papers you have already seen so you don't re-read them.

Then write the review:
- Organize by theme, not by paper.
- For each theme: the key papers (id + title), what they claim, where they agree and disagree.
- Note open problems / gaps you noticed.
- Cite every claim with its paper id (e.g. 2401.12345) so the user can pull it with `orx paper`.
- End with a short "start here" reading list of the 3-5 most load-bearing papers.
"#;

const HF_TEMPLATE: &str = r#"Work with the Hugging Face Hub using the `hf` CLI.

Task: {args}

Setup — verify before anything else:
1. `hf version` — if missing, install with `curl -LsSf https://hf.co/cli/install.sh | sh` (or `pip install -U "huggingface_hub[cli]"`), then re-check.
2. `hf auth whoami` — HF_TOKEN is usually already in the environment (synced from the orx up settings). If unauthenticated, ask the user to add their token in the orx up settings (Hugging Face section) or run `hf auth login`.

Using the CLI:
- Discover flags with `hf --help` and `hf <command> --help`; don't guess.
- Key families: `hf download` / `hf upload` (models, datasets, spaces), `hf jobs` (run compute on HF infra), `hf repo` (create/manage repos), `hf cache` (inspect/clean local cache).
- Prefer `--repo-type dataset|space` flags over guessing repo id formats.
- For anything destructive (deleting repos/files, overwriting), confirm with the user first.
"#;

const ICML_REPRO_TEMPLATE: &str = r#"Reproduce an ICML 2026 paper for the agent reproduction challenge and publish a Trackio logbook.

Paper: {args}

Setup — verify before anything else:
1. `hf version` — if missing, install with `curl -LsSf https://hf.co/cli/install.sh | sh` (or `pip install -U "huggingface_hub[cli]"`), then re-check.
2. `hf auth whoami` — HF_TOKEN is usually already in the environment (synced from the orx up settings). If unauthenticated, ask the user to add their token in the orx up settings (Hugging Face section) or run `hf auth login`. Note the username — the logbook publishes under it.
3. `trackio --version` — if missing, `pip install --upgrade trackio`.

Workflow:
1. Fetch the challenge guide and follow it — it is the authoritative rulebook (metadata tags, claim verdicts, judging criteria):
   `curl -sL https://huggingface.co/datasets/ICML-2026-agent-repro/challenge/resolve/main/README.md`
2. Open a logbook for the paper: `trackio logbook open --title "Repro: <paper title>"`. For logbook command details, run `trackio logbook --help` — do not guess flags.
3. Reproduce the paper claim by claim on the Hugging Face harness (`hf jobs`), one logbook page per claim. Simplified setups and toy-scale runs are allowed per the guide; record honest verdicts and compute costs.
4. Publish: `trackio logbook publish <hf-username>/<openreview-id>` (the OpenReview ID from the paper reference above; after later edits, `trackio logbook sync`).
"#;

const REPRODUCE_PAPER_TEMPLATE: &str = r#"Reproduce a research paper claim by claim on the user's compute.

Paper and compute: {args}

Before running anything:
1. Confirm the compute. The user should name where runs execute — an `~/.ssh/config` host alias (`orx exp run --backend ssh --host <alias>`, configurable in orx up Settings → Compute → SSH), another `orx` backend (`hf`, `modal`, `k8s` with a `--flavor`), or the local machine. If unspecified, ask before launching anything.
2. Read the paper. If it's on alphaXiv, `orx paper <id>` gives a structured report (`--full` for raw text); `orx lit "<query>"` can find it. Otherwise ask the user for a PDF or link.
3. Optional tracking: if the user wants metrics logged, prefer Weights & Biases — check `wandb login` / `WANDB_API_KEY` and log each run to a project named after the paper. Don't require it.

Workflow:
1. Enumerate the paper's main empirical claims (headline table/figure results first). Confirm with the user which ones to attempt.
2. Reproduce claim by claim on the agreed compute. Simplified setups and toy-scale runs are fine when full scale is out of budget — say so explicitly when you downscale.
3. For each claim record an honest verdict (reproduced / partially / not reproduced / not attempted), the evidence (numbers vs. paper's numbers), and the compute cost.
4. Finish with a summary: per-claim verdicts, where results diverged and why, and what a full-scale reproduction would still need.
"#;

pub const CATALOG: &[Skill] = &[
    Skill {
        name: "lit-review",
        description: "Multi-hop literature review via alphaXiv search",
        arg_hint: "<topic>",
        template: LIT_REVIEW_TEMPLATE,
        no_args: "(none given — ask the user what topic to review before searching)",
    },
    Skill {
        name: "hf",
        description: "Hugging Face Hub via the hf CLI (installs it if missing)",
        arg_hint: "<task>",
        template: HF_TEMPLATE,
        no_args: "(none given — ask the user what they want to do on the Hugging Face Hub)",
    },
    Skill {
        name: "icml-repro",
        description: "Reproduce an ICML 2026 paper and publish a Trackio logbook",
        arg_hint: "<paper title> (OpenReview <id>)",
        template: ICML_REPRO_TEMPLATE,
        no_args: "(none given — ask the user which ICML 2026 paper to reproduce: title plus OpenReview ID)",
    },
    Skill {
        name: "reproduce-paper",
        description: "Reproduce a paper claim by claim on compute you specify",
        arg_hint: "<paper> on <compute>",
        template: REPRODUCE_PAPER_TEMPLATE,
        no_args: "(none given — ask the user which paper to reproduce and what compute to run on)",
    },
];

/// Expand a leading `/name [args]` into the skill's full prompt. `None` when
/// the text is not a known slash-skill (sent to the harness untouched).
pub fn expand(text: &str) -> Option<String> {
    let rest = text.strip_prefix('/')?;
    let (cmd, args) = match rest.split_once(char::is_whitespace) {
        Some((cmd, args)) => (cmd, args.trim()),
        None => (rest.trim_end(), ""),
    };
    let skill = CATALOG.iter().find(|s| s.name == cmd)?;
    let args = if args.is_empty() { skill.no_args } else { args };
    Some(skill.template.replace("{args}", args))
}

#[cfg(test)]
mod tests {
    use super::expand;

    #[test]
    fn expands_known_skill_with_args() {
        let out = expand("/lit-review sparse autoencoders").unwrap();
        assert!(out.contains("Topic: sparse autoencoders"));
        assert!(out.contains("orx lit"));
    }

    #[test]
    fn expands_bare_invocation_to_ask() {
        let out = expand("/lit-review").unwrap();
        assert!(out.contains("ask the user"));
    }

    #[test]
    fn expands_hf_skill() {
        let out = expand("/hf download llama-3 weights").unwrap();
        assert!(out.contains("Task: download llama-3 weights"));
        assert!(out.contains("hf version"));
        let bare = expand("/hf").unwrap();
        assert!(bare.contains("ask the user"));
    }

    #[test]
    fn expands_icml_repro_skill() {
        let out = expand("/icml-repro Maximum Likelihood RL (OpenReview EeuLO2BjFN)").unwrap();
        assert!(out.contains("Paper: Maximum Likelihood RL (OpenReview EeuLO2BjFN)"));
        assert!(out.contains("trackio logbook publish"));
        assert!(expand("/icml-repro").unwrap().contains("ask the user"));
    }

    #[test]
    fn expands_reproduce_paper_skill() {
        let out = expand("/reproduce-paper Maximum Likelihood RL on ssh host lambda-a100").unwrap();
        assert!(out.contains("Paper and compute: Maximum Likelihood RL on ssh host lambda-a100"));
        assert!(out.contains("Confirm the compute"));
        assert!(!out.contains("trackio"));
        assert!(expand("/reproduce-paper").unwrap().contains("ask the user"));
    }

    #[test]
    fn passes_through_unknown_or_plain_text() {
        assert!(expand("/unknown thing").is_none());
        assert!(expand("hello /lit-review").is_none());
        assert!(expand("just text").is_none());
    }
}
