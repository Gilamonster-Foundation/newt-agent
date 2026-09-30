import { expect, test } from "@playwright/test";
import { spawn } from "node:child_process";
import { createServer } from "node:http";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { waitUntilReady } from "./readiness.mjs";

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

test.beforeAll(async ({}, testInfo) => {
  // The default 60s hook timeout was sized for waitUntilReady alone. A cold
  // build (buildFirst) can take minutes on a loaded box; give the hook room
  // for build + readiness rather than let the *hook* time out uninformatively
  // before waitUntilReady's own, more detailed error gets a chance to fire.
  testInfo.setTimeout(5 * 60_000);
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

  // Build BEFORE timing readiness, and launch the resulting BINARY directly
  // rather than `cargo run`: `cargo run` re-invokes Cargo (lock + fingerprint
  // recheck) even against a warm target/, so readiness was still timing
  // Cargo's overhead, not just newt-web's startup. `--message-format
  // json-render-diagnostics` reports the exact `executable` path Cargo
  // produced for THIS manifest/profile/features — no guessing the target dir.
  const buildStart = Date.now();
  const executable = await buildFirst();
  console.log(`[cockpit.spec] build done in ${Date.now() - buildStart}ms: ${executable}`);

  const spawnStart = Date.now();
  appProcess = spawn(executable, [], {
    cwd: repoRoot,
    env: {
      ...process.env,
      NEWT_WEB_BIND: `127.0.0.1:${appPort}`,
      NEWT_WEB_AUTH_HEADER: "",
      NEWT_WEB_STATE_DIR: stateDir,
      NEWT_WEB_WORKSPACE: repoRoot,
    },
    // Capture BOTH streams: an empty appLog on a live, unresponsive
    // process used to be indistinguishable between "logged nothing" and
    // "logged to the stream we ignored".
    stdio: ["ignore", "pipe", "pipe"],
  });
  appProcess.once("error", (error) => {
    console.log(`[cockpit.spec] spawn error after ${Date.now() - spawnStart}ms: ${error.message}`);
  });
  appProcess.once("exit", (code, signal) => {
    console.log(`[cockpit.spec] app exited (code=${code} signal=${signal}) after ${Date.now() - spawnStart}ms`);
  });
  appProcess.stdout.on("data", (chunk) => {
    appLog += chunk.toString();
  });
  appProcess.stderr.on("data", (chunk) => {
    appLog += chunk.toString();
  });
  const readyStart = Date.now();
  await waitUntilReady(baseURL, {
    isExited: () => appProcess.exitCode,
    getLog: () => appLog,
  });
  console.log(`[cockpit.spec] spawn ok, ready after ${Date.now() - readyStart}ms`);
});

/// Compile newt-web on its own deadline, separate from `waitUntilReady`'s,
/// and return the `executable` path Cargo's own build output names for the
/// `newt-web` bin — never a guessed `target/…` path. `--message-format
/// json-render-diagnostics` still renders human diagnostics on stderr while
/// emitting one JSON object per line on stdout.
async function buildFirst() {
  const BUILD_TIMEOUT_MS = 180_000;
  return new Promise((resolve, reject) => {
    const build = spawn(
      "cargo",
      [
        "build",
        "--message-format=json-render-diagnostics",
        "--manifest-path",
        path.join(webRoot, "Cargo.toml"),
      ],
      { cwd: repoRoot, stdio: ["ignore", "pipe", "pipe"] },
    );
    let stdout = "";
    let stderrLog = "";
    build.stdout.on("data", (chunk) => (stdout += chunk.toString()));
    build.stderr.on("data", (chunk) => (stderrLog += chunk.toString()));
    const timer = setTimeout(() => {
      build.kill("SIGKILL");
      reject(new Error(`newt-web build did not finish within ${BUILD_TIMEOUT_MS}ms\n${stderrLog}`));
    }, BUILD_TIMEOUT_MS);
    // 'error' (e.g. `cargo` not found) never fires 'close', so it needs its
    // own handler or a bad spawn hangs until the timeout instead of failing
    // fast with the real reason.
    build.on("error", (error) => {
      clearTimeout(timer);
      reject(new Error(`newt-web build failed to start: ${error.message}\n${stderrLog}`));
    });
    // 'close', not 'exit': 'exit' can fire before the stdio pipes have
    // flushed their last chunks, so `stdout` could still be missing the
    // trailing compiler-artifact line at the moment this reads it.
    build.on("close", (code) => {
      clearTimeout(timer);
      if (code !== 0) {
        reject(new Error(`newt-web build failed (${code})\n${stderrLog}`));
        return;
      }
      const executable = stdout
        .split("\n")
        .filter(Boolean)
        .map((line) => {
          try {
            return JSON.parse(line);
          } catch {
            return null;
          }
        })
        .find(
          (message) =>
            message?.reason === "compiler-artifact" &&
            message.target?.name === "newt-web" &&
            message.target?.kind?.includes("bin") &&
            message.executable,
        )?.executable;
      if (!executable) {
        reject(new Error(`newt-web build reported no bin executable\n${stderrLog}`));
        return;
      }
      resolve(executable);
    });
  });
}

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
