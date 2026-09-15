# Task log
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

## #18 DONE — real DOM for Boa (Acid3 foundation): thread-local LIVE_DOCS registry (raw ptr, only live during
one eval; LiveDocGuard<'a> + PhantomData keeps the &mut borrow exclusive) + ~44 __ironsurfDom* natives (tree,
attrs, mutate via DocumentMutator+flush, query w/ SyntaxError, RangeError names, clone shallow/deep, serialize)
+ PRELUDE glue: one NodeProto over {__d,__n} wrappers, realm-side __ironsurfNodeCache for identity, Proxy-based
LIVE NodeList/HTMLCollection/NamedNodeMap (re-read per access), JS DocumentFragment, id/className; run_scripts/
run_due_timers take Option<&mut HtmlDocument>, tab registers live doc + settles after scripts; --body-out now
shares that serializer. 16 bridge tests (identity, liveness, innerHTML round-trip, NotFound/Range/Syntax) +
headless smoke (script-built red div ⇒ >5k pure-red px in PPM, markup in --body-out); 72u/4 headless green,
workspace green. Known gaps: element-scoped queries are document-wide; no events/style/CSSOM yet.
## #16 GPU acceleration — slint built with backend-linuxkms-noseat (DRM/KMS + EGL/GBM FemtoVG; bare-VT GPU path);
main.rs force-software hack replaced by GPU-first policy: auto = winit/LinuxKMS FemtoVG with Slint's automatic
software fallback (--software pins old path, --linuxkms pins KMS, SLINT_BACKEND wins; pure backend_choice + test);
box has no GPU: FemtoVG still initializes via Mesa llvmpipe (libGL loaded, verified) — on real GPUs it binds vendor
drivers. KMS libs are LINKED not dlopen'd: build -dev pkgs + runtime sonames documented in BUILD_DEBIAN/
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
## player.rs compile-clean — VecDeque/Vec bridge: decode/flush emit into a local Vec then buffered.extend
(out) (VecDeque::append no longer takes &mut Vec); present_due holds an early frame as Some((pts, frame));
test hooks: set_test_metadata drops needless .clone() (E0716), seek test asserts player.current_pts() (Frame
has no pts), open_bytes expect_err→.err().expect (VideoTrack: !Debug). cargo check -p ironsurf-tab [plain +
--tests] show ZERO media/player.rs diagnostics (crate still red elsewhere).
## ironsurf-tab GREEN — full test suite 0 failed (161 bin + 18 other targets, timing suites stable ×3); cargo check plain + --no-default-features 0 errors.
player.rs: canplay fires once per load at first decoded picture; paused freezes pipeline slack (only first
frame or seek target reaches screen); paused needs_repaint = seek-await only, not in-flight decode.
widget.rs: kurbo/stylo call-shape (from_rgb8/rgba8, one-point BezPath, RoundedRect::from_rect, Alpha);
strip click at x==width seeks 1.0, picture area stays half-open. Demux fixtures made spec-shaped (stsc/stco
fields, plain strh/strf chunks, canonical EBML varints, RIFF movi children); XVID-class → "MPEG-4 Part 2".
## video wired — media engine live in the tab. runtime.rs: attach_video_widgets (blitz set_custom_widget + width/height attrs → VideoWidget::with_size_hint, 640×360 fallback, per-tick re-scan for late <video>), fetch worker threads (SelfPipe + results queue, container cookies/policy, 2xx + MAX_VIDEO_BYTES only), pump_media after run_due_timers with MEDIA_TICK=40ms capping the poll sleep, PlayerEvent→DOM events via __ironsurfFire, catch_unwind media_guard on every step; --no-media master switch. js.rs: __ironsurfMediaGet/MediaCmd natives + NodeProto HTMLMediaElement glue (currentTime/duration/paused/ended/playbackRate/muted/videoWidth/videoHeight/mediaError, play/pause/canPlayType, instanceof HTMLMediaElement), page writes queue MediaCommands the loop applies (MediaHost cell in HostState). Spec APIs adapted to shipped blitz 0.3 (set_custom_widget not Node::Widget; SelfPipe not to_main; std::thread not tokio); /tmp/vid/flower.mp4 is 960×540 (tests assert the demuxer's own reading). cargo test -p ironsurf-tab 190 green; cargo check --workspace clean (ironsurf-net/-session/-ironsurf are not packages in this tree).
