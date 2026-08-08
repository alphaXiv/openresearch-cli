---
name: orx-compute
description: "Launch committed experiments, wait for completion, and inspect logs. Use before launching or repairing any run."
---

Commit the experiment branch first, then launch on the configured backend:

```sh
orx exp status <expId>
orx exp run <expId>
orx exp wait --project <projectId>
orx runs <projectId>
orx logs <runId>
```

The runner archives the recorded commit and transfers that exact snapshot to
the backend. Uncommitted changes are excluded. A run returns
immediately; use `orx exp wait --project` as a wake-up signal and reconcile
terminal state with `orx runs` after every wake.

Remote compute does not require publication or repository credentials. Provider
credentials and backend-specific flags are still required where applicable.
