import { randomUUID } from "node:crypto";
import { createServer } from "node:http";
import type { AddressInfo } from "node:net";
import { openBrowser } from "../browser.ts";
import { DEFAULT_API_URL, saveCredentials } from "../config.ts";

const SUCCESS_HTML = `<!doctype html><meta charset="utf-8"><title>OpenResearch CLI</title>
<body style="font-family:system-ui;display:grid;place-items:center;height:100vh;margin:0">
<div style="text-align:center">
<h1>You're logged in</h1><p>You can close this tab and return to your terminal.</p>
</div></body>`;

const LOGIN_TIMEOUT_MS = 5 * 60 * 1000;

interface LoginOptions {
  apiUrl?: string;
}

/**
 * Loopback OAuth: spin up a throwaway HTTP listener on a random localhost port,
 * send the user through the browser to `{api}/auth/cli/login`, and wait for the
 * API to redirect back here with a freshly minted personal access token.
 */
export async function login(options: LoginOptions): Promise<void> {
  const apiUrl = options.apiUrl ?? DEFAULT_API_URL;
  const nonce = randomUUID();

  const token = await new Promise<string>((resolve, reject) => {
    const server = createServer((req, res) => {
      const url = new URL(req.url ?? "/", "http://127.0.0.1");
      if (url.pathname !== "/callback") {
        res.writeHead(404).end();
        return;
      }
      const received = url.searchParams.get("token");
      const state = url.searchParams.get("state");
      if (!received || state !== nonce) {
        res.writeHead(400, { "content-type": "text/plain" }).end("Invalid login response.");
        reject(new Error("Login failed: invalid or mismatched response from the server."));
        cleanup();
        return;
      }
      res.writeHead(200, { "content-type": "text/html" }).end(SUCCESS_HTML);
      resolve(received);
      cleanup();
    });

    const timeout = setTimeout(() => {
      reject(new Error("Login timed out after 5 minutes."));
      cleanup();
    }, LOGIN_TIMEOUT_MS);

    function cleanup() {
      clearTimeout(timeout);
      server.close();
    }

    server.on("error", (err) => {
      reject(err);
      cleanup();
    });

    server.listen(0, "127.0.0.1", () => {
      const { port } = server.address() as AddressInfo;
      const loginUrl = `${apiUrl}/auth/cli/login?port=${port}&state=${nonce}`;
      console.log("Opening your browser to log in…");
      console.log(`If it doesn't open, visit:\n  ${loginUrl}\n`);
      openBrowser(loginUrl);
    });
  });

  await saveCredentials({ apiUrl, token });
  console.log("✓ Logged in. Credentials saved.");
}
