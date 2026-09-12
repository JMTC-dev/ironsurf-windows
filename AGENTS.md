# ServFox — agent environment

- **Contracts**: implement exactly per `/workspace/ARCHITECTURE.md` (single source of truth). Extend, never contradict.
- **Rust**: `export PATH="$HOME/.cargo/bin:$PATH"` (cargo 1.98 stable). Workspace at `/workspace`, crates under `crates/`.
- **System deps**: Debian trixie, root. `apt` log at `/tmp/apt.log` — wait for `APT_EXIT=0` before building `servfox-tab`. Servo needs glib/gstreamer/mesa/fonts (installed there).
- **Display**: X session at `DISPLAY=:2.0` (Xvfb-ish env; no GPU — prefer software rendering, never require GL). GUI screenshots: `xwd`/`import`.
- **Servo** (`servo` crate from crates.io, ≥0.1): docs.rs fails; authoritative API = the unpacked registry source (`~/.cargo/registry/src/*/servo-*`) or GitHub `main` (`components/servo/{lib.rs,webview.rs,webview_delegate.rs}`, `examples/winit_minimal.rs`). First build takes 30–45 min — build once, reuse `target/`, never bump the locked version.
- **Handoff protocol**: read `TASKLOG.md` first; append a ≤5-line entry last; keep TASKLOG ≤30 lines. Commit your work (git) at task end.
- Keep sessions shallow: touch only your task's files; `cargo test -p <crate>` (headless — never assume a display or network beyond localhost + example.com).
