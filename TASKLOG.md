# Task log
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

## #19 FIX container manager "New" dead — row's selta TouchArea was declared AFTER the row content:
last-declared TouchArea wins hit-testing (proven with scratch slint/test-backend repro), so it swallowed
New/Move tab/Delete/Ren clicks (only ever fired mgr-select). selta moved first in the row (bottom z) ⇒
buttons clickable, row still selects. Same latent bug fixed in tab strip: tabta after xt killed the ✕ —
tabta first, ✕ in a glyph-sized Rectangle (bare TouchArea would fill the tab). Verified live on :2.0
via xdotool: New spawns tab 2, row-click selects, ＋ works, ✕ closes; shell suite green.
## #20 HTML <form> end to end — new src/forms.rs: spec entry list (tree order, form=/ancestor
owner, disabled/nameless/datalist out, checkbox/radio checkedness, live editor text, select
selectedness), GET query / POST bodies per enctype (urlencoded, text/plain, multipart), triggers
= armed submit press finalised on release (JS click preventDefault cancels; our entry list
replaces blitz's select-less one) + implicit Enter (not fed to blitz); selects clickable
(selected attr+highlight, value attr synced), prepare_document seeds textarea editors; POST rides
netmod::fetch_document_post/MainDocPost (real POST, never cached); GET works for file:/ironsurf:
(blitz drops), POST to local schemes = honest status; JS events replay via __ironsurfFire
(mousedown/up/click, keydown/up) + .value/.checked bindings (live editor); welcome page is a
working form demo echoing its query. 88u + all integration suites green; ARCH §HTML forms.
## #21 page zoom — common: +ToTab::SetZoom{zoom} (+round-trips, ARCH §5); tab: zoom state feeds
Viewport::zoom (blitz hidpi×zoom), SetZoom → set_viewport restyle + forced frame, logical→CSS px
for pointer coords + pixel wheel deltas (÷zoom), zoom survives navigation (fresh docs inherit) and
NaN/negative rejected; shell keymap: Ctrl-=/+/−/0 chords, ×÷1.1 whole-percent steps clamped 30–500%,
per-tab factor + "Zoom N%" status. Live xdotool 100→133% repaints (98.9k px differ); workspace green.
## #22 FIX inverted mouse wheel — ironsurf.slint negated Slint's wheel deltas before IPC, but Slint hands
over winit's deltas untouched and blitz beta.2 speaks that very convention (positive delta_y = scroll UP:
handle_wheel → scroll_chain_by feeds ScrollAmount::By{-x,-y}), so that extra flip reversed the wheel. Handler
now passes delta-x/delta-y verbatim; convention pinned in common::ScrollEvent docs + ARCH ToTab section and
by the new tab test wheel_down_scrolls_the_document_down_and_wheel_up_back_up (pixel deltas, 20px notch, clamp
at the top); workspace green.
## video wired — media engine live in the tab. runtime.rs: attach_video_widgets (blitz set_custom_widget + width/height attrs → VideoWidget::with_size_hint, 640×360 fallback, per-tick re-scan for late <video>), fetch worker threads (SelfPipe + results queue, container cookies/policy, 2xx + MAX_VIDEO_BYTES only), pump_media after run_due_timers with MEDIA_TICK=40ms capping the poll sleep, PlayerEvent→DOM events via __ironsurfFire, catch_unwind media_guard on every step; --no-media master switch. js.rs: __ironsurfMediaGet/MediaCmd natives + NodeProto HTMLMediaElement glue (currentTime/duration/paused/ended/playbackRate/muted/videoWidth/videoHeight/mediaError, play/pause/canPlayType, instanceof HTMLMediaElement), page writes queue MediaCommands the loop applies (MediaHost cell in HostState). Spec APIs adapted to shipped blitz 0.3 (set_custom_widget not Node::Widget; SelfPipe not to_main; std::thread not tokio); /tmp/vid/flower.mp4 is 960×540 (tests assert the demuxer's own reading). cargo test -p ironsurf-tab 190 green; cargo check --workspace clean (ironsurf-net/-session/-ironsurf are not packages in this tree).
