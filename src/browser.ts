import { spawn } from "node:child_process";

/** Opens a URL in the user's default browser, cross-platform. */
export function openBrowser(url: string): void {
  const command =
    process.platform === "darwin" ? "open" : process.platform === "win32" ? "start" : "xdg-open";
  // `start` is a shell builtin on Windows, so it needs a shell; the others don't.
  const child = spawn(command, [url], {
    shell: process.platform === "win32",
    stdio: "ignore",
    detached: true,
  });
  child.on("error", () => {
    // Non-fatal: we already printed the URL for the user to open manually.
  });
  child.unref();
}
