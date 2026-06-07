import { type DevFsOp, devFs } from "../client.ts";
import { requireCredentials } from "../config.ts";

async function readStdin(): Promise<string> {
  const chunks: Buffer[] = [];
  for await (const chunk of process.stdin) chunks.push(chunk as Buffer);
  return Buffer.concat(chunks).toString("utf8");
}

/**
 * The file-edit verbs (read/write/str-replace/ls/grep/rm). Each maps to one
 * `dev/fs` op against the experiment's live dev working tree. All require an
 * open dev node (`orx dev open`); the API returns a clear error otherwise.
 */
export async function fsCommand(
  verb: string,
  expId: string | undefined,
  rest: string[],
): Promise<void> {
  if (!expId) {
    console.error(`Usage: orx ${verb} <experimentId> ...`);
    process.exit(1);
  }
  const creds = await requireCredentials();

  let op: DevFsOp;
  switch (verb) {
    case "read":
      if (!rest[0]) return usage(`orx read <experimentId> <path>`);
      op = { op: "read", path: rest[0] };
      break;
    case "write":
      if (!rest[0]) return usage(`orx write <experimentId> <path>   (content on stdin)`);
      op = { op: "write", path: rest[0], content: await readStdin() };
      break;
    case "str-replace":
      if (rest.length < 3)
        return usage(`orx str-replace <experimentId> <path> <old_string> <new_string>`);
      op = { op: "str_replace", path: rest[0]!, old_string: rest[1]!, new_string: rest[2]! };
      break;
    case "ls":
      op = rest[0] ? { op: "list", path: rest[0] } : { op: "list" };
      break;
    case "grep":
      if (!rest[0]) return usage(`orx grep <experimentId> <pattern>`);
      op = { op: "search", query: rest[0] };
      break;
    case "rm":
      if (!rest[0]) return usage(`orx rm <experimentId> <path>`);
      op = { op: "delete", path: rest[0] };
      break;
    default:
      return usage(`unknown file command: ${verb}`);
  }

  const { output } = await devFs(creds, expId, op);
  console.log(output);
}

function usage(msg: string): never {
  console.error(`Usage: ${msg}`);
  process.exit(1);
}
