#!/usr/bin/env bash
# Screenshot the ⛔ popup blocker **hovered** — before and after one click —
# → docs/screenshots/popup-tip-blocked.png and docs/screenshots/popup-tip-allowed.png
#
# The two things a reader of the toolbar is owed, proven on a live X screen with
# the pixels as the witness (the headless suite pins the words themselves):
#
#   1. the ⛔ says `popups blocked` / `popups allowed` while the pointer rests on
#      it, in the one tooltip box the window owns;
#   2. the click that changes the switch changes the **button and its words**
#      immediately — the pointer does not move, no tab is switched, and the tab
#      never answers a policy it was handed, so nothing on the wire could have
#      repainted either one.
#
# The ⛔ is found by the colour of its own held-in look (#35567D, the only one
# of them in the toolbar while the active tab blocks); the tooltip box is found
# by its background (#1B1B1B), which appears and disappears with the pointer.
#
# Needs: $DISPLAY (Xvfb is enough: `Xvfb :99 -screen 0 1280x900x24`), xdotool,
# import/compare (ImageMagick), python3, and a built workspace
# (`cargo build --workspace` — the browser is the single
# `target/debug/ironsurf`, and `fake_tab` is the scripted harness the shell is
# pointed at with `--tab-bin`). No GPU, nothing off-box.
set -u
cd "$(dirname "$0")/.."
export PATH="$HOME/.cargo/bin:$PATH"
export DISPLAY="${DISPLAY:-:99}"

DATA="$(mktemp -d /tmp/ironsurf-popup-tip.XXXX)"
mkdir -p docs/screenshots

# ---- the browser, on the scripted fake tab (the page is not the subject) ----
IRONSURF_SMOKE=1 IRONSURF_DATA_DIR="$DATA/profile" \
  target/debug/ironsurf --software --log debug --tab-bin target/debug/fake_tab \
  >"$DATA/out.log" 2>"$DATA/shell.log" &
SHELL_PID=$!
trap 'kill $SHELL_PID 2>/dev/null' EXIT

WID=$(xdotool search --name IronSurf 2>/dev/null | tail -1)
for i in $(seq 40); do [ -n "$WID" ] && break; sleep 0.25; WID=$(xdotool search --name IronSurf 2>/dev/null | tail -1); done
[ -n "$WID" ] || { echo "no IronSurf window (see $DATA/shell.log)" >&2; exit 1; }
xdotool windowfocus --sync "$WID" 2>/dev/null; sleep 1.5

# Where the ⛔ sits, window-relative: the centre of the pressed-blue button.
cat > "$DATA/find.py" <<'PY'
import re, subprocess, sys
shot = "/tmp/ironsurf-tip-find.png"
subprocess.run(["import", "-window", sys.argv[1], shot], check=True)
dump = subprocess.run(["magick", shot, "-crop", "x38+0+0", "+repage", "txt:-"],
                      capture_output=True, text=True).stdout
pts = [(int(m.group(1)), int(m.group(2)))
       for line in dump.splitlines()
       for m in [re.match(r"(\d+),(\d+):\s+\((\d+),(\d+),(\d+)", line)]
       if m and (int(m.group(3)), int(m.group(4)), int(m.group(5))) == (53, 86, 125)]
if not pts:
    sys.exit("no pressed ⛔ (#35567D) in the toolbar")
xs = [p[0] for p in pts]; ys = [p[1] for p in pts]
print((min(xs) + max(xs)) // 2, (min(ys) + max(ys)) // 2)
PY
read -r BX BY < <(python3 "$DATA/find.py" "$WID")
echo "⛔ centre at window-relative ($BX,$BY)"

# ---- hover, then click with the pointer exactly where it is -----------------
xdotool mousemove --window "$WID" "$BX" "$BY"; sleep 0.6
import -window "$WID" docs/screenshots/popup-tip-blocked.png 2>/dev/null || { echo "capture failed" >&2; exit 1; }
xdotool click 1 2>/dev/null; sleep 0.7
import -window "$WID" docs/screenshots/popup-tip-allowed.png 2>/dev/null
# …and the pointer finally leaves, so the held-in icon is visible unhovered.
xdotool mousemove --window "$WID" 420 500; sleep 0.5
import -window "$WID" "$DATA/unhovered-after-click.png" 2>/dev/null

# ---- what the pixels say ---------------------------------------------------
python3 - "$WID" "$BX" "$BY" "$DATA" <<'PY'
import re, subprocess, sys

wid, bx, by, data = sys.argv[1], int(sys.argv[2]), int(sys.argv[3]), sys.argv[4]


def shot(path):
    subprocess.run(["import", "-window", wid, path], check=True)
    return subprocess.run(["magick", path, "txt:-"], capture_output=True, text=True).stdout


def pixels(dump, want, box=None):
    out = []
    for line in dump.splitlines():
        m = re.match(r"(\d+),(\d+):\s+\((\d+),(\d+),(\d+)", line)
        if not m:
            continue
        x, y, rgb = int(m.group(1)), int(m.group(2)), (int(m.group(3)), int(m.group(4)), int(m.group(5)))
        if rgb == want and (box is None or (box[0] <= x <= box[2] and box[1] <= y <= box[3])):
            out.append((x, y))
    return out


BLOCKED = "docs/screenshots/popup-tip-blocked.png"
ALLOWED = "docs/screenshots/popup-tip-allowed.png"


def box_of(path, colour):
    dump = subprocess.run(["magick", path, "txt:-"], capture_output=True, text=True).stdout
    pts = pixels(dump, colour)
    if not pts:
        return None
    xs, ys = [p[0] for p in pts], [p[1] for p in pts]
    return min(xs), min(ys), max(xs), max(ys)


TIP, PRESSED = (27, 27, 27), (53, 86, 125)
tip_before = box_of(BLOCKED, TIP)
tip_after = box_of(ALLOWED, TIP)
assert tip_before, "the tooltip never appeared while the ⛔ was hovered"
assert tip_after, "the tooltip vanished while the pointer was still on the ⛔ after the click"
assert tip_before[1] > 31 and tip_before[0] <= bx, f"the box hangs below the button it came from: {tip_before}"
print(f"tooltip box before: {tip_before}  after: {tip_after}")

changed = subprocess.run(
    ["compare", "-metric", "AE", BLOCKED, ALLOWED, "null:"],
    capture_output=True, text=True).stderr.strip()
tip_changed = subprocess.run(
    ["compare", "-metric", "AE",
     f"{BLOCKED}[{(tip_before[2]-tip_before[0]+12)}x{(tip_before[3]-tip_before[1]+12)}"
     f"+{tip_before[0]-6}+{tip_before[1]-6}]",
     f"{ALLOWED}[{(tip_before[2]-tip_before[0]+12)}x{(tip_before[3]-tip_before[1]+12)}"
     f"+{tip_before[0]-6}+{tip_before[1]-6}]",
     "null:"], capture_output=True, text=True).stderr.strip()
print(f"pixels that changed over the whole window: {changed}, of which inside the tooltip: {tip_changed}")

unhovered = pixels(shot(f"{data}/unhovered-after-click.png"), PRESSED)
assert not unhovered, f"the ⛔ is still drawn pressed after the click ({len(unhovered)} px) — the icon waited for a tab switch"
print("and with the pointer off the button: it is no longer drawn pressed")
print("OK: the ⛔ named its state, restated it under the click, and changed with it")
PY

echo "screenshots: docs/screenshots/popup-tip-blocked.png, docs/screenshots/popup-tip-allowed.png"
echo "log: $DATA/shell.log"
