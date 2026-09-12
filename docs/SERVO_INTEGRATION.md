# Servo 0.5 integration notes (servfox-tab spike, task 4)

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

`servfox-tab --spike --url file:///… --out out.ppm --size 800x600`

* loads, fires `notify_url_changed` / `notify_page_title_changed` (title captured),
  reaches `LoadStatus::Complete`, captures after render-up-to-date
  (~0.2 s warm), writes binary PPM; bounded by a 60 s deadline that dumps delegate
  state on timeout.
* `cargo test -p servfox-tab --test spike` (headless, `DISPLAY` removed from the
  child env): fixture = white page + solid `#ff3200` box + blue `<h1>`; asserts PPM
  magic/dimensions and non-uniform pixels. Captured box pixel is exactly `(255,50,0)`.

## Not yet done (follows in the IPC task)

`present()` (not needed when reading back directly), `ToTab`/`FromTab` pipe loop,
sandbox application, `--profile-dir` → Servo data path mapping, `netmod.rs`
(`load_web_resource` interception), input event plumbing.
