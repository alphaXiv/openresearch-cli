---
name: orx-lit
description: "Search literature and read papers via alphaXiv (`orx lit` / `orx paper`). Use when grounding a hypothesis, hunting related work, baselines, or code to seed from, when the user mentions a paper, author, or arXiv id, or before designing a novel experiment."
---

These tap **alphaXiv's public corpus** (2.5M+ arXiv papers: CS, math, physics,
stats, q-bio/fin, EE — not PubMed/biomed). They need **no `orx login`** and hit
alphaXiv hosts, not the OpenResearch API. Use them to ground research in real
literature: find related work, pull a paper's structured report, and only drop to
its full text when you need a specific equation/table/section.

```sh
orx lit "speculative decoding for LLMs"            # ranked hits (id, title, date, votes, abstract)
orx lit "rotary position embeddings" --limit 10    # widen the result set (default 5)
orx lit "kv cache compression" --json              # raw JSON for programmatic use
orx paper 2401.12345                               # machine-readable report (the default)
orx paper https://arxiv.org/abs/2401.12345         # any arXiv/alphaXiv URL works too
orx paper 2401.12345v2 --full                      # full extracted text (fallback)
```

- **`orx lit`** prints, per hit: `<paperId>  <title>`, then `<date> · <votes> votes`,
  then a truncated abstract. The **`paperId`** is what you feed to `orx paper`.
  Results are relevance-ranked, capped at `--limit` (default 5). `--json` emits the
  raw hit objects (incl. matched `snippets`) for piping.
- **`orx paper <id>`** writes the report markdown to **stdout** (pipe/redirect-friendly).
  The id can be a bare id (`2401.12345`), a versioned id (`2401.12345v2`), or an
  arXiv / alphaXiv URL — the CLI normalizes it.
- **The paper's code: `GitHub: <url>` line.** When alphaXiv has a GitHub repo linked
  to the paper, `orx paper` prints it as the first line (with `--full` too). If the
  report leaves you with questions about *how* something was actually implemented —
  exact hyperparameters, training loop details, a trick the paper glosses over —
  clone the repo into a temp dir and read the code:

  ```sh
  dir=$(mktemp -d) && git clone --depth 1 <githubUrl> "$dir"
  ```

  Inspect it there (grep for the model/optimizer setup, read the configs), and rely
  on it as the ground truth for reproducing the paper. No line means no repo is
  linked. Note the linked repo is the most-starred one associated with the paper —
  occasionally a big framework rather than the paper's own code; sanity-check the
  repo name before leaning on it.
- **Report first, full text only when needed.** The default report is a compact
  (~10 KB) structured analysis and is enough for most questions. Reach for `--full`
  only when the report is missing a specific detail — it returns the entire paper.
- **404s are normal answers, not errors of yours.** A paper whose report hasn't been
  generated yet exits non-zero with a hint to try `--full`; one with no extracted
  text yet points you at the arXiv PDF. Try `--full`, then the PDF, before giving up.
- Override hosts with `ALPHAXIV_API_URL` (search) and `ALPHAXIV_WEB_URL` (paper docs)
  if you ever need to point elsewhere.

**Grounding a research loop in literature.** Before forming hypotheses for a project
(step 2 of the auto-research loop), search the literature for prior art on the knob
you're about to vary, pull the most relevant report, and let it inform the change you
write into a child's description:

```sh
orx lit "learning rate warmup schedules transformers" --limit 5
orx paper <bestPaperId>          # read its report; cite the idea in the child's --description
```
