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
1. Enumerate the paper's main empirical claims (headline table/figure results first). Unless the user specifies, focus on the main illustrative claim of the paper.
2. Reproduce claim by claim on the agreed compute. Simplified setups and toy-scale runs are fine when full scale is out of budget — say so explicitly when you downscale.
3. For each claim record an honest verdict (reproduced / partially / not reproduced / not attempted), the evidence (numbers vs. paper's numbers), and the compute cost.
4. Finish with a summary: per-claim verdicts, where results diverged and why, and what a full-scale reproduction would still need.
"#;

const PAPER_TO_MARIMO_TEMPLATE: &str = r#"Reproduce a research paper's main illustrative claim and publish it as a self-contained, tutorial-style marimo notebook that opens in molab.

Paper, compute, and preferences: {args}

The final deliverable is:
- a reproducible research result produced through `orx`;
- a tutorial-style marimo notebook on the repository's `main` branch;
- a public molab link from the GitHub README;
- an optional short interactive experiment that uses molab's RTX PRO 6000 GPU.

Before running anything:
1. Inspect the project with `orx projects`, `orx runs <project-id>`, `git branch -a`, and relevant `orx exp desc <experiment-id>` entries so you extend existing work instead of duplicating it.
2. Confirm the compute if the user did not specify it: `hf` or `modal` with an explicit flavor, `k8s` with a committed manifest, or `ssh` with an explicit host alias. Formal reproduction runs must use `orx exp run`; molab's GPU is for the notebook's short teaching experiment, not untracked reproduction runs.
3. Read the paper. For alphaXiv papers use `orx paper <id>` and use `--full` when the structured report omits an important detail. Use `orx lit "<query>"` to locate related work or public implementations.
4. Enumerate the main empirical claims, prioritizing the headline table or figure. Unless the user asks for broader coverage, select the single claim that makes the clearest illustrative tutorial.
5. Inspect repository visibility and history before publication. Molab's GitHub opener requires a public repository. If the repository is private and the user has not already authorized a visibility change, explain this requirement, ask permission to make it public, and stop until the user approves. After approval, scan the complete Git history for credentials or private artifacts, change visibility with `gh`, and continue the workflow; do not make the user perform the change manually.

Git and experiment structure:
- Immutable `orx/*` branches hold experiment nodes and their exact code.
- The experiment root must never be the `main` branch.
- `main` is the maintained public presentation surface containing the README, notebook, and small published artifacts.
- Do not require GitHub releases or version tags unless the user asks for them. The canonical molab link should point to `main`.
- Never mutate an experiment root after it has been run. Create child experiments for every code or configuration variant.

orx never binds a new experiment root to `main` — every node gets its own `orx/*` branch — so `main` is free for publication by default. Only a legacy project may have a root riding `main` (orx prints a warning when touching one); in that case do not silently edit it: publish through a dedicated documentation branch and flag the needed migration at handoff.

Reproduce the claim:
1. Create a child experiment for the selected claim.
2. Encode all parameters in committed code or configuration and keep the inherited run command unchanged.
3. Commit and push before launching; remote jobs clone the pushed branch.
4. Launch with `orx exp run <experiment-id> --backend <backend> ...`.
5. Hold the turn open with `orx exp wait` until the run is terminal, then read the evidence with `orx logs <run-id>`.
6. Record findings immediately with `orx exp desc`.

Downscale when necessary, but state exactly how the setup differs from the paper. Record an honest verdict — reproduced, partially reproduced, not reproduced, or not attempted — plus the paper's number, reproduced number, sample size, model/data substitutions, scoring method, and compute cost. Keep internal run IDs in `orx exp desc`, not in reader-facing materials.

Build the marimo notebook:
- Create a valid marimo Python notebook, normally `claim_tutorial.py`, that works standalone when opened directly by molab.
- Make it an illustrated tutorial, not a run log.
- Show useful frozen results immediately; never make the reader run an expensive cell just to see the conclusion.
- Use marimo's native reactive widgets — sliders, dropdowns, selectors, tables, and run buttons — when they make the paper's mechanism or result meaningfully easier to explore.

Use this narrative structure:
1. Title and question — explain the claim in plain language and why it matters.
2. Paper result — show the reported number and setup and link to the paper.
3. Reproduction result — display the verdict, headline metrics, sample sizes, and closest like-for-like comparison.
4. Visual explanation — prefer an informative calibration curve, heatmap, layer sweep, intervention plot, or compact table. Explain axes and colors; avoid decoration that does not clarify the claim.
5. Robustness and interpretation — include negative controls, leakage checks, or sensitivity analyses when available, and explain what would falsify the interpretation.
6. Limitations and provenance — distinguish reproduced evidence from paper evidence, list substitutions and unattempted claims, and link to the exact GitHub experiment branches containing the runnable code and configuration. Include approximate compute cost.
7. Optional interactive GPU lab — provide a small teaching experiment related to the claim.

Make provenance reader-facing:
- Do not publish raw experiment or run IDs in the notebook or README; use descriptive branch links instead.
- Use descriptive links such as `[Confirmatory reproduction code](<GitHub branch/tree URL>)` and `[Layer-sweep code](<GitHub branch/tree URL>)` as the primary references.
- Briefly say what each linked branch contains: runner, fixed configuration, manifest, and evaluation method.
- If a public run or dashboard URL exists, link it with a descriptive label; otherwise the branch link is sufficient.
- Prefer a small provenance table with columns for experiment, code, verdict, and compute over a paragraph of opaque identifiers.

Interactive GPU lab requirements:
- Put expensive work behind a marimo run button so it never starts automatically.
- Detect CUDA with `torch.cuda.is_available()` and report the selected device.
- Design for molab's attached RTX PRO 6000, target a few seconds, and remain comfortably below two minutes.
- Bound sample counts, optimization steps, downloads, and memory use; provide a CPU fallback when practical.
- Let the reader manipulate a conceptually meaningful variable and produce a visible change in a plot or metric.
- Label synthetic and toy experiments explicitly; never present them as reproduction evidence.
- Do not download or run the paper's full model merely because molab has a strong GPU. The interactive section should teach the mechanism, not repeat a multi-hour reproduction.

Good interactive examples include varying probe signal strength and viewing calibration, selecting layers and updating an activation heatmap, changing intervention strength and plotting the effect, or training a tiny linear probe on already-published activations.

Auxiliary files in molab:
Molab opens the notebook from GitHub but does not clone the repository. Repository-relative paths do not exist unless the notebook creates them. Choose the simplest publication method:
1. Embed small results directly in the notebook: headline metrics, short tables, compact heatmap matrices, and small JSON-like records.
2. For medium artifacts, commit them to Git and download them in an early setup cell from `raw.githubusercontent.com`. Pin the URL to a commit SHA when reproducibility matters, create the destination directory, cache downloads, and verify a recorded SHA-256 checksum. Fail with a clear message if an artifact is unavailable.
3. Use a public artifact host such as a Hugging Face dataset for files too large for ordinary Git.

Never assume that a relative path such as `data/results.json` already exists in molab. Prefer embedding data when that keeps the notebook genuinely single-file. Declare lightweight dependencies using marimo-compatible inline script metadata when needed. Do not force-install a CUDA-specific PyTorch wheel over molab's GPU environment; use the provided PyTorch installation when available.

Validate before publication:
1. Run `marimo check <notebook.py>`.
2. Copy only the notebook into a clean temporary directory and export or execute it there without repository files.
3. Confirm initial load does not start expensive computation and that all embedded outputs, tables, and figures render.
4. Confirm every auxiliary download uses a public URL and checksum.
5. Do not execute formal training or evaluation on the local edit machine.
6. Never use molab as a test runner. If the interactive path needs execution, validate the same code and dependencies on the user's agreed external backend through `orx exp run`, and capture the device, runtime, and result in `orx` logs. Molab is only the delivery surface. Never claim GPU validation when only static checks were performed.

Publish on GitHub:
Publish the notebook and README on `main`, provided `main` is not an experiment root. Add this single-line README badge, replacing the placeholders:
`[![Open in molab](https://marimo.io/molab-shield.svg)](https://molab.marimo.io/github/<owner>/<repo>/blob/main/<notebook.py>)`

Also add a short `Experiment log` to the README so a reader landing in the repository can understand what was tried. Include only the important branches, link each branch descriptively, and summarize its change and outcome; omit raw experiment and run IDs, and include failed attempts only when they explain the experimental lineage.

After pushing:
1. Verify the repository is public. If it is still private and permission has not been granted, explain that the molab link cannot work anonymously, ask permission to make the repository public, and stop. Once permission is granted, make it public with `gh` and verify the new visibility before continuing.
2. Fetch the raw notebook anonymously and require HTTP 200.
3. Open the molab URL anonymously and require HTTP 200.
4. Confirm the returned notebook contains the expected title and interactive section.
5. Point the repository homepage at the molab URL when appropriate.

Do not create a GitHub release merely to obtain an immutable URL. Reproducibility comes from experiment branches, pinned artifact commits, and Git history; `main` remains the friendly canonical entry point.

Finish by reporting:
- the molab and GitHub URLs;
- the claim and verdict;
- paper numbers versus reproduced numbers;
- formal reproduction compute cost;
- what the notebook embeds or downloads;
- whether the optional GPU cell was actually executed and on what device;
- limitations and what a full-scale reproduction would still require.

Do not stop after creating the notebook. Finish only after the reproduction is analyzed, the notebook is validated, public links work, and provenance is recorded in the experiment tree.
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
    Skill {
        name: "paper-to-marimo",
        description: "Reproduce a paper and publish an interactive molab tutorial",
        arg_hint: "<paper> on <compute>",
        template: PAPER_TO_MARIMO_TEMPLATE,
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
    fn expands_paper_to_marimo_skill() {
        let out =
            expand("/paper-to-marimo What LLM Forecasters Know but Don't Say on k8s").unwrap();
        assert!(out.contains(
            "Paper, compute, and preferences: What LLM Forecasters Know but Don't Say on k8s"
        ));
        assert!(out.contains("RTX PRO 6000"));
        assert!(out.contains("blob/main/<notebook.py>"));
        assert!(out.contains("Molab opens the notebook from GitHub but does not clone"));
        assert!(out.contains("Never use molab as a test runner"));
        assert!(out.contains("Molab is only the delivery surface"));
        assert!(out.contains("marimo's native reactive widgets"));
        assert!(out.contains("ask permission to make it public"));
        assert!(out.contains("do not make the user perform the change manually"));
        assert!(out.contains("Do not publish raw experiment or run IDs"));
        assert!(out.contains("Confirmatory reproduction code"));
        assert!(out.contains("short `Experiment log`"));
        assert!(expand("/paper-to-marimo").unwrap().contains("ask the user"));
    }

    #[test]
    fn passes_through_unknown_or_plain_text() {
        assert!(expand("/unknown thing").is_none());
        assert!(expand("hello /lit-review").is_none());
        assert!(expand("just text").is_none());
    }
}
