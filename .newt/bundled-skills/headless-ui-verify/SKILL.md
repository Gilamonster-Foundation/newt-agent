---
name: headless-ui-verify
description: See a GUI/desktop-app change actually work on a machine with no display — run the app under a virtual X display (Xvfb), drive it, screenshot the window, and read the PNG back.
when_to_use: When you changed a desktop GUI (Tauri, Electron, GTK/Qt, or any X11 app) on a headless box and need visual proof it works — not just green tests. Also when a unit/mock test proves the wiring but you want to confirm the rendered result, or when a human keeps asking "does it actually look right?".
version: 1.0.0
license: Apache-2.0
caveats:
  exec: { only: ["Xvfb", "xdpyinfo", "dbus-launch", "dbus-run-session", "import", "ffmpeg", "python3", "bash", "curl"] }
  fs_read: all
  net: { only: ["127.0.0.1"] }
  max_calls: unlimited
---

# Verify a GUI headlessly (Xvfb + screenshot + read-back)

A non-interactive agent on a server has no display, so a desktop GUI can't open
a window you can see — and tmux won't help (it drives *terminals*, not GTK/WebKit
windows; for TUIs use `tmux-drive`). But you don't need a real display: run the
app on a **virtual framebuffer (Xvfb)**, drive it however it's normally driven
(CLI, IPC, MCP), capture the virtual screen to a PNG, and **read the PNG back**.
That last step is the whole point — you are looking at the real rendered app.

## What it gives you

`unit test → "the function returns live:true"`. This skill →
`"the tab actually appeared and the diagram rendered"`. Use both: the test is the
deterministic gate, the screenshot is the truth.

## One-time host setup

The virtual-display + capture stack (Debian/Ubuntu):

```bash
sudo apt install -y xvfb xauth x11-utils dbus-x11 libgl1-mesa-dri imagemagick
```

Plus **the app's own build/runtime deps**. A headless box often lacks the whole
GTK/WebKit stack — check before you build. For a Tauri app on Ubuntu 24.04:

```bash
sudo apt install -y libwebkit2gtk-4.1-dev libgtk-3-dev libglib2.0-dev \
  libayatana-appindicator3-dev librsvg2-dev libdbus-1-dev libssl-dev patchelf pkg-config
# probe what's missing first:  pkg-config --exists webkit2gtk-4.1 || echo MISSING
```

## The loop

```
virtual display → session D-Bus + software GL → launch app
  → wait for a REAL readiness signal → drive it → screenshot → READ the png
```

1. **Virtual display.** `Xvfb :99 -screen 0 1400x900x24 &` then
   `export DISPLAY=:99`. Confirm with `xdpyinfo >/dev/null` before continuing.
2. **Session D-Bus + software GL** (see gotchas — GUIs die without these headless).
3. **Launch** the app in the background; capture its stdout/stderr to a log.
4. **Wait for readiness** — a socket, a listening port, or a log line the app
   emits when up. Never a fixed `sleep` (see gotchas).
5. **Drive** it the way it's really used — a CLI subcommand, an IPC/RPC call, an
   MCP `tools/call`, or `xdotool` for raw clicks/keys.
6. **Screenshot the root window:** `import -window root out.png` (ImageMagick).
7. **Read `out.png`** with your file-reading tool. This is the verification —
   look at it and confirm the change. Do not skip to "probably fine."

## The gotchas that will bite you

- **WebKit/Chromium render black or crash under Xvfb without software GL.** Export
  before launch: `LIBGL_ALWAYS_SOFTWARE=1`, `WEBKIT_DISABLE_COMPOSITING_MODE=1`,
  `WEBKIT_DISABLE_DMABUF_RENDERER=1`. (Electron: add `--disable-gpu`.)
- **GTK/WebKit apps need a session D-Bus.** Wrap with `dbus-run-session -- <app>`
  or `eval "$(dbus-launch --sh-syntax)"` first, else the app aborts on startup.
