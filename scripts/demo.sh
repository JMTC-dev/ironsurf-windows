#!/usr/bin/env bash
# Screenshot the DevTools panel on a live page → docs/screenshots/devtools.png
# (+ devtools-network.png).
#
# Starts a tiny localhost fixture (console.log/warn/error, a broken script,
# a bad DOM call, invalid CSS, and a valid 1×1 PNG <img>), launches the real
# shell + tab on $DISPLAY with software rendering, drives it with synthetic
# xdotool input (URL bar → Enter → click page → F12), captures the window
# with ImageMagick `import`, then tears everything down.
#
# Needs: $DISPLAY (Xvfb-ish box, default :2.0), xdotool, import, python3, and
# a built workspace (`cargo build --workspace`). No GPU; no network beyond
# 127.0.0.1. The timed `--smoke` contract would exit after 3 s, so this run
# stays open; IRONSURF_SMOKE=1 marks the CI-smoke convention.
set -u
cd "$(dirname "$0")/.."
export PATH="$HOME/.cargo/bin:$PATH"
export DISPLAY="${DISPLAY:-:2.0}"

DATA="$(mktemp -d /tmp/ironsurf-demo.XXXX)"
mkdir -p docs/screenshots

# ---- localhost fixture ------------------------------------------------------
python3 - "$DATA" <<'PY' &
import http.server, socketserver, sys, base64
png = base64.b64decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==")
PAGE = """<!doctype html>
<html><head><title>DevTools demo</title>
<style>body { invalid-property: invalid-value }</style></head>
<body style="margin:0;background:#20242b;color:#d8ded9;font-family:sans-serif">
<h1 style="padding:24px 32px 8px">IronSurf — DevTools demo</h1>
<p style="padding:0 32px">This page logs to the console, breaks a script, makes a bad DOM call, carries invalid CSS, and loads an image.</p>
<img src="/img.png" width="40" height="40" alt="sub">
<script>console.log("hello from the page");console.warn("a warning");console.error("something broke");</script>
<script>notAFunction();</script>
<script>document.querySelector("[");</script>
</body></html>"""
class H(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        if self.path in ("/", "/index.html"):
            body, ctype, code = PAGE.encode(), "text/html", 200
        elif self.path == "/img.png":
            body, ctype, code = png, "image/png", 200
        else:
            body, ctype, code = b"nope", "text/plain", 404
        self.send_response(code)
        self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)
    def log_message(self, *a): pass
srv = socketserver.TCPServer(("127.0.0.1", 0), H)
open(sys.argv[1] + "/port", "w").write(str(srv.server_address[1]))
srv.serve_forever()
PY
FIXTURE=$!
trap 'kill $FIXTURE 2>/dev/null' EXIT
for i in $(seq 50); do [ -s "$DATA/port" ] && break; sleep 0.1; done
PORT=$(cat "$DATA/port" 2>/dev/null)
[ -n "$PORT" ] || { echo "fixture server failed" >&2; exit 1; }
echo "fixture: http://127.0.0.1:$PORT/"

# ---- launch the browser (software rendering, no GPU) -------------------------
IRONSURF_SMOKE=1 IRONSURF_DATA_DIR="$DATA/profile" \
  target/debug/ironsurf --software --log debug \
    >"$DATA/out.log" 2>"$DATA/shell.log" &
SHELL_PID=$!
trap 'kill $SHELL_PID 2>/dev/null; kill $FIXTURE 2>/dev/null' EXIT

# ---- drive it with synthetic input -------------------------------------------
# (the WM-less X server places the window at an offset, so every pointer
#  action below is window-RELATIVE via `--window $WID`)
WID=$(xdotool search --name IronSurf 2>/dev/null | tail -1)
for i in $(seq 40); do [ -n "$WID" ] && break; sleep 0.25; WID=$(xdotool search --name IronSurf 2>/dev/null | tail -1); done
[ -n "$WID" ] || { echo "no IronSurf window (see $DATA/shell.log)" >&2; exit 1; }
xdotool windowfocus "$WID"; sleep 1

# focus the URL bar (top toolbar, y≈19), clear it, type the fixture URL, Enter
xdotool mousemove --window "$WID" 420 19 click 1; sleep 0.2
xdotool key   --window "$WID" ctrl+a;  sleep 0.2
xdotool key   --window "$WID" Delete;  sleep 0.2
xdotool type  --window "$WID" --clearmodifiers --delay 40 "http://127.0.0.1:$PORT/index.html"
xdotool key   --window "$WID" Return
sleep 6

# hand focus to the page, then open DevTools with F12. The panel state is
# persisted per settings, so if it was already open the first F12 closes it —
# detect that from the log and press again.
xdotool mousemove --window "$WID" 900 140 click 1; sleep 0.3
xdotool key --window "$WID" F12; sleep 1
if grep -q "DevTools closed" "$DATA/shell.log"; then xdotool key --window "$WID" F12; sleep 1; fi

# ---- capture ------------------------------------------------------------------
if ! import -window "$WID" docs/screenshots/devtools.png 2>/dev/null; then
  echo "capture failed (import -window $WID)" >&2; exit 1
fi
echo "screenshot: docs/screenshots/devtools.png (Console pane)"
# second shot: the Network pane (header button ≈ x 140, y 514)
xdotool mousemove --window "$WID" 140 514 click 1; sleep 0.7
import -window "$WID" docs/screenshots/devtools-network.png 2>/dev/null \
  && echo "screenshot: docs/screenshots/devtools-network.png (Network pane)"
ls -la docs/screenshots/
