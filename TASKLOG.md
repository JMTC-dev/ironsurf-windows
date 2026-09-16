# Task log
## #36 Hover cursor completed (hand over <a>): the Slint half was broken — `Rectangle.absolute-x/-y` do not exist (Slint 1.17 exposes only
  `absolute-position`), and Rust has no `slint::MouseCursor` root export (it is `slint::private_unstable_api::re_exports::MouseCursor`, the very
  type the generated `set_page_cursor` takes). Page-point now maps through `page-frame.absolute-position`, the pump re-dispatches a synthetic
  PointerMoved (LogicalPosition) so a shape arriving while the mouse sits still still paints, and tab switches re-arm the per-tab shape.
  Tests: common 30-variant CursorChanged round-trip, shell state/map units + tests/cursor.rs (fake_tab --cursor: transitions only, in order,
  unchanged shapes silent), tab tests/hover_cursor.rs (REAL blitz engine over localhost: hand on `<a>`, `cursor: crosshair` overrides CSS-quiet
  heuristics, Out ⇒ arrow, no duplicate emits); live :2.0 xdotool hover over link/text/blank — alive, no panic, no dispatch loop. Workspace +
  release green (tab 177u+8 suites, shell 47u+12, common 24); ARCH FromTab snippet synced.
## #35 window.alert/confirm/prompt as real synchronous modals: common +FromTab::DialogRequest{dialog_id,kind,message,default}
  +ToTab::DialogResponse{dialog_id,confirmed,text} (DialogKind, MAX_DIALOG_MESSAGE 64 KiB, round-trip tests). Tab: DialogHost
  (runtime.rs) blocks the calling script in place — poll+read+decode stdin, every other ToTab deferred FIFO (take_deferred) and
  replayed by drain_stdin after the answer; js.rs __ironsurfDialog native + prelude trio (alert undefined, confirm bool, prompt
  string|null; default prefills only the box). EOF/broken stdout/no host ⇒ cancelled, never a hang. Shell: FIFO dialog queue
  (several tabs may each block), centred Slint overlay (scrim dead TouchArea, prompt LineEdit, OK/Cancel, prompt-Enter), requesting
  tab activated; answer/rollback on close/crash/reap (state.rs DialogRequest ⇒ false). fake_tab --dialog (+--crash order); tests:
  tab tests/dialogs.rs (3 real-engine localhost: trio return shapes, deferred Navigate in order, cancel false/null, EOF-cancel + exit
  0), shell tests/dialogs.rs (queue/activation/prompt text/cancel) + tests/dialog_crash.rs (crash closes the overlay); tab 178u,
  shell 60, common 23 all green; live :2.0 run (xdotool --window hover-then-XTEST click): alert OK, confirm Cancel, prompt default
  "Marty" edited → Enter, page title reads the answers. CLI: -u/--url start URL (usage error without value) and -h/--help usage.
## #34 DevTools verified end to end + docs/screenshots: new crates/ironsurf-shell/tests/devtools_e2e.rs spawns the REAL ironsurf-tab
  through the shell TabProc pump on a std::net localhost fixture — asserts the full console feed (js log/error, broken script + tab
  alive, dom + css source entries) and NetworkEvents (document + <img> valid-PNG, exact bytes, method/status/duration sane, status⊕error)
  and clean Destroy. cargo test --workspace green (incl. the new test); release green (rustc needed RUST_MIN_STACK for vp9dec).
  scripts/demo.sh reproduces docs/screenshots/devtools{,-network}.png on :2.0 (xdotool --window coords, URL bar, F12); README
  §Developer tools + ARCH §Developer tools, §DevTools CSS seam and tab emission/NetworkEvent notes.
## #33 DevTools panel UI in the shell (Slint): bottom-docked overlay Rectangle over the page (no PopupWindow — LinuxKMS), F12 (U+F70F, U+F70B also) / Ctrl-I toggle it per tab
  (keymap `Shortcut::ToggleDevTools`, consumed, page-focus only); header [Console n | Network n | ✕], Console rows (icon+colour per level, `[source] message`) + Network table
  (Method|URL|Status|Size|Time, unknown `—`, failures show the error text in red). Pure view-models `TabState::console_rows/net_rows` + `format_size/duration`; app.rs keeps the
  open flag+pane on the tab record, pushes rows into shared VecModels from sync_ui and the drain tick (reduce says `false` for feed msgs, so nothing else would); fake_tab
  `--devtools-feed`; keymap/state/app units + tests/devtools.rs; shell 44u + all suites, common 22, workspace build green; :2.0 pixel-diff confirms the 1024x240 band paints on F12.
