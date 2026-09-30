import { expect, test } from "@playwright/test";
import net from "node:net";
import { waitUntilReady } from "./readiness.mjs";

// No beforeAll here: this drives waitUntilReady's deadline logic directly
// against a fake socket, independent of cockpit.spec.mjs's real app boot —
// so it can't be starved by (or mistaken for evidence about) that boot.

test("waitUntilReady reports elapsed time and a bounded log tail at its deadline @bat", async () => {
  // Accepts the TCP connection but never writes a response — the exact
  // failure mode an unbounded `fetch` can't distinguish from "still
  // starting": the socket is live, so the loop must keep rechecking its
  // deadline instead of hanging on one probe.
  const sockets = new Set();
  const stuck = net.createServer((socket) => {
    sockets.add(socket);
    socket.on("close", () => sockets.delete(socket));
  });
  const port = await new Promise((resolve, reject) => {
    stuck.once("error", reject);
    stuck.listen(0, "127.0.0.1", () => resolve(stuck.address().port));
  });
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
    // Destroy any still-accepted sockets before closing — `server.close()`
    // only stops new connections; it waits for existing ones to end on
    // their own, and this server's whole point is a socket that never does.
    for (const socket of sockets) socket.destroy();
    await new Promise((resolve) => stuck.close(resolve));
  }

  expect(caught?.message).toMatch(/did not become ready after \d+ms \(deadline 300ms\)/);
  expect(caught?.message).toContain("line 59");
  expect(caught?.message).not.toContain("line 0\n");
});
