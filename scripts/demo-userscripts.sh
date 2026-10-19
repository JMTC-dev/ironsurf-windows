#!/usr/bin/env bash
# Screenshot the userscript menu → docs/screenshots/userscripts.png
#
# Seeds a profile with three userscripts (enabled, enabled, disabled), starts
# a localhost fixture whose title a script bumps live, launches the real
# shell + tab on $DISPLAY with software rendering, opens the "</>" menu with
# synthetic xdotool input, and captures the window with ImageMagick `import`.
#
# Needs: $DISPLAY (Xvfb-ish box, default :2.0), xdotool, import, python3, and
# a built workspace (`cargo build --workspace`). No GPU; no network beyond
# 127.0.0.1. Mirrors scripts/demo.sh (same fixture/window conventions).
set -u
cd "$(dirname "$0")/.."
export PATH="$HOME/.cargo/bin:$PATH"
export DISPLAY="${DISPLAY:-:2.0}"

DATA="$(mktemp -d /tmp/ironsurf-usdemo.XXXX)"
mkdir -p docs/screenshots "$DATA/profile"

# ---- seeded profile: three scripts, the storage file format (bare array) ----
cat > "$DATA/profile/userscripts.json" <<'JSON'
[
  {"id":1,"name":"PageTweaks — bump title","patterns":["*127.0.0.1"],"code":"document.title = 'Bumped by PageTweaks ✔';","enabled":true},
  {"id":2,"name":"Console notifier","patterns":["*"], "code":"console.log('hello from Console notifier');","enabled":true},
  {"id":3,"name":"Old ad-killer","patterns":["*://ads.example.com/*"],"code":"// retired\n","enabled":false}
]
JSON

# ---- localhost fixture -------------------------------------------------------
python3 - "$DATA" <<'PY' &
import http.server, socketserver, sys
PAGE = """<!doctype html>
<html><head><title>Userscripts demo</title></head>
<body style="margin:0;background:#20242b;color:#d8ded9;font-family:sans-serif">
<h1 style="padding:24px 32px 8px">IronSurf — userscripts demo</h1>
<p style="padding:0 32px">Open the <b>&lt;/&gt;</b> menu (top bar). The title of this page was rewritten by <i>PageTweaks — bump title</i> before the menu ever opened.</p>
</body></html>"""
class H(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        body = PAGE.encode()
        self.send_response(200)
        self.send_header("Content-Type", "text/html")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)
    def log_message(self, *a): pass
srv = socketserver.TCPServer(("127.0.0.1", 0), H)
open(sys.argv[1] + "/port", "w").write(str(srv.server_address[1]))
srv.serve_forever()
PY
FIXTURE=$!
trap 'kill $SHELL_PID 2>/dev/null; kill $FIXTURE 2>/dev/null' EXIT
for i in $(seq 50); do [ -s "$DATA/port" ] && break; sleep 0.1; done
PORT=$(cat "$DATA/port" 2>/dev/null)
[ -n "$PORT" ] || { echo "fixture server failed" >&2; exit 1; }
echo "fixture: http://127.0.0.1:$PORT/"

# ---- launch the browser (software rendering, no GPU) --------------------------
IRONSURF_SMOKE=1 IRONSURF_DATA_DIR="$DATA/profile" \
  target/debug/ironsurf --software --log debug \
    >"$DATA/out.log" 2>"$DATA/shell.log" &
SHELL_PID=$!

# ---- drive it with synthetic input --------------------------------------------
WID=$(xdotool search --name IronSurf 2>/dev/null | tail -1)
for i in $(seq 40); do [ -n "$WID" ] && break; sleep 0.25; WID=$(xdotool search --name IronSurf 2>/dev/null | tail -1); done
[ -n "$WID" ] || { echo "no IronSurf window (see $DATA/shell.log)" >&2; exit 1; }
xdotool windowfocus "$WID"; sleep 1

# navigate to the fixture so the userscript's effect is visible behind the menu
xdotool mousemove --window "$WID" 420 19 click 1; sleep 0.2
xdotool key   --window "$WID" ctrl+a;  sleep 0.2
xdotool key   --window "$WID" Delete;  sleep 0.2
xdotool type  --window "$WID" --clearmodifiers --delay 40 "http://127.0.0.1:$PORT/"
xdotool key   --window "$WID" Return
sleep 6
grep -q "running 2 userscript" "$DATA/shell.log" || echo "note: no 'running 2 userscript(s)' line (see $DATA/shell.log)"

# open the "</>" menu at y≈19: it sits between ＋ and the container pill, so
# its centre is ≈ width-119; the shell logs every open/close, and the probe
# list (each click toggles, so a miss just retargets) covers pill widths.
WW=$(xdotool getwindowgeometry --shell "$WID" | sed -n 's/^WIDTH=//p')
for DX in 119 90 150 60; do
  xdotool mousemove --window "$WID" $((WW - DX)) 19 click 1; sleep 0.7
  grep -q "userscript menu opened" "$DATA/shell.log" && break
done
grep -q "userscript menu opened" "$DATA/shell.log" || { echo "could not open the </> menu (see $DATA/shell.log)" >&2; exit 1; }
sleep 0.7

if ! import -window "$WID" docs/screenshots/userscripts.png 2>/dev/null; then
  echo "capture failed (import -window $WID)" >&2; exit 1
fi
echo "screenshot: docs/screenshots/userscripts.png"
ls -la docs/screenshots/userscripts.png
