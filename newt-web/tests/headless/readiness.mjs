// Shared with cockpit.spec.mjs (boots the real app) and readiness-deadline.spec.mjs
// (drives waitUntilReady's deadline logic against a fake socket, no app boot).

export const READY_TIMEOUT_MS = 45_000;

/// The last `maxLines` lines of a log — so a runaway or noisy process can't
/// make the diagnostic itself unusable.
export function logTail(log, maxLines = 50) {
  return log.split("\n").slice(-maxLines).join("\n");
}

/// `isExited`/`getLog` default to the real `appProcess`/`appLog`, and are
/// overridable so this loop's deadline behaviour is testable without a real
/// `newt-web` process.
export async function waitUntilReady(url, opts = {}) {
  const timeoutMs = opts.timeoutMs ?? READY_TIMEOUT_MS;
  const isExited = opts.isExited;
  const getLog = opts.getLog;
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
