# Building IronSurf on Debian (trixie)

Verified on this box: Debian 13 (trixie) amd64, root, no GPU, `DISPLAY=:2.0`,
cargo/rustc **1.98.1** (rustup), workspace tree as of `main` (Blitz + Boa + Slint stack).
Every claim below is from `cargo tree` / the crates' own `build.rs` / `objdump -p` on the
binaries built here — not from upstream docs.

## TL;DR

```sh
# 1. Rust toolchain (AGENTS.md convention: cargo in ~/.cargo/bin)
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
export PATH="$HOME/.cargo/bin:$PATH"

# 2. Build-time system packages
apt install -y --no-install-recommends \
  build-essential pkg-config git curl ca-certificates \
  libssl-dev libfontconfig-dev libudev-dev

# 3. Runtime libraries for `cargo run -p ironsurf-shell` (X11, software rendering)
apt install -y --no-install-recommends \
  libx11-6 libx11-xcb1 libxcb1 libxcb-xkb1 libxkbcommon0 libxkbcommon-x11-0 \
  libxi6 libxrender1 libxcursor1 libfontconfig1 fonts-dejavu-core

cd /workspace && cargo build --workspace && cargo test --workspace
```

## Why each build-time package

| Package | Pulled by | Evidence |
|---|---|---|
| `build-essential` | linking every crate; `ring` 0.17 (via `rustls` ← our `reqwest 0.12`) compiles C/asm with `cc` | `cargo tree -i ring`; `cc` from Debian 14.2.0 |
| `pkg-config` | `openssl-sys` and `yeslogic-fontconfig-sys` build scripts resolve libs through it | both `build.rs` call `pkg_config::find_library(..)` |
| `libssl-dev` | **`openssl-sys` ← `native-tls` ← `reqwest 0.13` ← `blitz-net`** — the engine's network stack uses OpenSSL, even though our own `reqwest 0.12` uses rustls | `objdump -p target/debug/ironsurf-tab` → `NEEDED libssl.so.3`, `libcrypto.so.3` |
| `libfontconfig-dev` | `yeslogic-fontconfig-sys` ← `usvg` `system-fonts` ← `blitz-paint`/`anyrender_svg` and Slint's `fontique` | its `build.rs`: `pkg_config::find_library("fontconfig").unwrap()` → hard failure without `fontconfig.pc`; binary `NEEDED libfontconfig.so.1` |
| `git` | cargo git deps, and all repo work | — |
| `curl`, `ca-certificates` | rustup download; **runtime** TLS roots for the OpenSSL path (root store is read from `/etc/ssl/certs`) | `ca-certificates` 20250419 installed here |

Not required (a common over-install, inherited from the old Servo build): `cmake`, `nasm`,
`clang`/`libclang`, `llvm`, `libx11-dev`/`libxcb1-dev`/`libx11-xcb-dev`, `libwayland-dev`,
`libgl-dev`, `libglib2.0-dev`, GStreamer packages. See §Legacy.

X11/Wayland/GL libraries are **dlopen'd at runtime**, never linked at build time:
`x11-dl`, `x11rb` (`dl-libxcb`), `tiny-xlib`, `xkbcommon-dl`, `wayland-sys` (`dlopen`),
`softbuffer` (`x11-dlopen`). So dev headers for them only slow you down.

## Runtime packages (running the shell, not building it)

`ironsurf-shell` forces Slint's **software renderer** (`BackendSelector::renderer_name("software")`
in `crates/ironsurf-shell/src/main.rs`) — no GPU, no GL context. The X11 libs below are loaded
lazily (`strings` on the binary shows the exact soname list):

