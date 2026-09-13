# Task log

## #0–#3 prehistory — contracts; common IPC 11✓; storage (registry/containers/RFC6265 jar/LRU/history) 28✓; sandbox (prctl/setrlimits/Landlock BestEffort, sbox-probe LIVE HERE) 7✓.
## #4+#5 Servo spike+runtime — SUPERSEDED by #6; legacy appendix in docs/ENGINE_INTEGRATION.md
## #6 pivot Servo→Blitz+vello_cpu — blitz@0.3.0-beta.2 + anyrender 0.13 + anyrender_vello_cpu 0.17;
spike.rs headless parse→resolve→render_to_buffer proof; e2e #[ignore].
## #7a docs/BLITZ_API_NOTES.md — vendor-verified cheat-sheet: NetHandler=1 fn; nav via NavigationProvider
hook; set_viewport not resize(); has_changes() INVERTED; Doc not Send/Sync→one thread; vello=premul RGBA8.
## #7b netmod — BlitzNetProvider (jar/cache behind Mutex; reqwest-0.12-rustls workers; error⇒drop
handler) + fetch_document: https-upgrade+block page, cookies, ETag. 16 green.
## #7c Blitz IPC runtime core — TabArgs CLI (+--offline-fixtures fallback), sandbox ON (degrades never
aborts), 1-thread poll[stdin,self-pipe], nav/back/fwd/reload/resize(set_viewport)/SetVisible, HtmlDocument
per nav→settle→vello ≤30fps, Title/Url/History dedupe, Shell+Nav hooks, jars saved on Destroy/EOF. NOTE:
ironsurf-shell fails PRE-EXISTING at HEAD (slint 1.x drift) — task #9 owns it.
## #7d inputs + link-click navigation — handle_input LIVE: Pointer→UiEvent (CSS-px page coords,
held buttons tracked), Key→KeyDown(text rides input)+KeyUp, Scroll→Wheel (≤3 notch=Lines×20px else
Pixels), each→resolve+repaint ≤30fps; link nav via the wired NavigationProvider hook (e2e click
<a>→page2+title+Back) + input-survival test. Tab 27 green; shell still #9.
## #8 Boa JS layer (ironsurf-tab) — NEW src/js.rs: <script>s in document order (src= via
fetch_document ⇒ same net policy; module/JSON/unfetchable skipped w/ notes); ONE boa-0.22 Context per
navigation; 5 natives + trusted JS glue (global IS window): title get/set→TitleChanged+repaint, body
textContent snapshot, console→stderr+StatusText, every eval caught, timers counted-only (TODO), javascript:
refused at navigate(). 48 tab tests green (17 new js unit + 4 new e2e); no mozjs/spidermonkey/wgpu; workspace green ex-#9.
## #9 DONE — workspace green on Blitz stack; shell tabs are REAL — fixed slint 1.17 drift
(Color channels u8, Image::size()), pump test→tests/pump.rs (no CARGO_BIN_EXE in lib tests); fixed tab
history Fresh index (Back was dead; e2e wait_for vacuously re-matched old msgs→added wait_for_new); NEW
tests/real_tab.rs: real ironsurf-tab in shell — nav/back/fwd/frames/input headless (+IRONSURF_ONLINE=1→
example.com live); apt libxkbcommon-x11-0 for winit X11; docs SERVO→ENGINE_INTEGRATION.md (Blitz/Boa §A–D).
## #10 rename servfox→ironsurf — crates/dirs, crate+bin names, slint ui→ironsurf.slint, env IRONSURF_*,
all src/tests/docs/TASKLOG refs; Cargo.lock rewritten. cargo build+test --workspace: 130 passed, 0 failed.
