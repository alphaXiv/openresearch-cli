---
name: orx-reports
description: "Write research reports into the local project's files dir (tree-mirroring folder layout) so they appear in the dashboard's Files tab. Use when a line of work concludes, when the user asks for a write-up, summary, comparison, or figures, or before ending a long task — findings not written down are lost."
---

In local mode (`orx up`), reports are written **directly into the project's files
dir** — there is no upload step. Anything under the files dir (reports, figures,
data files) appears in the dashboard's Files tab immediately, grouped by
experiment. The files dir path is shown in your session playbook (the "Files dir:"
line at the top).

When a line of work concludes (or the user asks for a write-up), write a report
whose layout mirrors the experiment tree — every top-level folder is named for an
experiment slug:

- **Per-experiment output** goes in the folder named for its slug:
  `<files-dir>/<experiment-slug>/report.md`, plus an `images/` subfolder for any
  figures it references by relative path. One experiment, one folder — its
  `report.md` is that experiment's findings.
- **Cross-experiment syntheses** and anything not tied to one node (comparisons,
  lit reviews) go under the reserved `project/` namespace as their own report
  folders: `<files-dir>/project/<topic>/report.md`.

A report's first `# ` heading becomes its title. The markdown references images by
relative path (`![](images/foo.png)`). There is no upload step — save the files and
they show up in the Files tab, grouped by experiment.

Include a figure only when it plots measured outputs from completed run logs or
artifacts. There is no minimum figure count, and a report with no observed data
must contain no figures. Use prose or a compact table for protocols, lineage,
blockers, intended configurations, paper-only numbers, and missing evidence;
never manufacture a workflow, experiment-tree, or evidence-boundary diagram to
make the report look visual.
