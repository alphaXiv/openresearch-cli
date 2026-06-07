import { homedir } from "node:os";
import { dirname, join } from "node:path";
import { mkdir, readFile, rm, writeFile } from "node:fs/promises";

// Where the API lives. Override per-invocation with `--api-url` or persist it in
// the OPENRESEARCH_API_URL env var. Defaults to local dev; point this at the
// production API host for real use.
export const DEFAULT_API_URL = process.env["OPENRESEARCH_API_URL"] ?? "http://localhost:4000";

export interface Credentials {
  apiUrl: string;
  token: string;
}

function configDir(): string {
  const base = process.env["XDG_CONFIG_HOME"] ?? join(homedir(), ".config");
  return join(base, "openresearch");
}

function credentialsPath(): string {
  return join(configDir(), "credentials.json");
}

export async function loadCredentials(): Promise<Credentials | null> {
  try {
    const raw = await readFile(credentialsPath(), "utf8");
    const parsed = JSON.parse(raw) as Partial<Credentials>;
    if (!parsed.apiUrl || !parsed.token) return null;
    return { apiUrl: parsed.apiUrl, token: parsed.token };
  } catch {
    return null;
  }
}

export async function saveCredentials(creds: Credentials): Promise<void> {
  const path = credentialsPath();
  await mkdir(dirname(path), { recursive: true });
  // Mode 0600: token is a bearer secret — keep it owner-only.
  await writeFile(path, `${JSON.stringify(creds, null, 2)}\n`, { mode: 0o600 });
}

export async function clearCredentials(): Promise<void> {
  await rm(credentialsPath(), { force: true });
}

/** Loads stored credentials or exits with a helpful message. */
export async function requireCredentials(): Promise<Credentials> {
  const creds = await loadCredentials();
  if (!creds) {
    console.error("Not logged in. Run `orx login` first.");
    process.exit(1);
  }
  return creds;
}
