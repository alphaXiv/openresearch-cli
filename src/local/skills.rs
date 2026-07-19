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
1. Confirm the compute. The user should name where runs execute — an `~/.ssh/config` host alias (`orx exp run --backend ssh --host <alias>`, configurable in orx up Settings → Compute), another `orx` backend (`hf`, `modal`, `k8s` with a `--flavor`), or the local machine. An explicitly named backend is already selected and authorized: use it directly and do not require a default target, provider choice, billing target, or another approval. In particular, when the caller specifies the configured Kubernetes cluster, pass `--backend k8s`; the absence of a default compute target is not a blocker. Only when the caller leaves compute unspecified should you use the default compute target configured in orx up Settings → Compute, or ask before launching if no default exists.
2. Read the paper. If it's on alphaXiv, `orx paper <id>` gives a structured report (`--full` for raw text); `orx lit "<query>"` can find it. Otherwise ask the user for a PDF or link.
3. Plan to the user's compute window. When the caller supplies an absolute deadline and available accelerator capacity, treat both as authoritative: keep the available GPUs occupied with scientifically useful parallel variants, seeds, ablations, controls, or profiling runs; refill freed capacity after each completion; and stop early when the target claims are adequately evaluated. Interpret capacity by total GPUs across in-flight runs, not by raw run count. Do not invent or maintain a GPU-hour ledger unless the user explicitly asks for one. For vague small-budget language such as "for a little bit," prefer published-checkpoint evaluation and targeted checks. Larger windows may support broader sweeps, added seeds, fine-tuning, or retraining, but they make training eligible, not mandatory.
4. Optional tracking: if the user wants metrics logged, prefer Weights & Biases — check `wandb login` / `WANDB_API_KEY` and log each run to a project named after the paper. Don't require it.

Workflow:
1. Enumerate the paper's main empirical claims (headline table/figure results first). Unless the user specifies, focus on the main illustrative claim of the paper.
2. Reproduce claim by claim on the agreed compute. Simplified setups and toy-scale runs are fine when full scale is out of budget — say so explicitly when you downscale.
3. For each claim, record the paper's result, the observed result, an assessment (aligned / partially aligned / inconclusive under this setup / not attempted), and the compute cost. When results diverge, state that this run did not show the reported effect, quantify the difference, and explain relevant uncertainty or substitutions. Do not characterize the claim as wrong, incorrect, failed, or "not reproduced," and do not infer beyond the tested setup.
4. Finish with a summary: per-claim assessments, where results diverged and why, and what a full-scale reproduction would still need.

Write a visual autoresearch report:
- Write for readers who may not understand the paper. Lead with its central question, then explain the implementation, experiments, and evidence.
- When completed runs produced measured data, place the strongest useful result plot immediately after the title. Every plotted value must be traceable to run logs or artifacts.
- There is no minimum figure count. If no completed run produced measurements, include no figures. Never turn prose, paper-only reference numbers, intended configurations, experiment-tree structure, process stages, missing evidence, or zero runs into a diagram merely to make the report visual.
- Prefer actual result plots—comparisons, training curves, robustness results, or diagnostics—from observed run data. Use a compact table or prose for protocols, lineage, blockers, and evidence boundaries rather than a synthetic infographic.
- Make the report implementation-led rather than a run log. Trace the important code path, consequential design choices, and the smallest code or configuration changes used to test them.
- Use measured plots, compact tables, and short code excerpts only when they explain the result better than prose. Avoid long uninterrupted text, repeated conclusions, and exhaustive infrastructure histories.
- Clearly separate paper evidence, observed evidence, divergent or inconclusive results, partial runs, and unattempted claims. End with a concise assessment and descriptive links to the relevant experiment branches.
- Use one clear title and normal Markdown hierarchy: H2 for major sections and H3 only for genuine subsections.
- Keep the report self-contained: store figures in an `images/` directory beside `report.md`, reference them as `images/<filename>`, and verify every image renders before publication.
- Perform a final editorial pass for clarity and concision. The result should feel like a rigorous technical report, not an experiment database dump.

