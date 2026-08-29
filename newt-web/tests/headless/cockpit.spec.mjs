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

test("BAT: a diagram renders server-side in the page's own ink @bat", async ({ page }) => {
  await page.goto(baseURL);
  await expect(page).toHaveTitle("newt-web");

  // E0b (#1869): diagrams are drawn server-side and arrive as SVG in the
  // transcript. There is no client runtime to enhance them with — Mermaid
  // could not draw under the strict CSP C3b shipped, and a blocked theme
  // rendered black-on-black.
  const ink = await page.evaluate(() => {
    const host = document.createElement("div");
    host.className = "md";
    // Exactly what the server emits for a supported fence.
    host.innerHTML =
      '<figure class="diagram"><svg viewBox="0 0 100 50" role="img" aria-label="d">' +
      '<rect x="1" y="1" width="40" height="20" fill="none" stroke="currentColor"/>' +
      '<text x="20" y="14" fill="currentColor">A</text></svg></figure>';
    document.body.append(host);
    const page_ink = getComputedStyle(document.body).color;
    return {
      page_ink,
      stroke: getComputedStyle(host.querySelector("rect")).stroke,
      text: getComputedStyle(host.querySelector("text")).fill,
    };
  });

  // **Readability, not presence.** The diagram's ink resolves to the PAGE's
  // own foreground colour, so it cannot be invisible against the page
  // background — which is precisely what black-on-black was.
  expect(ink.stroke).toBe(ink.page_ink);
  expect(ink.text).toBe(ink.page_ink);
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
  // The diagram is DRAWN, and drawn readably: its ink is the page's own
  // foreground, so it cannot be invisible against the page background. This
  // is the assertion the black-on-black regression needed — the old test
  // asserted a diagram was PRESENT and stayed green over an unreadable one.
  const svg = page.locator('.transcript .diagram svg');
  await expect(svg).toBeVisible();
  const legible = await page.evaluate(() => {
    const rect = document.querySelector(".transcript .diagram svg rect");
    const text = document.querySelector(".transcript .diagram svg text");
    return {
      page_ink: getComputedStyle(document.body).color,
      stroke: rect ? getComputedStyle(rect).stroke : null,
      text_fill: text ? getComputedStyle(text).fill : null,
      label: text ? text.textContent : null,
    };
  });
  expect(legible.stroke).toBe(legible.page_ink);
  expect(legible.text_fill).toBe(legible.page_ink);
  expect(legible.label).toBeTruthy();
  // …and the adjacent accessible text travels with it.
  await expect(page.locator(".transcript .diagram figcaption")).toHaveCount(1);
  // The injected <script> the model sent is still gone.
  await expect(page.locator(".transcript")).not.toContainText("not allowed");

  // The enhanced path still resets the prompt box — behaviour that used to
  // live in an `hx-on::` attribute, which htmx EVALUATES and which therefore
  // required `script-src 'unsafe-eval'`. It lives in `assets/panel.js`.
  await expect(page.getByPlaceholder("prompt…")).toHaveValue("");

  violations.push(...(await page.evaluate(() => window.__cspViolations || [])));
  expect(violations, `CSP violations on the shell page: ${violations.join(", ")}`).toEqual([]);

  const layout = await page.evaluate(() => ({
    pageWidth: document.documentElement.scrollWidth,
    viewportWidth: document.documentElement.clientWidth,
  }));
  expect(layout.pageWidth).toBeLessThanOrEqual(layout.viewportWidth + 1);
});
