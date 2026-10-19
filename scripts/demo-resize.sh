#!/usr/bin/env bash
# Screenshot the DevTools panel **before and after dragging its top edge**
# → docs/screenshots/devtools-panel.png and docs/screenshots/devtools-resized.png
#
# Same fixture + launch as scripts/demo.sh, then: F12, locate the panel's own
# top edge in the captured pixels (the 1 px #4a4a4a separator), grab the strip
# that straddles it with synthetic xdotool input, pull it up ~120 px, and shoot
# again. The shell's own log records the height it agreed to
# ("DevTools panel of tab N resized to …"), which is printed here.
#
# Needs: $DISPLAY, xdotool, import, python3, and a built workspace
# (`cargo build -p ironsurf-shell -p ironsurf-tab`). No GPU; localhost only.
set -u
cd "$(dirname "$0")/.."
export PATH="$HOME/.cargo/bin:$PATH"
export DISPLAY="${DISPLAY:-:2.0}"

DATA="$(mktemp -d /tmp/ironsurf-resize.XXXX)"
mkdir -p docs/screenshots

# ---- localhost fixture ------------------------------------------------------
python3 - "$DATA" <<'PY' &
import http.server, socketserver, sys
PAGE = """<!doctype html>
<html><head><title>DevTools resize demo</title></head>
<body style="margin:0;background:#20242b;color:#d8ded9;font-family:sans-serif">
<h1 style="padding:24px 32px 8px">IronSurf — drag the panel's top edge</h1>
<p style="padding:0 32px">F12 opens the panel; its top edge is grabbed 4 px above this separator.</p>
<script>console.log("panel height before the drag");</script>
</body></html>"""
class H(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        body, ctype, code = PAGE.encode(), "text/html", 200
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

# ---- launch the browser (software rendering, no GPU) ------------------------
IRONSURF_SMOKE=1 IRONSURF_DATA_DIR="$DATA/profile" \
  target/debug/ironsurf --software --log debug \
    >"$DATA/out.log" 2>"$DATA/shell.log" &
SHELL_PID=$!
trap 'kill $SHELL_PID 2>/dev/null; kill $FIXTURE 2>/dev/null' EXIT

# ---- drive it ---------------------------------------------------------------
WID=$(xdotool search --name IronSurf 2>/dev/null | tail -1)
for i in $(seq 40); do [ -n "$WID" ] && break; sleep 0.25; WID=$(xdotool search --name IronSurf 2>/dev/null | tail -1); done
[ -n "$WID" ] || { echo "no IronSurf window (see $DATA/shell.log)" >&2; exit 1; }
xdotool windowfocus "$WID"; sleep 1

xdotool mousemove --window "$WID" 420 19 click 1; sleep 0.2
xdotool key   --window "$WID" ctrl+a;  sleep 0.2
xdotool key   --window "$WID" Delete;  sleep 0.2
xdotool type  --window "$WID" --clearmodifiers --delay 40 "http://127.0.0.1:$PORT/"
xdotool key   --window "$WID" Return
sleep 5

xdotool mousemove --window "$WID" 900 140 click 1; sleep 0.3
xdotool key --window "$WID" F12; sleep 1
if grep -q "DevTools closed" "$DATA/shell.log"; then xdotool key --window "$WID" F12; sleep 1; fi

import -window "$WID" docs/screenshots/devtools-panel.png 2>/dev/null || {
  echo "capture failed" >&2; exit 1; }

# ---- find the panel's top edge (the 1 px separator) in the shot -------------
EDGE=$(python3 - <<'PY'
import re, subprocess
# Locate the panel's own top edge: the row painted almost entirely in the
# separator grey #4a4a4a, nearest the bottom of the shot.
dump = subprocess.run(["magick", "docs/screenshots/devtools-panel.png", "txt:-"],
                      capture_output=True, text=True).stdout
rows = {}
for line in dump.splitlines()[1:]:
    m = re.match(r"(\d+),(\d+):\s+\S+\s+(#[0-9A-Fa-f]{6})", line)
    if m:
        rows.setdefault(int(m.group(2)), []).append(m.group(3).upper())
candidates = [
    (y, sum(1 for c in cols if c == "#4A4A4A"))
    for y, cols in rows.items()
    if y >= 300 and sum(1 for c in cols if c == "#4A4A4A") >= 0.9 * len(cols)
]
print(max(candidates, key=lambda c: c[1])[0] if candidates else "")
PY
)
[ -n "$EDGE" ] || { echo "panel separator not found in the shot" >&2; exit 1; }
echo "panel top edge at window-relative y=$EDGE"

# ---- drag the strip up 120 px ----------------------------------------------
# The grab strip is 8 px tall and centred on that edge, so y=EDGE is dead centre.
xdotool mousemove --window "$WID" 600 "$EDGE"; sleep 0.3
xdotool mousedown 1; sleep 0.2
for i in $(seq 12); do
  xdotool mousemove --window "$WID" 600 $((EDGE - i * 10)); sleep 0.08
done
xdotool mouseup 1; sleep 0.6
import -window "$WID" docs/screenshots/devtools-resized.png 2>/dev/null

grep "resized to" "$DATA/shell.log" | tail -2
echo "screenshots: docs/screenshots/devtools-panel.png (before), docs/screenshots/devtools-resized.png (after)"
echo "log: $DATA/shell.log"
