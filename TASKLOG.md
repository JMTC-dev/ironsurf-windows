# Task log
## #0–#5 prehistory — IPC 11✓; storage (registry/containers/RFC6265 jar/LRU/history) 28✓; sandbox
(prctl/setrlimits/Landlock BestEffort, sbox-probe LIVE HERE) 7✓; Servo spike SUPERSEDED by #6.
## #6 pivot Servo→Blitz+vello_cpu — blitz@0.3.0-beta.2 + anyrender 0.13 + anyrender_vello_cpu 0.17;
spike.rs headless parse→render_to_buffer proof; e2e #[ignore].
## #7a docs/BLITZ_API_NOTES.md — vendor-verified cheat-sheet: NetHandler=1 fn; nav via NavigationProvider
hook; set_viewport not resize(); has_changes() INVERTED; Doc not Send/Sync→one thread; vello=premul RGBA8.
## #7b netmod — BlitzNetProvider (jar/cache behind Mutex; reqwest-0.12-rustls workers; error⇒drop
handler) + fetch_document: https-upgrade+block page, cookies, ETag. 16 green.
## #7c Blitz IPC runtime core — TabArgs CLI (+--offline-fixtures fallback), sandbox ON (degrades never
aborts), 1-thread poll[stdin,self-pipe], nav/back/fwd/reload/resize(set_viewport)/SetVisible, HtmlDocument
per nav→settle→vello ≤30fps, Title/Url/History dedupe, Shell+Nav hooks, jars saved on Destroy/EOF.
## #7d inputs + link-click navigation — handle_input LIVE: Pointer→UiEvent (CSS-px page coords,
held buttons), Key→KeyDown(text rides input)+KeyUp, Scroll→Wheel, ≤30fps repaint; link nav via the wired
NavigationProvider hook (e2e click <a>→page2+title+Back) + input-survival test.
## #8 Boa JS layer (ironsurf-tab) — NEW src/js.rs: <script>s in document order (src= via fetch_document ⇒ same
net policy; module/JSON/unfetchable skipped w/ notes); ONE boa-0.22 Context per navigation; 5 natives + trusted
JS glue (global IS window): title get/set→TitleChanged+repaint, body textContent snapshot, console→StatusText,
every eval caught, timers counted-only (TODO), javascript: refused at navigate().
## #9 DONE — workspace green on Blitz stack; shell tabs are REAL — fixed slint 1.17 drift (Color channels u8,
Image::size()), pump test→tests/pump.rs (no CARGO_BIN_EXE in lib tests); tab history Fresh index fixed (Back
was dead; e2e wait_for vacuously re-matched→added wait_for_new); NEW tests/real_tab.rs (+IRONSURF_ONLINE=1→
example.com live); apt libxkbcommon-x11-0 for winit X11; docs SERVO→ENGINE_INTEGRATION.md (Blitz/Boa §A–D).
## #10 rename servfox→ironsurf — crates/dirs, crate+bin names, slint ui→ironsurf.slint, env IRONSURF_*,
all src/tests/docs/TASKLOG refs; Cargo.lock rewritten. cargo build+test --workspace: 130 passed, 0 failed.
## #11 link clicks feel instant + honest status bar — tab: coalesce_moves drops superseded hover moves per
stdin batch + no repaint when get_hover_node_id() unchanged (60-move flood→click ack 19.98s→0.93s, frames 64→2);
hover <a href> ⇒ StatusText(target) / None on leave. shell: move_due 33 ms move gate (newest flushed per tick),
LoadingChanged(true) clears sticky Error (old errors shadowed every new status = "appends the old logs"), spinner
refreshes at load start + "Loading …". live human-click 11.5s→1.5s (3 trials); 121 tests green; docs §E.
## #12 docs/BUILD_DEBIAN.md — apt prerequisites proven from cargo tree/build.rs/objdump: build-time =
build-essential, pkg-config, git/curl/ca-certs, libssl-dev (openssl-sys←native-tls←reqwest0.13←blitz-net),
libfontconfig-dev (yeslogic-fontconfig-sys←usvg/fontique); X11/Wayland/GL are dlopen'd so NO -dev headers.
Runtime = libx11-6/-xcb1/libxcb1/libxcb-xkb1/libxkbcommon{,-x11}-0/libxi6/libxrender1/libxcursor1/
libfontconfig1/fonts-dejavu-core; Servo-era deps (glib/gstreamer/mesa/llvm/libx11-dev) dropped in README+AGENTS.
