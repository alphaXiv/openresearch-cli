---
name: orx-reports
description: "Write a research report and publish it with `orx report upload` (list/show/download too) so it appears on the project page. Use when a line of work concludes, when the user asks for a write-up, summary, comparison, or figures, or before ending a long task — findings not written down are lost."
---

When a line of work concludes, write up the experiment tree as a local markdown
report and publish it to the project with `orx report upload` — it then shows up
on the project page (and its public view) with images inline.

## Report folder layout

The folder holds `report.md` plus an `images/` subfolder; the markdown references
images by relative path (`![](images/foo.png)`). A report's first `# ` heading
becomes its title.

Include a figure only when it plots measured outputs from completed run logs or
artifacts. There is no minimum figure count, and a report with no observed data
must contain no figures. Use prose or a compact table for protocols, lineage,
blockers, intended configurations, paper-only numbers, and missing evidence;
never manufacture a workflow, experiment-tree, or evidence-boundary diagram to
make the report look visual.

For the canonical section structure and worked layout, fetch the report skill:

```sh
orx skill report             # write a local markdown research report (with charts)
```

## Publishing and reading reports — `orx report`

```sh
orx report upload <projectId> <folder> [--title "<t>"]   # upload report.md + images/
orx report list <projectId>                              # list the project's reports
orx report show <projectId> <reportId|slug>              # print a report's markdown to stdout
orx report download <projectId> <reportId|slug> <dir>    # write report.md + images back locally
```

- **`upload`** takes a folder (`report.md` + `images/`); the report appears on the
  project page and its public view. `--title` overrides the title (otherwise the
  first `# ` heading is used).
- **`list`** shows the project's reports.
- **`show`** prints a report's markdown to **stdout** and works on any public
  project — handy for reading others' write-ups.
- **`download`** is the inverse of `upload`: it writes `report.md` plus the
  referenced images back into a local folder.
