import { devClose, devOpen, devStatus } from "../client.ts";
import { requireCredentials } from "../config.ts";

const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));
const POLL_INTERVAL_MS = 3000;
const PROVISION_TIMEOUT_MS = 5 * 60 * 1000;

interface DevOptions {
  message?: string;
  discard?: boolean;
}

export async function dev(
  sub: string | undefined,
  expId: string | undefined,
  options: DevOptions,
): Promise<void> {
  if (sub !== "open" && sub !== "close" && sub !== "status") {
    console.error("Usage: orx dev <open|close|status> <experimentId>");
    process.exit(1);
  }
  if (!expId) {
    console.error(`Usage: orx dev ${sub} <experimentId>`);
    process.exit(1);
  }
  const creds = await requireCredentials();

  if (sub === "status") {
    const s = await devStatus(creds, expId);
    console.log(`state: ${s.state}${s.sandboxId ? `  (${s.sandboxId})` : ""}`);
    if (s.state === "online") {
      if (s.dirty.length === 0) console.log("working tree clean");
      else {
        console.log(`${s.dirty.length} uncommitted change(s):`);
        for (const line of s.dirty) console.log(`  ${line}`);
      }
    }
    return;
  }

  if (sub === "close") {
    const res = await devClose(creds, expId, {
      message: options.message,
      discard: options.discard,
    });
    if (!res.tornDown) {
      console.log("No dev node was open.");
      return;
    }
    if (res.committed) console.log(`✓ Committed & pushed${res.commitSha ? ` (${res.commitSha.slice(0, 7)})` : ""}.`);
    else console.log(options.discard ? "Discarded changes." : "Nothing to commit.");
    console.log("✓ Dev node torn down.");
    return;
  }

  // open: provision (or reuse), then poll until the node is online.
  const opened = await devOpen(creds, expId);
  if (opened.state === "online") {
    console.log(`✓ Dev node ready (${opened.sandboxId}).`);
    return;
  }
  process.stdout.write("Provisioning dev node");
  const deadline = Date.now() + PROVISION_TIMEOUT_MS;
  while (Date.now() < deadline) {
    await sleep(POLL_INTERVAL_MS);
    process.stdout.write(".");
    const s = await devStatus(creds, expId);
    if (s.state === "online") {
      process.stdout.write("\n");
      console.log(`✓ Dev node ready (${s.sandboxId}).`);
      return;
    }
    if (s.state === "none" || s.state === "offline") {
      process.stdout.write("\n");
      console.error(`Provisioning failed (state: ${s.state}).`);
      process.exit(1);
    }
  }
  process.stdout.write("\n");
  console.error("Timed out waiting for the dev node to come online.");
  process.exit(1);
}