## #32 DevTools console on the wire: ironsurf-tab now emits FromTab::ConsoleLog for every JS console.* line (all four levels, page order, source "js"), every uncaught script exception (source "js", or "dom" when a native __ironsurfDom* threw — the registration macro records native Errs into HostState::dom_throws and JsRuntime::classify_throw matches the eval-time message with boa's (native)/(at :l:c) Display suffix stripped; reported exactly once, StatusText unchanged), and a NEW src/logging.rs CSS seam: <style> bodies + inline style="" re-parsed through stylo's real ParseErrorReporter (blitz passes None upstream — gap documented in-code) plus wrong-Content-Type .css responses from BlitzNetProvider; tests/console_log.rs (5 localhost wire tests: levels+order, broken script + tab alive, dom-once, css seams, content-type ±control) + js/logging units; tab 176u + all suites green.
## #31 Network requests ride the IPC (DevTools Network panel): one FromTab::NetworkEvent per settled/failed
request — method, URL, status, bytes, duration_ms; docs report every redirect hop (*_report fetch variants),
NetProvider subresources + <script src> push into net_events drained per poll tick; failures error Some/status
None (upgrade block names the requested URL); revalidated cache hits report the entry's status/size, data:/
unsupported silent (no-spam); localhost wire test tests/network_events.rs + tab 172u + e2e 14 + suite green.
## #30 Shell-side DevTools state reduction: TabState now carries a bounded console log (Vec<ConsoleEntry{source,level,
message}>, cap MAX_CONSOLE_ENTRIES=500) and a running network log (Vec<NetEvent>, cap MAX_NETWORK_ENTRIES=500, never
cleared on nav), both appended oldest-dropped-first via push_bounded and never dirtying the frame (reduce returns false).
5 state.rs unit tests cover append order, ring caps, success+failure net events; ironsurf-shell --lib 37 + full suite green.
## #29 DevTools IPC contract: FromTab::ConsoleLog gained `source` ("js"|"css"|"dom"|"page") and there is a new
FromTab::NetworkEvent { event: NetEvent } — NetEvent{method,url,status,response_bytes,duration_ms,error}
(error is Some exactly when the request failed, status then None). Shell console log carries the source, both
messages are no-ops for TabState (state.rs arms added), tab logs a net one-liner; ARCH §ironsurf-common snippet
synced; common round-trips (all four sources, success/failure/bare events), common 22 + shell --lib 32 + pump green.
## #28 Right-click context menu + session clipboard: tab hit-tests the cursor (link/image/video+playing/selection
/caret → FromTab::ContextMenu{x,y,ContextTarget}); shell renders a Slint overlay menu whose rows are the pure
context_menu_items matrix (Back/Forward/Reload, open link/image in new tab, Copy selection/link/image/video/page
address, Play/Pause video, Paste only editable+non-empty clipboard). Sandbox-safe clipboard: ShellProvider
copy→FromTab::ClipboardText→shell store, Ctrl+V preceded by ToTab::Clipboard push; Paste/ToggleVideo ride
ToTab::ContextAction. common round-trips + shell unit matrix + context_menu e2e green; workspace suites green.
## #27 Crashed tabs are loud: dead child (try_wait poll in every drain tick) or broken-pipe send now pins
"Tab N has crashed" on the status line (tab kept in strip, last frame; further commands dropped, ✕ closes),
tab stderr is captured (bounded 16K tail, was /dev/null) + RUST_BACKTRACE=full for children + tab panic hook
force-prints backtrace, so the log carries exit status + panic + backtrace; fake_tab --crash; pump + gui-level
crash tests; shell suite green (26u + pump 2 + gui + real_tab + tab_crash).
## #26 FIX video play button dead — widget drew the play/pause glyph INSIDE the control strip (x=12, bottom 22px)
but click_action mapped the whole strip to Seek: the visible button only sought ~4% (nothing visible). Strip left zone
(PLAY_ZONE_WIDTH=30) now toggles, bar maps onto its drawn track (BAR_X/BAR_RIGHT_MARGIN shared with paint); strip test
updated + new unit test; tests/video_play_button.rs spawns the real tab on file:///tmp/vid/flower.mp4 (fixture MP4 is
actually 960×540) and proves strip-click pause/resume from the pixel stream; tab 169u+14e2e+4+3+1+1, workspace green.
## #25 JS-driven navigation implemented (was: refused) — location.href=/assign/replace/reload queue
js::PendingNav on HostState via __ironsurfNavigate (glue location also gained real protocol/host/
hostname/pathname/search/hash getters); Tab::drain_nav_hook follows it after every JS turn (relative
resolve vs doc URL; replace→Session(idx) history); javascript: URLs execute in the live page realm
(run_javascript_url, pre-parse raw check + percent-decode) with string completion value → new
about:blank doc replacing the entry (spec); netmod still never *fetches* javascript:; 4 e2e green
(in-page exec, string→doc, href-push+Back, replace-hides-referrer) + js unit tests; ARCH/docs synced.
## #24 search engine → https://html.duckduckgo.com/html/?q= (was duckduckgo.com/?q=) — classify::search_url +
all test expectations + gui.rs + ARCH §URL-bar updated; ironsurf-shell suite green (26u + gui/e2e).
## #23 cookies actually returned to sites — navigations are never cookie-less: plan_document (old plan dropped)
routes every top-level http(s) load through managed_http_fetch EVEN under default Passthrough (Passthrough now skips
only https-upgrade+cache); redirects followed hop-by-hop on a Policy::none() client (FetchWorker::managed_client):
per-hop jar Cookie (cross-host redirect never replays blindly; localhost≠127.0.0.1 proven), per-hop Set-Cookie
capture (chain cookies reach the final hop), POST→GET on 303/301/302, 307/308 keep body; Tab+provider share ONE
Arc<Mutex<CookieJar>> (was two jars, last-save-won clobbered subresource cookies). cookies.rs RFC6265 hardening:
§5.1.4 path-match exact (Path=/foo hits /foo+/foo/bar, /foo/ never /foo; no trailing-slash normalization), Domain= on
IP host or invalid/single-label → reject, malformed Max-Age → delete, already-expired Expires deletes not stores;
HttpOnly persisted+sent normally, scripts have zero cookie access; per-container jars = profile dirs
(containers_have_separate_jars test + sandbox probe). localhost HTTP wire tests (redirect chain, path/domain scope,
cross-host, Passthrough nav), runtime ptr_eq shared-jar test; ARCH cookie rules+net layer updated; workspace green,
live example.com real_tab ok.