- **A DEBUG Tauri/Electron build loads a DEV URL, not your built assets.** A
  debug `cargo build`/`electron .` points the webview at `http://localhost:<port>`
  (Tauri `devUrl`, e.g. 5173). With no dev server the window shows *"Could not
  connect to localhost / Connection refused"* — a blank/error page, and you'll
  wrongly think the app is broken. Fix: build the frontend and **serve it on that
  port** — `( cd app/dist && python3 -m http.server 5173 --bind 127.0.0.1 ) &` —
  or do a **release build** (which embeds the assets). The Tauri runtime still
  injects its IPC into whatever URL the webview loads, so a static server works.
- **Wait for a real signal, not a sleep.** Poll for the thing that proves "up":
  `for _ in $(seq 1 60); do [ -S ~/.app/sock ] && break; sleep 0.5; done`, or
  `curl -sf localhost:PORT`, or `grep -q "Ready" app.log`. Fixed sleeps flake.
- **Blank screenshot ≠ failure.** WebKit-under-Xvfb is occasionally uncooperative.
  Before concluding, check the app's console/debug log for the event you drove
  (e.g. the "opened file X" line). If the log shows the action landed, the
  wiring works even if the pixels didn't; note it and retry the shot.
- **Always clean up.** `trap` a cleanup that kills the app, the D-Bus daemon, and
  Xvfb, and removes any socket — otherwise a zombie Xvfb/app leaks across runs.

## Complete harness (adapt the four env vars)

```bash
#!/usr/bin/env bash
set -uo pipefail
APP="${APP_BIN:?path to the app binary}"        # e.g. target/debug/my-app
APP_ARGS=(${APP_ARGS:-})                        # launch args (file to open, etc.)
READY="${READY_PROBE:-true}"                    # cmd that exits 0 when app is up
DRIVE="${DRIVE_CMD:-true}"                       # cmd that drives the app
OUT="${OUT:-/tmp/ui-shot.png}"
FE_DIST="${FRONTEND_DIST:-}"; FE_PORT="${FRONTEND_PORT:-5173}"  # Tauri/Electron dev-URL case
DISP=":99"

cleanup(){ kill "${APP_PID:-0}" "${FE_PID:-0}" "${DBUS_SESSION_BUS_PID:-0}" "${XVFB_PID:-0}" 2>/dev/null; }
trap cleanup EXIT

Xvfb "$DISP" -screen 0 1400x900x24 -nolisten tcp >/tmp/xvfb.log 2>&1 & XVFB_PID=$!
export DISPLAY="$DISP"; sleep 1
xdpyinfo >/dev/null 2>&1 || { echo "Xvfb failed"; cat /tmp/xvfb.log; exit 3; }
eval "$(dbus-launch --sh-syntax)"
export LIBGL_ALWAYS_SOFTWARE=1 WEBKIT_DISABLE_COMPOSITING_MODE=1 WEBKIT_DISABLE_DMABUF_RENDERER=1

[ -n "$FE_DIST" ] && { ( cd "$FE_DIST" && exec python3 -m http.server "$FE_PORT" --bind 127.0.0.1 ) >/tmp/fe.log 2>&1 & FE_PID=$!; sleep 1.5; }

"$APP" "${APP_ARGS[@]}" >/tmp/app.log 2>&1 & APP_PID=$!
ok=0; for _ in $(seq 1 60); do bash -c "$READY" && { ok=1; break; }; sleep 0.5; done
[ "$ok" = 1 ] || { echo "app never became ready:"; tail -30 /tmp/app.log; exit 4; }

bash -c "$DRIVE"
sleep 2
import -window root "$OUT" && echo "screenshot: $OUT   (now READ it back)"
```

Then read `$OUT` and state what you see. If it matches the intended change,
you've verified it; if not, you've caught a bug the tests missed.