| Soname (dlopen'd) | Debian package |
|---|---|
| `libX11.so.6`, `libX11-xcb.so.1`, `libxcb.so.1` | `libx11-6`, `libx11-xcb1`, `libxcb1` |
| `libxkbcommon.so.0`, `libxkbcommon-x11.so.0` (+ `libxcb-xkb.so.1`) | `libxkbcommon0`, `libxkbcommon-x11-0`, `libxcb-xkb1` (added in task #9 — winit X11 dies without it) |
| `libXi.so.6`, `libXrender.so.1`, `libXcursor.so.1` | `libxi6`, `libxrender1`, `libxcursor1` |
| `libfontconfig.so.1` → `libfreetype.so.6`, `libexpat.so.1`, `libpng16.so.16`, `libz.so.1`, `libuuid.so.1` | `libfontconfig1` (pulls the rest) |
| fonts for the page renderer (`fontdb`/`usvg`) and the UI | `fonts-dejavu-core` (or `fonts-liberation`) |
| `libssl.so.3`, `libcrypto.so.3` | `libssl3t64` (comes in automatically with `libssl-dev`) |

Optional:

```sh
# Wayland session instead of X11 (wayland-sys dlopens these)
apt install -y --no-install-recommends libwayland-client0
# SLINT_BACKEND=femtovg (GPU/GL path) on a headless box needs a software GL:
apt install -y --no-install-recommends libegl1 libgl1 libgl1-mesa-dri
```

`libatspi`/GTK are **not** needed: Slint accessibility here is `accesskit_unix` (pure Rust over
D-Bus), and the X11 clipboard is `x11-clipboard` (pure Rust over XCB).

## Optional: display + test tooling

```sh
apt install -y --no-install-recommends xvfb x11-apps x11-utils xdotool imagemagick
```

`xvfb` only if there is no X server (this box already has `:2.0`). `xdotool` drives the live
human-click tests (`docs/ENGINE_INTEGRATION.md` §GUI), `x11-utils` gives `xwininfo` for the window
id, `imagemagick` (`import -window …`) and `x11-apps` (`xwd`) take screenshots.

The test suite itself needs nothing beyond localhost + `example.com`:
`cargo test --workspace` is fully headless; `tests/real_tab.rs` live-network cases are gated on
`IRONSURF_ONLINE=1` and the Slint pump tests use `i-slint-backend-testing` (no display).

## Legacy packages that are no longer dependencies

Removed with the Servo/mozjs pivot (task #6/#10; see `docs/ENGINE_INTEGRATION.md` §System build
deps): `libglib2.0-dev`, `libgstreamer-plugins-base1.0-dev` + friends, `libgbm-dev`/Mesa dev
packages, `llvm` (was only needed for `llvm-objdump` in `mozjs-sys`), `clang`/`libclang`,
`libfreetype-dev` + `freetype-sys`, `libx11-dev`/`libxcb1-dev`. If you are pruning an old box,
`dpkg -l` will still show several of them — none are used by the current graph
(`cargo tree -e normal | grep -- '-sys '` → only `openssl-sys`, `wayland-sys`,
`yeslogic-fontconfig-sys`, `linux-raw-sys`).

Related stale junk: `target/debug/build/freetype-sys-*` and `tikv-jemalloc-sys-*` directories are
leftovers from the Servo era. `target/` here is **62 GB**; expect 25 GB+ for a fresh debug build,
and ~20–35 min for the cold workspace build (`boa_engine` + `slint` dominate), 6 cores / 24 GB RAM.

## Sanity checks

```sh
pkg-config --modversion openssl fontconfig   # → 3.5.x / 2.15.x
cc --version && git --version
cargo build --workspace && cargo test --workspace   # 121+ tests green headless
```

Failure modes worth recognising:

* `Could not find directory of OpenSSL installation` / `find_library("openssl")` → `libssl-dev`
  (or, to build OpenSSL from source instead: `apt install -y perl make` + `openssl/vendored` —
  slower, not the default here).
* panic in `yeslogic-fontconfig-sys/build.rs` → `libfontconfig-dev`; to link dynamically without
  headers: `RUST_FONTCONFIG_DLOPEN=1 cargo build`.
* `X11 error: Failed to open display` with everything installed → missing
  `libxkbcommon-x11-0`, or run under `xvfb-run`.