Publish a polished GitHub artifact:
- Treat the repository README as the public landing page, not as an afterthought. Add a project-specific reproduction section at the very top, before any upstream README content. It must state which paper claim was tested, what was done, the assessment, the paper number versus the observed number, the downscaling/substitutions, the agreed compute, and links to the detailed report or notebook when present.
- In that top section, add a compact `Experiment log` or provenance table covering the important branches only. Use descriptive links to each branch and include columns for branch/experiment, purpose or change, **exact run command**, assessment/outcome, and compute. Copy the command verbatim from `orx exp status`; do not abbreviate it, replace it with pseudocode, or show only the entrypoint.
- Account for `main` explicitly. If a formal experiment was ever launched from `main`, include `main` in the table with the exact command and result. If `main` is presentation-only, say `Not run as an experiment (publication surface)` rather than inventing a command.
- Publish every reader-facing report on `main`, alongside the README and other small presentation artifacts. A report is not considered published if it exists only in the dashboard Files directory, a local artifact directory, an `orx/*` experiment branch, or an internal run log. Copy or recreate the final report under a clear repository path such as `reports/<topic>/report.md` or `artifacts/<topic>/report.md`, then add a descriptive link to it in the README's top reproduction section. If several reports are produced, link every important one and briefly say what each contains.
- Also publish a self-contained, tutorial-style marimo notebook on `main` that explains the central claim and opens with the already-produced evidence; do not make readers rerun expensive experiments to see the result. Validate it with `marimo check <notebook.py>`, embed small results or fetch them from public URLs instead of assuming repository-relative artifacts exist in Molab, and keep optional interactive work bounded and separate from formal reproduction evidence. If the repository is public—or the user explicitly requested public publication and its history is safe to expose—add and verify `[![Open in molab](https://marimo.io/molab-shield.svg)](https://molab.marimo.io/github/<owner>/<repo>/blob/main/<notebook.py>)` in the README. Otherwise preserve private visibility, omit the unusable Molab link, and include concise local `marimo edit <notebook.py>` and `marimo run <notebook.py>` instructions. Never change repository visibility without explicit authorization.
- Include failed branches only when they explain the lineage to the successful result. Keep raw experiment and run IDs in `orx exp desc`, not in the README.
- Keep the README current whenever another important branch is run or its assessment changes. A reader landing on GitHub should be able to understand what was tried and reproduce the command without inspecting the internal experiment database.
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
2. Confirm the compute if the user did not specify it: the default compute target from orx up Settings → Compute when one is set (omit `--backend` to launch there), or an explicit backend — `hf` or `modal` with a flavor, `k8s` with a committed manifest, or `ssh` with a host alias. Formal reproduction runs must use `orx exp run`; molab's GPU is for the notebook's short teaching experiment, not untracked reproduction runs.
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
4. Launch with `orx exp run <experiment-id> --backend <backend> ...` (omit `--backend` to use the configured default target).
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
- Design specifically for molab's attached RTX PRO 6000: the lab should materially benefit from GPU acceleration, not be a scalar/vector toy that runs just as well on CPU.
- Prefer a genuine compact-model workload—such as batched inference, activation hooks, a layer × token intervention sweep, or probe training. A bounded one-time model download is acceptable when it is central to the lesson.
- Target roughly 5–500 seconds on the RTX PRO 6000 after cached setup and remain comfortably below ten minutes. Bound samples, steps, downloads, and memory; a CPU fallback may run a reduced, clearly labeled version.
- Let the reader manipulate a conceptually meaningful variable and produce a visible change in a plot or metric.
- Label synthetic and toy experiments explicitly; never present them as reproduction evidence.
- Do not rerun the paper's full-scale experiment merely because molab has a strong GPU.

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

Treat the README as the polished public landing page. At the very top, before any upstream README content, add a concise reproduction overview explaining the selected claim, what was done, the verdict, the paper number versus the reproduced number, the main substitutions/downscaling, the compute used, and links to the molab tutorial and detailed report.

Immediately below that overview, add a compact `Experiment log` or provenance table so a reader can understand what was tried without consulting `orx`. Include only the important branches, link each branch descriptively, and use columns for branch/experiment, purpose or change, **exact run command**, verdict/outcome, and compute. Copy each command verbatim from `orx exp status`; do not abbreviate it, replace it with pseudocode, or show only the entrypoint. Include failed attempts only when they explain the experimental lineage, and omit raw experiment and run IDs.

