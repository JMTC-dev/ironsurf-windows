# Task log
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

## #12 docs/BUILD_DEBIAN.md — apt prereqs proven from cargo tree/build.rs/objdump: build-essential, pkg-config,
git/curl/ca-certs, libssl-dev (native-tls←reqwest0.13←blitz-net), libfontconfig-dev (usvg/fontique); X11/Wayland/GL
are dlopen'd so NO -dev headers; runtime lib list in that doc; Servo-era deps dropped from README+AGENTS.
## #13 stderr diagnostics everywhere — NEW ironsurf_common::logging (no deps): Off<Error<Warn<Info<Debug<Trace>,
error!/warn!/info!/debug!/trace!, level = `--log LVL|-v|-vv` → IRONSURF_LOG → RUST_LOG → info; lines
`[hh:mm:ss.mmm] LVL ctx mod: msg` (UTC; ctx `shell`/`tab 3`). Covered: tab startup+sandbox, navigate+fetch
(status/bytes/ms), parse/settle/script timings, page console.* AT THE PAGE'S LEVEL, url/title/loading, cookie +
protocol errors; netmod req/resp/cache/Set-Cookie/https-upgrade; shell spawn/destroy/reap, FromTab reduction,
address bar, shortcuts. stdout stays IPC-only ⇒ frames+input are TRACE-only (volume contract: nobody reads a tab's
stderr pipe) and a child inherits stderr only when debug+a-terminal (TabStderr, --tab-stderr, IRONSURF_TAB_STDERR).
docs §F + README; 133 tests green.
## #14 FIX: tab panic on real desktops — RLIMIT_NPROC is per-UID on Linux; absolute 128 < uid's live
tasks ⇒ every pthread_create EAGAIN ⇒ reqwest tokio build panicked (netmod.rs). Now NPROC = current
task count (from /proc Threads: for our euid) + max_processes headroom, never below live usage;
notes explain; ARCHITECTURE comment extended; +unit test + examples/nproc_check reproducer.
## #15 data: URLs are MIME-aware — netmod::parse_data_url (DataUrl{mime,charset,base64,bytes}) decodes
both RFC 2397 forms; data:text/javascript;base64,… typed/navigated to renders as INERT escaped source titled
by media type (render_document, between fetch and HtmlDocument::from_html) instead of being parsed as HTML —
executing it would be a javascript: URL by another name; markup (html/xhtml/svg/xml) parses as before, and
<script src="data:…"> still runs. Shell: scheme match case-insensitive + javascript: passed to the tab.
## #13 middle-click = open link in new tab — common: FromTab::OpenLinkInNewTab{url} (+round-trip, ARCH updated);
tab: middle Up over <a href> swallowed (no DOM click/nav hook), emits OpenLinkInNewTab; shell drain: spawns
sibling tab in same container via open_tab; state.reduce treats it as an action (no state change). UI already
forwarded button=2 (middle). cargo test --workspace green.
## #16 built-in start page ironsurf://welcome (was https://example.com) — common: WELCOME_URL +
default_url(); tab: netmod INTERNAL_SCHEME answered 100% locally (welcome page self-contained,
unknown page = honest internal 404, URL escaped not injected; main-doc + subresource paths),
status line shows full internal URL; shell classify: ironsurf:// pass-through (no-dot host
would've become a search); gui test + ARCH updated. Live: tab loads it, title "Welcome to
IronSurf"; workspace green.
## #17 ACID3 testing started — plan queued (#72–#79): acid3.acidtests.org fetched OK (self-contained 183KB page;
score lands in #score firstChild; tests run in setTimeout buckets; every CSS/selector test uses iframe#selectors
contentDocument+doc.write). Queue order: headless --headless harness → real timers → DOM bridge (identity-cached
wrappers {__d,__n}, LIVE NodeList, mutator-backed create/append/remove/query via blitz-dom) → getComputedStyle+el.style
→ DOM Events → contentDocument+write → acid3 measure+fix top misses → harden+docs. Baseline pre-queue: JS cannot
bootstrap acid3 at all (no createElement/timers). Design refs: docs/ACID3_STATUS.md will hold repro + score.
## #17b real JS timers in ironsurf-tab (unblocks ACID3 bucket queue step 2): js.rs TimerQueue
(ids start at 1, due-asc, intervals re-arm before run so clearInterval-inside works) + JsRuntime
(Context+host+queue alive for the document's life; thread-local scope stack); real
setTimeout/setInterval/clearAll/raf(16ms one-shot w/ timestamp) natives replace counted-only stub
(count kept in stats); live poll slice clamped by earliest deadline + run_due_timers (settle,
<=30fps repaint, throwing callback never kills tab); headless --wait pumps same queue. Fixed latent
take_due bug (dropped future entries) + next_id=0. Tests: 4 unit + e2e TitleChanged("timer-title") +
headless title=timer-fired. 56u/11 e2e/3 headless/3 idle/1 spike green; acid3 local mirror boots
without timer errors (buckets still need DOM bridge, next queue steps).
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
