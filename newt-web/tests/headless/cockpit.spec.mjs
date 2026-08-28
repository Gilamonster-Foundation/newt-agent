import { expect, test } from "@playwright/test";
import { spawn } from "node:child_process";
import { createServer } from "node:http";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";

const webRoot = fileURLToPath(new URL("../..", import.meta.url));
const repoRoot = path.resolve(webRoot, "..");

let appProcess;
let backend;
let baseURL;
let backendURL;
let stateDir;
let appLog = "";

function listen(server) {
  return new Promise((resolve, reject) => {
    server.once("error", reject);
    server.listen(0, "127.0.0.1", () => {
      server.off("error", reject);
      resolve(server.address().port);
    });
  });
}

async function reservePort() {
  const server = createServer();
  const port = await listen(server);
  await new Promise((resolve) => server.close(resolve));
  return port;
}

async function waitUntilReady(url) {
  const deadline = Date.now() + 45_000;
  while (Date.now() < deadline) {
    if (appProcess.exitCode !== null) {
      throw new Error(`newt-web exited before readiness (${appProcess.exitCode})\n${appLog}`);
    }
    try {
      const response = await fetch(`${url}/healthz`);
      if (response.ok) return;
    } catch (_error) {
      // The listener is still starting.
    }
    await new Promise((resolve) => setTimeout(resolve, 100));
  }
  throw new Error(`newt-web did not become ready\n${appLog}`);
}

test.beforeAll(async () => {
  const reply = [
    "# Portable result",
    "",
    "**Markdown survives.**",
    "",
    "```mermaid",
    "flowchart TD",
    "  A[Harness] --> B[Markdown]",
    "  B --> C[Mobile GUI]",
    "```",
    "",
    "<script>alert('not allowed')</script>",
  ].join("\n");

  backend = createServer((_request, response) => {
    response.writeHead(200, { "content-type": "application/json" });
    response.end(JSON.stringify({
      model: "acceptance-model",
      message: { role: "assistant", content: reply },
      done: true,
    }));
  });
  const backendPort = await listen(backend);
  backendURL = `http://127.0.0.1:${backendPort}`;

  const appPort = await reservePort();
  baseURL = `http://127.0.0.1:${appPort}`;
  stateDir = await mkdtemp(path.join(tmpdir(), "newt-web-acceptance-"));
  appProcess = spawn(
    "cargo",
    ["run", "--quiet", "--manifest-path", path.join(webRoot, "Cargo.toml")],
    {
      cwd: repoRoot,
      env: {
        ...process.env,
        NEWT_WEB_BIND: `127.0.0.1:${appPort}`,
        NEWT_WEB_AUTH_HEADER: "",
        NEWT_WEB_STATE_DIR: stateDir,
        NEWT_WEB_WORKSPACE: repoRoot,
      },
      stdio: ["ignore", "ignore", "pipe"],
    },
  );
  appProcess.stderr.on("data", (chunk) => {
    appLog += chunk.toString();
  });
  await waitUntilReady(baseURL);
});

test.afterAll(async () => {
  if (appProcess && appProcess.exitCode === null) {
    appProcess.kill("SIGTERM");
    await new Promise((resolve) => appProcess.once("exit", resolve));
  }
  if (backend) await new Promise((resolve) => backend.close(resolve));
  if (stateDir) await rm(stateDir, { recursive: true, force: true });
});

