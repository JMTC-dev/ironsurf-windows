# Task log
## #7c/#7d Blitz IPC runtime + inputs — TabArgs CLI (+--offline-fixtures), sandbox ON (degrades, never aborts),
1-thread poll[stdin,self-pipe], nav/back/fwd/reload/resize/SetVisible, HtmlDocument per nav→settle→vello ≤30fps,
Title/Url/History dedupe, jars saved on Destroy/EOF; handle_input Pointer/Key/Scroll + link nav via the hook.
## #8 Boa JS layer (ironsurf-tab/src/js.rs) — <script>s in document order (src= via fetch_document ⇒ same net
policy); ONE boa-0.22 Context per navigation; 5 natives + trusted glue (global IS window): title get/set→
TitleChanged+repaint, body textContent, console→StatusText, eval caught, timers counted-only (TODO),
javascript: refused at navigate().
## #9 DONE — workspace green on Blitz stack; shell tabs are REAL — slint 1.17 drift fixed (Color u8, Image::size()),
pump test→tests/pump.rs (no CARGO_BIN_EXE in lib tests); history Fresh index fixed (Back was dead) + wait_for_new;
tests/real_tab.rs (+IRONSURF_ONLINE=1→example.com live); docs SERVO→ENGINE_INTEGRATION.md (Blitz/Boa §A–D).
## #10 rename servfox→ironsurf — crates/dirs, crate+bin names, slint ui→ironsurf.slint, env IRONSURF_*,
all src/tests/docs/TASKLOG refs; Cargo.lock rewritten.
## #11 instant-feeling link clicks + honest status bar — tab: coalesce_moves drops superseded hover moves per
stdin batch + no repaint when get_hover_node_id() unchanged (flood→click ack 19.98s→0.93s, frames 64→2), hover
<a href>⇒StatusText(target); shell: move_due 33 ms gate, LoadingChanged(true) clears sticky Error, spinner at load
start; live human-click 11.5s→1.5s; docs §E.
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
## #13 added debug and info messages to stderr of the process for debugging, for example when URL is changed, page is loading, console error messages etc.
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
