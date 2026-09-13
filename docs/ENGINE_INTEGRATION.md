# Engine integration notes (ironsurf-tab)

The engine is **Blitz + vello_cpu + Boa** (pivot in task 6). Sections A–D below are
the live contract discovered implementing tasks 6–9. The legacy Servo 0.5 spike
notes are kept at the bottom as an appendix: their **OS/process-level** gotchas
still apply to the tab process today; their API specifics are obsolete.

## A. Paint path (Blitz 0.3.0-beta.2 + anyrender 0.13 + anyrender_vello_cpu 0.17)

* One `HtmlDocument::from_html(html, DocumentConfig)` **per navigation**, rebuilt on
  every nav/reload; `Document` is **neither `Send` nor `Sync`** → the whole engine
  lives on one dedicated thread; the IPC loop talks to it via a self-pipe wake.
* Layout/paint cycle: `document.resolve(scale)` after every input/net/JS tick, then
  `blitz_paint::paint_scene` → `VelloCpuImageRenderer::render_to_buffer` → RGBA8.
  **vello_cpu emits premultiplied RGBA**; `FromTab::Frame` is straight alpha → the
  tab un-premultiplies before shipping (and Slint's `Rgba8Pixel` is straight alpha).
* `Document::has_changes()` semantics are **inverted** from what the name suggests
  (see `BLITZ_API_NOTES.md`); the tab tracks its own `needs_paint` and throttles to
  ≤30 fps, but force-bypasses the throttle right after load/resize (`last_paint = None`).
* Resize: `set_viewport(Viewport)` is **the** resize entry point (`resize()` does not
  reflow). Until the shell sends `ToTab::Resize` the tab paints at `DEFAULT_SIZE`
  (1024×768) — a fresh tab produces its first frame with **no** Resize, which is what
  lets the shell composite before the window lays out.
* Headless by construction: no winit, no wgpu, no display server for the tab; only
  the shell needs X (Slint software renderer via softbuffer — force
  `renderer_name("software")` before the first window; runtime dep
  `libxkbcommon-x11-0` for winit's X11 keyboard backend).

## B. NetProvider (`src/netmod.rs`)

* `NetHandler` is **one function**; `BlitzNetProvider` implements the
  `blitz_traits::net::NetProvider` trait and delivers `(status, headers, body)`
  to that handler. Subtle failure mode: on the error path the handler must be
  **dropped** (not called with a bogus response) or Blitz stalls the request queue.
* Fetching: `reqwest 0.12` (rustls) on worker threads; per-container cookie jar +
  LRU cache live behind a `Mutex` inside the provider (Blitz may call from any thread).
* Policy (per §Network layer): navigations are upgraded `http→https`, an upgrade
  failure renders a block page naming both URLs; `Secure` cookies are sent on https
  only; ETag/`If-None-Match` revalidation serves the cached body only on a matching
  validator. `--managed-net` toggles Managed vs Passthrough (default: Passthrough).
* `fetch_document` (shared by navigations and `<script src>`) is the single network
  entry point, so scripts obey exactly the same cookie/cache/upgrade policy.

## C. Boa JS surface (boa 0.22, `src/js.rs`)

* **One `boa::Context` per navigation** (fresh realm = correct location/title state).
  `<script>` blocks run in document order; `src=` scripts go through
  `fetch_document` (same net policy); `type=module`/JSON/unfetchable are skipped with notes.
* The binding surface is 5 natives + a trusted JS glue script; the glue makes the
  global object *be* `window` (self-referential), provides `document.title` get/set
  (set ⇒ `FromTab::TitleChanged` + repaint), `document.body.textContent` snapshot,
  and `console.*` ⇒ tab stderr + `FromTab::StatusText`.
* Every `eval` is caught: a throwing script is reported (`ConsoleLog`/`Error`) and
  the **next** script still runs (asserted in `tests/e2e.rs`).
* Timers (`setTimeout`/`setInterval`) are counted-only today (scheduled = TODO);
  `javascript:` URLs are refused at `navigate()` time.

## D. History & shell-side lessons (task 9)

* `NavHistory::Fresh` in `runtime.rs` once computed the target index **before**
  pushing the new URL, so `history_index` never advanced → Back was dead after any
  shell-driven navigation. The tab's own e2e missed it because its `wait_for`
  matches *any message ever seen* (a Back assertion re-matched the page-1
  `UrlChanged` from spawn). Now: Fresh ⇒ `position(url)` or push-and-point-at-HEAD,
  plus a `wait_for_new` helper that only accepts fresh messages.
* **Cargo gotcha:** `CARGO_BIN_EXE_<name>` is *not* defined for the lib's unit-test
  target — any test that spawns a sibling bin must live under `tests/` (the shell's
  fake-tab pump test moved to `tests/pump.rs`). Integration tests can locate
  workspace sibling bins via `current_exe()/../../ironsurf-tab`.
* **Slint 1.17 drift:** `Color` channels are `u8` (`color.green() > 150`, not `> 0.6`);
  `Image` exposes `size() -> IntSize`, not `width()/height()` methods.
* End-to-end proof lives in `crates/ironsurf-shell/tests/real_tab.rs`: the real Slint
  window + real `ironsurf-tab` binary + Blitz frames, headless on file fixtures in the
  container profile dir; the live `https://example.com` leg runs with `IRONSURF_ONLINE=1`.

---

# Appendix — legacy Servo 0.5 spike notes (superseded by the Blitz pivot)

Everything below was discovered against the **published `servo = "0.5.0"` crate**
(`~/.cargo/registry/src/*/servo-0.5.0/`, unpacked), headless on this box (no GPU,
no X for the spike — `unset DISPLAY`). API-level notes are historical; the **still
valid OS/process gotchas** are: non-blocking stdin for the IPC loop, `libc::_exit`
(not `process::exit`) to dodge atexit/teardown races, Landlock's extra read-only
paths (`/etc/ld.so.cache`, `/etc/fonts`, `/usr/share/fontconfig`,
`/usr/share/ca-certificates`, …), fixtures must live *inside* the profile dir
because the sandbox blocks other reads, and benign `XDG_RUNTIME_DIR` noise.

Everything below was discovered against the **published `servo = "0.5.0"` crate**
(`~/.cargo/registry/src/*/servo-0.5.0/`, unpacked), headless on this box (no GPU,
no X for the spike — `unset DISPLAY`).

## Chosen rendering context

**`servo::SoftwareRenderingContext`** (re-export from `paint_api::rendering_context`
in `servo-paint-api-0.5.0/rendering_context.rs`). Built on **surfman 0.13's software
adapter** (pure-software GL — smitha), created with
`SoftwareRenderingContext::new(dpi::PhysicalSize<u32>)` + `make_current()`.
Full WebRender pipeline runs on top of it with zero display-server dependency:
the fixture page rendered to pixels with `DISPLAY` unset. `OffscreenRenderingContext`
requires a parent `WindowRenderingContext`, so it is not usable fully headless.

Read-back path: `WebView::take_screenshot(None, callback)` → internally
`rendering_context.make_current()` + `RenderingContext::read_to_image(viewport)`
(`screenshot.rs` in `servo-paint`). We get an `image::RgbaImage` (re-exported as
`servo::RgbaImage`) and write PPM ourselves.

## Feature selection (Cargo.toml)

```toml
servo = { version = "0.5", default-features = false,
          features = ["baked-in-resources", "bundled_freetype"] }
```

* **Surprise:** there is no `bundled-resources` feature — it is `baked-in-resources`
  (the crates.io feature list also has `clipboard`, `js_jit`, `gstreamer`, `vello`,
  `webgpu`, `webxr`, …; defaults are baked-in-resources + bundled_freetype + clipboard + js_jit).
  We skip clipboard/js_jit for the spike (smaller build); re-add when the tab shell needs them.
* `multiprocess` does not exist as a feature in 0.5 (`Opts.multiprocess: bool` is a
  runtime option, default false).

## System build deps

* **`mozjs-sys` build script hard-fails with `ERROR: Cannot find llvm-objdump`** —
  Debian needs `apt-get install llvm` (provides `/usr/bin/llvm-objdump`). Not in the
  original apt dep set; everything else (glib/gstreamer/mesa/fonts) was already there.
* SpiderMonkey (mozjs 140) builds from source; a warm-`target/` debug build of the
  whole servo tree took ~6 min cold-cached on this box (~45 min total counting the first attempt).
  Never bump the locked servo/mozjs version.

## API surprises / gotchas (each cost a debugging cycle)

1. **Embedder paint contract.** `WebViewDelegate::notify_new_frame_ready` **must**
   call `WebView::paint()` (docs in `lib.rs` say so, but it looks optional). Screenshot
   completion runs at the end of the composite step (`Painter::render →
   screenshot_taker.maybe_take_screenshots`). Without any `paint()` call, the page
   loads, WebRender generates frames (`notify_new_frame_ready` fires endlessly) but
   `take_screenshot`'s callback **never fires** — it stalls after
   `ScreenshotReadinessResponse` forever. Fix: `webview.paint()` inside
   `notify_new_frame_ready` (what we now do in `SpikeDelegate`).
2. **`url::Url` is not re-exported.** `WebViewDelegate::notify_url_changed` and
   `WebViewBuilder::url()` take `url::Url` (the plain crate type), while `servo`
   only re-exports `ServoUrl`. Embedders need their own `url = "2"` dep and it must
   unify with servo's in the lockfile. Same for `dpi = "0.1"`: `PhysicalSize<u32>`
   (used by `SoftwareRenderingContext::new`, `WebView::resize`) comes from `dpi`,
   is not re-exported, and winit-based examples hide this.
3. **`surfman::error::Error` does not implement `std::error::Error`.** `?` on
   `make_current()`/`new()` fails to convert into `Box<dyn Error>`; map with
   `{:?}` first.
4. **Logging is inert until `Servo::setup_logging()`** (env_logger); `RUST_LOG`
   alone prints nothing, which makes early debugging look like total silence.
5. **`EventLoopWaker`** = `servo::EventLoopWaker` (re-exported via
   `pub use embedder_traits::*`); `wake()` may be called from engine threads, so
   the impl must be `Send + Sync` (`'static`). We use a non-blocking self-pipe
   (`libc::pipe` + `O_NONBLOCK`, `Arc<OwnedFd>` write-end, `poll()` read-end on the
   main thread). The `EventLoopWaker` trait no longer has `set_main_thread` in 0.5.
6. Timer-paced progress: even with a waker, spin `Servo::spin_event_loop()` on a
   ~10 ms cadence; the screenshot state machine advances off message traffic that is
   not always accompanied by a wake (servoshell drives it via a refresh timer).
7. **No `--user-data-path` in `servo::Opts`** (0.5 struct: multiprocess, sandbox,
   config_dir, temporary_storage, certificate_path, …). Binding Servo's on-disk state
   to our container profile dir will need `Opts.config_dir` or engine prefs
   investigation in the IPC task; for the spike we set `temporary_storage = true`.
8. `Opts::default()` exists and is constructible (no builder), and is consumed
   through `ServoBuilder::opts(..)`; `opts::initialize_options` is process-global
   (OnceCell) → exactly one `Servo` per process, as the architecture assumed.
9. A harmless `error: XDG_RUNTIME_DIR is invalid or not set in the environment.`
   line is printed by some engine subsystem at startup even in the pure-software
   path; it does not affect rendering (test asserts pixels regardless).
10. Screenshot arrives **at exactly the RenderingContext size** (viewport rect),
    800x600 → 800x600; a static page produces exactly 2 frames once `paint()` is
    wired correctly (no runaway repaints).

## Spike behaviour (proof)

`ironsurf-tab --spike --url file:///… --out out.ppm --size 800x600`

* loads, fires `notify_url_changed` / `notify_page_title_changed` (title captured),
  reaches `LoadStatus::Complete`, captures after render-up-to-date
  (~0.2 s warm), writes binary PPM; bounded by a 60 s deadline that dumps delegate
  state on timeout.
* `cargo test -p ironsurf-tab --test spike` (headless, `DISPLAY` removed from the
  child env): fixture = white page + solid `#ff3200` box + blue `<h1>`; asserts PPM
  magic/dimensions and non-uniform pixels. Captured box pixel is exactly `(255,50,0)`.

## Task 5 — full IPC runtime (`--tab-id/--container-id/--profile-dir/--url`) additions

* **Container data path knob found: `Opts.config_dir = Some(profile_dir)`.** No
  `--user-data-path` exists, but `config_dir` binds the engine's whole on-disk
  state there — verified: `clientstorage/default_v1/reg.sqlite` (cookie store),
  `localstorage.json`, and `cachestorage/` are created inside the profile dir.
  Keep `temporary_storage = false`.
* **Streaming frame readback** (replaces `take_screenshot` for live frames):
  in `notify_new_frame_ready` call `WebView::paint()` (same paint contract),
  then `rendering_context.make_current()` +
  `RenderingContext::read_to_image(DeviceIntRect::from_size(ctx.size()))` →
  `RgbaImage` → `FromTab::Frame`. Works, and is synchronous + cheap.
* **Resize trap (cost hours):** `Painter::resize_rendering_context`
  **early-returns when the context already reports the new size**. Resizing the
  `RenderingContext` ourselves before `WebView::resize()` therefore silently
  swallows the viewport change — no reflow, no further frames, ever. Call only
  `webview.resize(PhysicalSize)`; it resizes the context and repaints itself.
* **Stdin must be non-blocking.** Reading a frame with blocking `read_exact`
  after `poll()` fires deadlocks the loop until the *next* shell message (the
  engine stops getting spins; resize/navigation hang). Runtime: `O_NONBLOCK`
  stdin, pump bytes into a buffer, parse only complete length-prefixed frames.
  Shell-side frames must be a single `write_frame` (they always are).
* **Exit is `libc::_exit(code)`, not `std::process::exit`:** Mesa/glvnd
  registered atexit/destructors + live engine threads = teardown SIGSEGV race
  (observed flaky `exit -11`). stdout is flushed and the jar persisted first.
* **Landlock needs extra read-only paths** beyond the spec list or the GL stack
  dies at `dlopen` (already-loaded libs are fine, lazy ones not):
  `/etc/ld.so.cache`, `/etc/glvnd`, `/usr/share/glvnd` (glvnd scans these for
  EGL vendors — without them the *sandboxed* tab can't create a context),
  `/usr/share/ca-certificates` (`/etc/ssl/certs/*` are symlinks to it — TLS
  roots), `/etc/fonts` + `/usr/share/fontconfig`. Non-fatal denials left alone:
  `/proc/self/{stat,cgroup}`, `$HOME/.cache/mesa_shader_cache*`, `/sys` cpuinfo.
* **Input maps (0.5):** `MouseMove(MouseMoveEvent)`,
  `MouseButton(MouseButtonEvent::new(Down|Up, embedder MouseButton, WebViewPoint))`,
  `MouseLeftViewport` for Out; `Keyboard(servo::KeyboardEvent::new_without_event(
  KeyState, keyboard_types Key/Code parsed via FromStr, Location::Standard,
  Modifiers::CONTROL|SHIFT|ALT|META, repeat, false))`; scroll via
  `WebView::notify_scroll_event(Scroll::Delta(WebViewVector::Page(..)), point)`.
  `notify_history_changed(entries, current)` → `can_back = current > 0`,
  `can_fwd = current + 1 < len`. `LoadStatus` is `Started/HeadParsed/Complete`.
* **Managed net (`--managed-net`):** chokepoint `load_web_resource` →
  `intercept(WebResourceResponse::new(url).status_code(..).headers(HeaderMap))`
  (needs `http = "1"` dep to match servo-embedder-traits) +
  `send_body_data(..)` + `finish()`. reqwest (rustls, blocking) fetches on the
  main thread; `Set-Cookie` captured via `http::HeaderMap::get_all` into the
  container jar; cache revalidation sends `If-None-Match`/`If-Modified-Since`.
  Policy math is pure (`plan`/`upgrade_to_https`/`revalidate`/
  `forwarded_response_headers`) and unit-tested in `src/netmod.rs` (bin crate:
  `tests/` can't import modules, so pure-rule tests live in-module; `tests/e2e.rs`
  holds the live-protocol tests).
* e2e with the **sandbox ON**: the sandbox genuinely blocks reading the page,
  so the `file://` fixture lives *inside* the profile dir (which is
  writable+readable to the tab) — that's also the shape of future
  `--offline-fixtures` layouts.
