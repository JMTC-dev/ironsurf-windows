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

## Debugging (stderr diagnostics)

Everything the browser knows goes to **stderr** — never stdout, which is the
binary IPC channel between the shell and a tab.

```sh
IRONSURF_LOG=debug ironsurf-shell --tab-stderr   # shell + every tab, interleaved
ironsurf-shell --log trace                      # + frames, pointer events, IPC noise
ironsurf-shell --log off                        # quiet
```

`off|error|warn|info|debug|trace` (default `info`); `--log`/`-v` win over
`IRONSURF_LOG`, which wins over `RUST_LOG`. Lines look like
`[14:22:07.418] INFO tab 3 runtime: url -> https://example.com/`. URL changes,
load start/finish, tab spawn/exit and page `console.*`/JS errors are all there —
`docs/ENGINE_INTEGRATION.md` §F lists every site and why the noisy ones sit at
`trace` (a tab's stderr is a pipe nobody reads).

## Developer tools

`F12` or `Ctrl-I` toggles a per-tab DevTools panel docked over the bottom of the page:

![IronSurf DevTools console](docs/screenshots/devtools.png)

- **Console** — `console.log/info/warn/error`, uncaught JS exceptions, invalid CSS, and
  native DOM binding throws, each line tagged with its source (`[js]`, `[css]`, `[dom]`)
  and coloured by level.
- **Network** — one row per request the page made: method, URL, status, size and time
  (`—` where the engine could not measure something; failed requests show their error text
  in red instead of a status). Redirect hops appear as separate rows.
  ![Network pane](docs/screenshots/devtools-network.png)

Honest limitations: Blitz hands Stylo no parse-error reporter, so the console's CSS entries
come from the tab re-parsing `<style>` blocks and inline `style` attributes itself —
parse errors *inside* `<link rel=stylesheet>` files (and `@import` bodies) are invisible
today (their load failures and wrong `Content-Type`s do show up). Font/bidi traces stay on
stderr (`--log trace`), not in the panel.

`scripts/demo.sh` reproduces both screenshots on a headless X display with a localhost fixture.

## Userscripts

The `</>` button in the top bar opens the userscript menu:

![Userscript menu](docs/screenshots/userscripts.png)

- Each entry runs its JavaScript on matching pages (URL patterns with `*` wildcards, e.g.
  `*://*.example.com/*`, `example.org/docs`), in the page's own realm — real DOM, dialogs,
  navigation — after the page's own `<script>`s, in menu order.
- The enable checkbox flips a script off everywhere at once (disabled scripts are never
  even sent to a tab); ✕ deletes it; clicking the row edits it in place (same id, same
  `enabled` flag). A throwing script only kills itself: its name and error land in the
  DevTools console tagged `[userscript]`, the tab keeps running.
- Scripts live in `~/.local/share/ironsurf/userscripts.json` (next to the container
  registry), so they survive restarts; edits reach every open tab immediately.

## Feedback

Feel free to help out, fork, spend some tokens on it. The initial Proof of Concept build (this repo) is built entirely from locally-hosted Qwen 3.8 Flash Next.

