# IronSurf - A Browser Built in Rust

This is a browser I am building using AI Agents. Almost none of the code is hand-written (altho this README.md does contain human writtten content).

Components: Boa for JS, Stylo for CSS, Blitz for HTML DOM. Slint for the UI.

Tab support with native firefox-style multi-account containers is the goal.

## Building (Debian)

```sh
apt install -y --no-install-recommends build-essential pkg-config git curl ca-certificates \
  libssl-dev libfontconfig-dev            # build-time
apt install -y --no-install-recommends libxkbcommon0 libxkbcommon-x11-0 libx11-xcb1 \
  libxcb1 libxcb-xkb1 libxi6 libxrender1 libxcursor1 libfontconfig1 fonts-dejavu-core  # run-time
cargo build --workspace && cargo test --workspace
```

Full prerequisite list, why each package is needed, and troubleshooting: `docs/BUILD_DEBIAN.md`.

Feel free to help out, fork, spend some tokens on it. The initial Proof of Concept build (this repo) is built entirely from locally-hosted Qwen 3.8 Flash Next.

