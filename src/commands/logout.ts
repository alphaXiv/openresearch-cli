import { clearCredentials, loadCredentials } from "../config.ts";

export async function logout(): Promise<void> {
  const creds = await loadCredentials();
  if (!creds) {
    console.log("Not logged in.");
    return;
  }
  await clearCredentials();
  console.log("✓ Logged out. Local credentials removed.");
}