Account for `main` explicitly in this top section. If any formal experiment was launched from `main`, list `main` with the exact command and result. If `main` is only the maintained publication surface, state `Not run as an experiment (publication surface)` rather than inventing a command. Keep this table current whenever another important branch is run or its verdict changes.

Publish every reader-facing report on `main`, alongside the README, notebook, and other small presentation artifacts. A report is not considered published if it exists only in the dashboard Files directory, a local artifact directory, an `orx/*` experiment branch, or an internal run log. Copy or recreate each final report under a clear repository path such as `reports/<topic>/report.md` or `artifacts/<topic>/report.md`, and add a descriptive link to every important report in the README's top reproduction section with a short explanation of what it contains.

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
        assert!(out.contains("before any upstream README content"));
        assert!(out.contains("exact run command"));
        assert!(out.contains("Not run as an experiment (publication surface)"));
        assert!(out.contains("every reader-facing report on `main`"));
        assert!(out.contains("not considered published"));
        assert!(out.contains("strongest useful result plot immediately after the title"));
        assert!(out.contains("Every plotted value must be traceable to run logs or artifacts"));
        assert!(out.contains("There is no minimum figure count"));
        assert!(out.contains("If no completed run produced measurements, include no figures"));
        assert!(out.contains("zero runs into a diagram merely to make the report visual"));
        assert!(out.contains("Use a compact table or prose for protocols"));
        assert!(out.contains("implementation-led rather than a run log"));
        assert!(out.contains("images/<filename>"));
        assert!(out.contains("rigorous technical report"));
        assert!(out.contains("inconclusive under this setup"));
        assert!(out.contains("this run did not show the reported effect"));
        assert!(out.contains("Do not characterize the claim as wrong"));
        assert!(out.contains("per-claim assessments"));
        assert!(out.contains("absolute deadline and available accelerator capacity"));
        assert!(out.contains("total GPUs across in-flight runs"));
        assert!(out.contains("Do not invent or maintain a GPU-hour ledger"));
        assert!(out.contains("An explicitly named backend is already selected and authorized"));
        assert!(out.contains("the absence of a default compute target is not a blocker"));
        assert!(out.contains("published-checkpoint evaluation"));
        assert!(out.contains("training eligible, not mandatory"));
        assert!(out.contains("self-contained, tutorial-style marimo notebook"));
        assert!(out.contains("marimo check <notebook.py>"));
        assert!(out.contains("already-produced evidence"));
        assert!(
            out.contains("https://molab.marimo.io/github/<owner>/<repo>/blob/main/<notebook.py>")
        );
        assert!(out.contains("preserve private visibility"));
        assert!(out.contains("marimo edit <notebook.py>"));
        assert!(out.contains("Never change repository visibility without explicit authorization"));
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
        assert!(out.contains("materially benefit from GPU acceleration"));
        assert!(out.contains("5–500 seconds"));
        assert!(out.contains("blob/main/<notebook.py>"));
        assert!(out.contains("Molab opens the notebook from GitHub but does not clone"));
        assert!(out.contains("Never use molab as a test runner"));
        assert!(out.contains("Molab is only the delivery surface"));
        assert!(out.contains("marimo's native reactive widgets"));
        assert!(out.contains("ask permission to make it public"));
        assert!(out.contains("do not make the user perform the change manually"));
        assert!(out.contains("Do not publish raw experiment or run IDs"));
        assert!(out.contains("Confirmatory reproduction code"));
        assert!(out.contains("before any upstream README content"));
        assert!(out.contains("exact run command"));
        assert!(out.contains("Not run as an experiment (publication surface)"));
        assert!(out.contains("every reader-facing report on `main`"));
        assert!(out.contains("not considered published"));
        assert!(expand("/paper-to-marimo").unwrap().contains("ask the user"));
    }

    #[test]
    fn passes_through_unknown_or_plain_text() {
        assert!(expand("/unknown thing").is_none());
        assert!(expand("hello /lit-review").is_none());
        assert!(expand("just text").is_none());
    }
}