test("BAT: the cockpit falls back to diagram source under the CSP @bat", async ({ page }) => {
  await page.goto(baseURL);
  await expect(page).toHaveTitle("newt-web");
  await expect(page.getByText("No agents yet. Spawn one above.")).toBeVisible();

  // The shell serves a strict CSP (#1854). Mermaid themes its diagrams with a
  // <style> element it injects, scoped to a per-render id, and it has no nonce
  // support — so `style-src-elem 'nonce-…'` blocks it and there is no hash or
  // stylesheet that could admit it.
  //
  // Measured: with that stylesheet blocked, BOTH node fill and text fill fall
  // back to black, so a rendered diagram is black-on-black — unreadable, not
  // merely unstyled, and silently so. `markdown.js` therefore feature-detects
  // whether an injected <style> applies and, when it does not, leaves the
  // diagram as its own source: readable, labelled, and honest (ADR law 5).
  await page.evaluate(async () => {
    const host = document.createElement("div");
    host.className = "md";
    host.innerHTML =
      '<pre class="mermaid" data-markdown-extension="mermaid">flowchart LR\nA --> B</pre>';
    document.body.append(host);
    await window.newtEnhanceMarkdown(host);
  });

  const diagram = page.locator('[data-markdown-extension="mermaid"]');
  await expect(diagram).toHaveClass(/mermaid-error/);
  await expect(diagram).toHaveAttribute("aria-label", /security policy/i);
  // The source is still READABLE — the whole point of the fallback.
  await expect(diagram).toContainText("flowchart LR");
  await expect(page.locator('[data-markdown-extension="mermaid"] svg')).toHaveCount(0);
});

test("BAT: an invalid diagram still falls back to its source @bat", async ({ page }) => {
  await page.goto(baseURL);
  await page.evaluate(async () => {
    const host = document.createElement("div");
    host.className = "md";
    host.innerHTML =
      '<pre class="mermaid" data-markdown-extension="mermaid">not a diagram !?</pre>';
    document.body.append(host);
    await window.newtEnhanceMarkdown(host);
  });
  const fallback = page.locator(".mermaid-error");
  await expect(fallback).toHaveText("not a diagram !?");
  await expect(fallback).toHaveAttribute("aria-label", /.+/);
});

test("UAT: a phone-sized user drives a Markdown turn with no CSP violations @uat", async ({ page }) => {
  // Every CSP violation the page provokes, so a policy that silently breaks
  // the surface cannot pass. Before C3b this page had no policy at all.
  const violations = [];
  await page.addInitScript(() => {
    document.addEventListener("securitypolicyviolation", (e) =>
      (window.__cspViolations = window.__cspViolations || []).push(
        e.violatedDirective + " :: " + e.blockedURI,
      ),
    );
  });

  await page.setViewportSize({ width: 390, height: 844 });
  await page.goto(baseURL);
  await page.getByText("+ new scratch agent").click();
  await page.getByLabel("name", { exact: true }).fill("acceptance");
  await page.getByLabel("backend url", { exact: true }).fill(backendURL);
  await page.getByLabel("model", { exact: true }).fill("acceptance-model");
  await page.getByLabel("workspace", { exact: true }).fill(repoRoot);
  await page.getByRole("button", { name: "spawn" }).click();

  await expect(page.locator(".agent h2")).toContainText("acceptance");
  await page.getByPlaceholder("prompt…").fill("Show the portable flow");
  await page.getByRole("button", { name: "send" }).click();

  await expect(page.locator(".transcript strong")).toHaveText("Markdown survives.");
  // The diagram is present as readable source, and the injected <script> the
  // model sent is gone.
  const diagram = page.locator('.transcript [data-markdown-extension="mermaid"]');
  await expect(diagram).toContainText("flowchart TD");
  await expect(page.locator(".transcript")).not.toContainText("not allowed");

  // The enhanced path also resets the prompt box — behaviour that used to live
  // in an `hx-on::` attribute, which htmx EVALUATES and which therefore
  // required `script-src 'unsafe-eval'`. It moved into `assets/panel.js`.
  await expect(page.getByPlaceholder("prompt…")).toHaveValue("");

  violations.push(...(await page.evaluate(() => window.__cspViolations || [])));
  expect(violations, `CSP violations on the shell page: ${violations.join(", ")}`).toEqual([]);

  const layout = await page.evaluate(() => ({
    pageWidth: document.documentElement.scrollWidth,
    viewportWidth: document.documentElement.clientWidth,
  }));
  expect(layout.pageWidth).toBeLessThanOrEqual(layout.viewportWidth + 1);
});