## video wired — media engine live in the tab. runtime.rs: attach_video_widgets (blitz set_custom_widget + width/height attrs → VideoWidget::with_size_hint, 640×360 fallback, per-tick re-scan for late <video>), fetch worker threads (SelfPipe + results queue, container cookies/policy, 2xx + MAX_VIDEO_BYTES only), pump_media after run_due_timers with MEDIA_TICK=40ms capping the poll sleep, PlayerEvent→DOM events via __ironsurfFire, catch_unwind media_guard on every step; --no-media master switch. js.rs: __ironsurfMediaGet/MediaCmd natives + NodeProto HTMLMediaElement glue (currentTime/duration/paused/ended/playbackRate/muted/videoWidth/videoHeight/mediaError, play/pause/canPlayType, instanceof HTMLMediaElement), page writes queue MediaCommands the loop applies (MediaHost cell in HostState). Spec APIs adapted to shipped blitz 0.3 (set_custom_widget not Node::Widget; SelfPipe not to_main; std::thread not tokio); /tmp/vid/flower.mp4 is 960×540 (tests assert the demuxer's own reading). cargo test -p ironsurf-tab 190 green; cargo check --workspace clean (ironsurf-net/-session/-ironsurf are not packages in this tree).
## Ctrl-L/F6 now highlight the whole URL-bar address: UrlBar's focus-token handler pairs edit.focus() with
  edit.select-all() (Slint 1.17 LineEdit), so one character typed after the chord replaces the address (placeholder
  text now says "Ctrl-L or F6"); keymap adds bare F6 (U+F709, unmodified — alt/meta/ctrl combos untouched) →
  Shortcut::FocusUrl with unit tests; new tests/url_bar.rs drives the testing backend (window-activated, page click,
  invoke_page_key chords, synthetic keypress) asserting typing replaces the URL after Ctrl-L and F6; shell suite green.
