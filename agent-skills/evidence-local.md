In local mode (`orx up`) run **logs are the only evidence channel** — there is no
`artifacts`, `artifact`, `query`, `chart`, `search-logs`, or `wandb`. Make the run
command print everything you'll need to judge the result, then read it back with
`orx logs`.

## Reading run logs — `orx logs`

A run's terminal output (the PTY stream) is captured live while it runs and
persisted afterwards.

```sh
orx logs <runId>                    # tail (the end — usually what you want)
orx logs <runId> --head             # read from the start instead
orx logs <runId> --bytes 200000     # raise the byte cap (default 64 KB, max 1 MB)
orx logs <runId> --range 4096:8192  # exact byte window [start, end)
```

- The log goes to **stdout** (pipe/redirect-friendly); a `[source] bytes a–b of N`
  status line goes to **stderr**, noting if content was truncated above/below.
- `<runId>` comes from `orx runs <projectId>` (the run id, not the experiment id).

## Make the run print its own evidence

Run logs are the only evidence channel in local mode. Make the run command
print everything you'll need to stdout — final metrics, an `EVAL.md`-style
summary, key config — and read it back with `orx logs <runId>` (use `--head` /
`--range` for long logs). **If a run's output isn't in its log, it's lost.**

- Print final metrics and a compact summary block at the end of the run, not just
  scattered mid-training lines — that's what you'll tail to compare siblings.
- Echo the key config the run actually used (LR, batch size, seed) so a log alone
  tells you which variant it was.
- For a long run, a periodic one-line-per-step metric print keeps the trajectory
  visible via `orx logs --range`; a run that only prints a final number hides
  whether it was converging or diverging.
