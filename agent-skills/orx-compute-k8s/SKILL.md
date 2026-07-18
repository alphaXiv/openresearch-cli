---
name: orx-compute-k8s
description: "Run an experiment on your own Kubernetes cluster (`orx exp run --backend k8s`): the committed-manifest contract orx enforces at submit. Use when the user names k8s, kubernetes, or a cluster, before writing or editing `.orx/k8s.yaml`, for multi-node or Indexed Jobs, or when a k8s submit is rejected."
---

**Use `--backend k8s` ONLY when the user explicitly asks to run on their
cluster** ("run this on k8s", "use our cluster") or it is the configured
default target. Local projects (`orx up`) only for now. Auth comes from the
local kubeconfig — orx never stores cluster credentials; the context/namespace
live in `orx up` Settings → Compute.

**There are no flavors: the run's shape is a Kubernetes manifest you commit
on the experiment branch** (default `.orx/k8s.yaml`, or `--manifest <path>`).
Inspect the cluster yourself (`kubectl get nodes`, allocatable resources, GPU
products) and write whatever the run needs — a single-pod GPU Job, an Indexed
Job spanning nodes with a headless Service, an auxiliary inference Deployment.
The manifest inherits through the experiment tree like all code; changing it is
a commit, visible in the diff like any experimental variable.

```sh
orx exp run <expId> --backend k8s                    # runs .orx/k8s.yaml from the branch tip
orx exp run <expId> --backend k8s --manifest infra/run.yaml --timeout 8h
```

A minimal manifest:

```yaml
apiVersion: batch/v1
kind: Job
metadata:
  name: train-{{ORX_RUN}}
spec:
  template:
    spec:
      restartPolicy: Never
      containers:
        - name: run
          image: pytorch/pytorch:2.6.0-cuda12.4-cudnn9-runtime
          command: ["bash", "-c", "$ORX_SCRIPT"]
          resources:
            requests: { nvidia.com/gpu: "4", cpu: "32", memory: "128Gi" }
            limits: { nvidia.com/gpu: "4" }
```

The contract orx enforces at submit (loud, before anything runs):
- **Exactly one Job** — its completion/failure is the run's outcome. With
  several Jobs, label the primary `orx-primary: "true"`. Other resources
  (Services, Deployments, ConfigMaps) ride along; cancel deletes exactly what
  the manifest created.
- **Some container of that Job must run `$ORX_SCRIPT`** — the env var orx
  injects with the clone-and-run script (branch tip + the experiment's fixed
  run command). Set `command: ["bash", "-c", "$ORX_SCRIPT"]`. The manifest
  shapes *where* the command runs, never *what* runs.
- Every resource needs `metadata.name` (no `generateName`) and no foreign
  `metadata.namespace`. Use `{{ORX_RUN}}` in names — orx substitutes a
  run-unique token so re-runs don't collide.
- orx injects run labels, the `orx-env` Secret (synced env + `HF_TOKEN` /
  `GITHUB_TOKEN`) on the primary Job's containers, and defaults for
  `activeDeadlineSeconds` (from `--timeout`, default 4h; a manifest-set value
  wins), `ttlSecondsAfterFinished`, and `backoffLimit: 0`. Auxiliary
  resources that need the env reference the `orx-env` Secret themselves.
- The run log follows the primary Job's **leader pod** (completion index 0
  for Indexed Jobs, else its sole pod) — make it print the evidence; other
  pods stay reachable via `kubectl logs`. Cross-node traffic rides the pod
  network — fine for loosely-coupled work (async RL, parameter-server);
  tightly-coupled per-step all-reduce wants a fast fabric the cluster may not
  have.
- Everything downstream is identical (`orx exp wait` / `orx runs` /
  `orx logs`, cancel via `orx exp cancel`). A detached `orx supervise`
  watches the Job via kubectl; don't kill it.
