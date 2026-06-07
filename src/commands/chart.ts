import { mkdir, writeFile } from "node:fs/promises";
import { homedir } from "node:os";
import { join } from "node:path";
import { renderWandbChart, type WandbChartResult } from "../client.ts";
import { requireCredentials } from "../config.ts";

const USAGE =
  'Usage: orx chart wandb <projectId> --metric "<key>" --run <runId>[:label] [--run ...] [--smoothing <0-0.99>] [--out <dir>]';

/** Where rendered PNGs land by default — a stable cache dir an agent can re-read. */
function cacheDir(): string {
  const base = process.env["XDG_CACHE_HOME"] ?? join(homedir(), ".cache");
  return join(base, "openresearch", "charts");
}

/** Compact numeric formatting matching the assistant's chart summaries. */
function fmt(n: number): string {
  const abs = Math.abs(n);
  if (abs === 0) return "0";
  if (abs >= 10000 || abs < 0.001) return n.toExponential(3);
  return n.toPrecision(4);
}

/** Parse a `--run <id>[:label]` spec into its run id and optional legend label. */
function parseRun(spec: string): { runId: string; label?: string } {
  const idx = spec.indexOf(":");
  if (idx === -1) return { runId: spec };
  const label = spec.slice(idx + 1);
  return { runId: spec.slice(0, idx), label: label || undefined };
}

function printSummary(result: WandbChartResult): void {
  console.log(`Metric: ${result.metricKey}`);
  for (const s of result.summaries) {
    console.log(`  ${s.label}: n=${s.n}, min=${fmt(s.min)}, max=${fmt(s.max)}, last=${fmt(s.last)}`);
  }
  if (result.failed.length > 0) {
    console.log("Skipped:");
    for (const f of result.failed) console.log(`  ${f.label}: ${f.error}`);
  }
}

/**
 * Render a W&B metric across runs to a PNG and save it locally. The server does
 * the rendering (W&B fetch + chart + R2); we download the result so a
 * vision-capable agent can `Read` the file to see the chart, while the printed
 * summary lets a text-only agent reason without ever opening the image.
 */
export async function chart(
  sub: string | undefined,
  projectId: string | undefined,
  opts: { metric?: string; run?: string[]; smoothing?: string; out?: string },
): Promise<void> {
  if (sub !== "wandb" || !projectId || !opts.metric || !opts.run || opts.run.length === 0) {
    console.error(USAGE);
    process.exit(1);
  }

  let smoothing: number | undefined;
  if (opts.smoothing != null) {
    smoothing = Number(opts.smoothing);
    if (Number.isNaN(smoothing) || smoothing < 0 || smoothing > 0.99) {
      console.error("--smoothing must be a number between 0 and 0.99");
      process.exit(1);
    }
  }

  const creds = await requireCredentials();
  const result = await renderWandbChart(creds, projectId, {
    metricKey: opts.metric,
    runs: opts.run.map(parseRun),
    smoothing,
  });

  printSummary(result);

  if (!result.url || !result.chartId) {
    console.error(`\nNo chart rendered for '${result.metricKey}' — see skipped runs above.`);
    process.exit(1);
  }

  // Download the PNG so the agent can Read it from disk.
  const res = await fetch(result.url);
  if (!res.ok) {
    console.error(`\nFailed to download chart image (${res.status} ${res.statusText}).`);
    process.exit(1);
  }
  const bytes = Buffer.from(await res.arrayBuffer());

  const dir = opts.out ?? cacheDir();
  await mkdir(dir, { recursive: true });
  const slug =
    result.metricKey
      .replace(/[^a-z0-9]+/gi, "-")
      .replace(/^-+|-+$/g, "")
      .toLowerCase() || "metric";
  const file = join(dir, `${slug}-${result.chartId.slice(0, 8)}.png`);
  await writeFile(file, bytes);

  console.log(`\nChart: ${file}`);
  console.log("Read this PNG file to view the chart.");
}
