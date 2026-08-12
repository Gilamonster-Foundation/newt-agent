#!/usr/bin/env node
// stub-ollama.mjs — a deterministic, offline Ollama stand-in for drive.sh.
//
// The dock drive harness (newt-web-drive.yml) drives the REAL newt TUI +
// newt-web cockpit; it must NOT reach a real model or the network. This stub
// speaks just enough of the Ollama HTTP API for newt to (1) discover the
// endpoint, (2) adopt a model, and (3) complete a chat turn with a
// deterministic, echo-shaped reply the harness can assert on.
//
// Contract consumed by newt (verified against the branch under test):
//   * GET  /api/tags   — reachability probe + model list
//                        (newt-inference `LocalOllamaBackend::probe`,
//                         newt-core `backend_probe::fetch_ollama_models`).
//   * POST /api/show   — context window (newt-core
//                        `backend_probe::parse_ollama_show_window`:
//                        model_info.*context_length / parameters `num_ctx`).
//   * GET  /api/ps     — warm (loaded) models.
//   * POST /api/chat   — `stream:false`, single JSON reply
//                        `{message:{content}, prompt_eval_count, eval_count}`
//                        (newt-inference `LocalOllamaBackend::try_complete`).
//   * POST /api/generate, GET /api/version — served for completeness.
//
// The reply shape the harness asserts on (drive.sh):
//   STUB_REPLY ok — echo: <the last user message, verbatim>
// e.g. "hello from the driver" → "STUB_REPLY ok — echo: hello from the driver".
//
// On listen it prints one line to stdout:  STUB_OLLAMA_READY <url>
// (drive.sh greps for STUB_OLLAMA_READY and takes field 2 as the base URL).
//
// NOTE: this file is deliberately committed (see the .gitignore negation for
// newt-web/tests/drive/*.mjs) — the blanket `*.mjs` ignore previously swallowed
// it, which broke the drive lane on every clean checkout.

import http from "node:http";

const MODEL = "stub:latest";
// A large declared context window: newt treats the /api/show window as
// authoritative and refuses to dispatch when the prompt + advertised tool
// schemas (~10k tokens) exceed the input budget. 8192 is far too small; a
// realistic large window lets the real turn reach /api/chat.
const CTX = 131072;
const EM_DASH = "—"; // — : keep the reply byte-exact for the harness grep.

// The one model this stub "serves", in Ollama /api/tags shape.
const TAG = {
  name: MODEL,
  model: MODEL,
  modified_at: "2026-01-01T00:00:00Z",
  size: 1,
  digest: "0".repeat(64),
  details: {
    parent_model: "",
    format: "gguf",
    family: "stub",
    families: ["stub"],
    parameter_size: "1B",
    quantization_level: "Q4_0",
  },
};

function readBody(req) {
  return new Promise((resolve) => {
    let buf = "";
    req.on("data", (c) => (buf += c));
    req.on("end", () => resolve(buf));
  });
}

// The prompt to echo: the last user turn (fall back to the last message, then "").
function lastUserPrompt(body) {
  try {
    const msgs = JSON.parse(body || "{}").messages;
    if (Array.isArray(msgs) && msgs.length) {
      for (let i = msgs.length - 1; i >= 0; i--) {
        if (msgs[i] && msgs[i].role === "user") return String(msgs[i].content ?? "");
      }
      return String(msgs[msgs.length - 1].content ?? "");
    }
  } catch {
    /* fall through to generate-style prompt */
  }
  try {
    return String(JSON.parse(body || "{}").prompt ?? "");
  } catch {
    return "";
  }
}

function json(res, obj, code = 200) {
  const payload = JSON.stringify(obj);
  res.writeHead(code, { "content-type": "application/json" });
  res.end(payload);
}

const server = http.createServer(async (req, res) => {
  const url = (req.url || "").split("?")[0];
  const method = req.method || "GET";
  // Trace to stderr → drive.sh keeps it in $WORK/stub.err for debugging.
  process.stderr.write(`stub: ${method} ${url}\n`);

  if (method === "GET" && url === "/api/tags") {
    return json(res, { models: [TAG] });
  }
  if (method === "GET" && url === "/api/version") {
    return json(res, { version: "0.0.0-stub" });
  }
  if (method === "GET" && url === "/api/ps") {
    // Report the model warm so adoption needs no load.
    return json(res, { models: [{ ...TAG, size_vram: 1, expires_at: "2999-01-01T00:00:00Z" }] });
  }
  if (method === "POST" && url === "/api/show") {
    return json(res, {
      license: "stub",
      modelfile: `FROM ${MODEL}\nPARAMETER num_ctx ${CTX}`,
      parameters: `num_ctx ${CTX}`,
      template: "{{ .Prompt }}",
      details: TAG.details,
      model_info: { "general.context_length": CTX, "stub.context_length": CTX },
    });
  }
  if (method === "POST" && (url === "/api/chat" || url === "/api/generate")) {
    const body = await readBody(req);
    const content = `STUB_REPLY ok ${EM_DASH} echo: ${lastUserPrompt(body)}`;
    const common = {
      model: MODEL,
      created_at: "2026-01-01T00:00:00Z",
      done: true,
      done_reason: "stop",
      total_duration: 1,
      load_duration: 1,
      prompt_eval_count: 1,
      prompt_eval_duration: 1,
      eval_count: 1,
      eval_duration: 1,
    };
    if (url === "/api/chat") {
      return json(res, { ...common, message: { role: "assistant", content } });
    }
    return json(res, { ...common, response: content }); // /api/generate shape
  }

  // Unknown path: a benign 200 so an incidental probe never fails the run.
  return json(res, {});
});

server.listen(0, "127.0.0.1", () => {
  const { port } = server.address();
  // The line drive.sh waits for; field 2 is the base URL.
  process.stdout.write(`STUB_OLLAMA_READY http://127.0.0.1:${port}\n`);
});

for (const sig of ["SIGINT", "SIGTERM"]) {
  process.on(sig, () => {
    server.close();
    process.exit(0);
  });
}
