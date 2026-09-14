# IronSurf — agent environment

- **Contracts**: implement exactly per `/workspace/ARCHITECTURE.md` (single source of truth). Extend, never contradict.
- **Rust**: `export PATH="$HOME/.cargo/bin:$PATH"` (cargo 1.98 stable). Workspace at `/workspace`, crates under `crates/`.
- **System deps**: Debian trixie, root — full list + rationale in `docs/BUILD_DEBIAN.md`. Build-time: `apt install -y build-essential pkg-config git curl ca-certificates libssl-dev libfontconfig-dev` (OpenSSL via `blitz-net`'s reqwest, fontconfig via `usvg`/`fontique`). `apt` log at `/tmp/apt.log` — wait for `APT_EXIT=0` before building `ironsurf-tab`. Servo-era deps (glib/gstreamer/mesa/llvm/libx11-dev) are no longer needed.
- **Display**: X session at `DISPLAY=:2.0` (Xvfb-ish env; no GPU — prefer software rendering, never require GL). GUI screenshots: `xwd`/`import`.
- **Handoff protocol**: read `TASKLOG.md` first; append a ≤5-line entry last; keep TASKLOG ≤100 lines by removing past completed tasks (remove, not compress). Commit your work (git) at task end.
- Keep sessions shallow: touch only your task's files; `cargo test -p <crate>` (headless — never assume a display or network beyond localhost + example.com).
- Do not git push; you do not have access to git remotes
- Do not care about code fmt or linting, as long as it compiles
