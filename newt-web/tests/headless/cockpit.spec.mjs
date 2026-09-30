import { expect, test } from "@playwright/test";
import { spawn } from "node:child_process";
import { createServer } from "node:http";
import net from "node:net";
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

const READY_TIMEOUT_MS = 45_000;

/// The last `maxLines` lines of a log — so a runaway or noisy process can't
/// make the diagnostic itself unusable.
function logTail(log, maxLines = 50) {
  return log.split("\n").slice(-maxLines).join("\n");
}

/// `isExited`/`getLog` default to the real `appProcess`/`appLog`, and are
/// overridable so this loop's deadline behaviour is testable without a real
/// `newt-web` process (see the accept-without-response test below).
async function waitUntilReady(url, opts = {}) {
  const timeoutMs = opts.timeoutMs ?? READY_TIMEOUT_MS;
  const isExited = opts.isExited ?? (() => appProcess.exitCode);
  const getLog = opts.getLog ?? (() => appLog);
  const start = Date.now();
  const deadline = start + timeoutMs;
  while (Date.now() < deadline) {
    const exitCode = isExited();
    if (exitCode !== null) {
      throw new Error(
        `newt-web exited before readiness (${exitCode}) after ${Date.now() - start}ms\n${logTail(getLog())}`,
      );
    }
    try {
      // Bounded by the REMAINING overall deadline, not an unbounded fetch:
      // a socket that accepts but never answers (the failure this readiness
      // check exists for) must not stop the loop from rechecking its
      // deadline and reporting elapsed time + log tail.
      const response = await fetch(`${url}/healthz`, {
        signal: AbortSignal.timeout(Math.max(deadline - Date.now(), 1)),
      });
      if (response.ok) return;
    } catch (_error) {
      // Still starting, or this probe itself hit the remaining deadline.
    }
    await new Promise((resolve) => setTimeout(resolve, 100));
  }
  throw new Error(
    `newt-web did not become ready after ${Date.now() - start}ms (deadline ${timeoutMs}ms)\n` +
      `last log output:\n${logTail(getLog()) || "(empty — see stdio capture)"}`,
  );
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

  // Build BEFORE timing readiness. A cold or invalidated target/ can take
  // well over 45s to compile under `--quiet` (which prints nothing while
  // compiling), and that build time was being counted against the
  // readiness deadline: the empty-appLog, still-running failure this
  // regresses was newt-web still compiling, not newt-web hanging. Building
  // first, on its own generous deadline, narrows that gap — the following
  // `cargo run` can still wait on Cargo's lock or a fingerprint recheck, so
  // waitUntilReady is not guaranteed a strictly warm-binary startup, just a
  // much shorter one.
  await buildFirst();

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
      // Capture BOTH streams: an empty appLog on a live, unresponsive
      // process used to be indistinguishable between "logged nothing" and
      // "logged to the stream we ignored".
      stdio: ["ignore", "pipe", "pipe"],
    },
  );
  appProcess.stdout.on("data", (chunk) => {
    appLog += chunk.toString();
  });
  appProcess.stderr.on("data", (chunk) => {
    appLog += chunk.toString();
  });
  await waitUntilReady(baseURL);
});

/// Compile newt-web on its own deadline, separate from `waitUntilReady`'s.
/// `--quiet` prints nothing while compiling, so its own output cannot show
/// progress; a build that hangs or fails still reports exit code and output.
async function buildFirst() {
  const BUILD_TIMEOUT_MS = 180_000;
  await new Promise((resolve, reject) => {
    const build = spawn(
      "cargo",
      ["build", "--quiet", "--manifest-path", path.join(webRoot, "Cargo.toml")],
      { cwd: repoRoot, stdio: ["ignore", "pipe", "pipe"] },
    );
    let log = "";
    build.stdout.on("data", (chunk) => (log += chunk.toString()));
    build.stderr.on("data", (chunk) => (log += chunk.toString()));
    const timer = setTimeout(() => {
      build.kill("SIGKILL");
      reject(new Error(`newt-web build did not finish within ${BUILD_TIMEOUT_MS}ms\n${log}`));
    }, BUILD_TIMEOUT_MS);
    // 'error' (e.g. `cargo` not found) never fires 'close', so it needs its
    // own handler or a bad spawn hangs until the timeout instead of failing
    // fast with the real reason.
    build.on("error", (error) => {
      clearTimeout(timer);
      reject(new Error(`newt-web build failed to start: ${error.message}\n${log}`));
    });
    // 'close', not 'exit': 'exit' can fire before the stdio pipes have
    // flushed their last chunks, so `log` could still be missing trailing
    // output at the moment this reads it.
    build.on("close", (code) => {
      clearTimeout(timer);
      if (code === 0) resolve();
      else reject(new Error(`newt-web build failed (${code})\n${log}`));
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

test("waitUntilReady reports elapsed time and a bounded log tail at its deadline", async () => {
  // Accepts the TCP connection but never writes a response — the exact
  // failure mode an unbounded `fetch` can't distinguish from "still
  // starting": the socket is live, so the loop must keep rechecking its
  // deadline instead of hanging on one probe.
  const stuck = net.createServer((_socket) => {});
  const port = await listen(stuck);
  const manyLines = Array.from({ length: 60 }, (_, i) => `line ${i}`).join("\n");

  let caught;
  try {
    await waitUntilReady(`http://127.0.0.1:${port}`, {
      timeoutMs: 300,
      isExited: () => null,
      getLog: () => manyLines,
    });
  } catch (error) {
    caught = error;
  } finally {
    await new Promise((resolve) => stuck.close(resolve));
  }

  expect(caught?.message).toMatch(/did not become ready after \d+ms \(deadline 300ms\)/);
  expect(caught?.message).toContain("line 59");
  expect(caught?.message).not.toContain("line 0\n");
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
