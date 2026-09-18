# Task log
## #60 startpage search bar still unclickable/untypeable — ROOT CAUSE FOUND + fix verified LIVE, stopped mid-test-writing; WIP (runtime.rs UA patch + forms::UA_PATCH_CSS + new e2e test) is in `git stash` (stash@{0}) — pop it to finish.
  Root cause (#59 was necessary but insufficient; proven via live element_from_point probes on :2.0): blitz-dom beta.2's default.css lacks `input[type=hidden]{display:none}`, so
  startpage's flexed search `<form>` lets its ten hidden inputs eat every flex pixel and query `#q` collapses to width 0 — painted by its container but unhittable (right-click ⇒ editable:false, click focuses nothing).
  Fix = `ua_stylesheets: Some(vec![blitz_dom::DEFAULT_CSS, forms::UA_PATCH_CSS])` in `Tab::load_html` (Some REPLACES the default); UA_PATCH_CSS = `input[type="hidden"]{display:none}`; verified live — click focuses #q and typed glyphs paint.
  Values still submit (forms.rs reads attributes, not boxes). The console's CSS errors (-webkit/-ms input-placeholder, -webkit-scrollbar SelectorError) are benign — those rules are correctly dropped and unrelated. TODO next session: the stashed
  e2e `search_bar_with_hidden_inputs_is_clickable_and_typeable` passes the editable-wait but fails its paint assert (band ink 19 vs >100 — frame timing/band geometry); fix that, run the suites, commit.
## #59 startpage.com search bar usable again: its first inline script (matchMedia → `.dataset.siteTheme` → revealRoot, which
  lifts the page-shipped `#root { opacity: 0 }`) died at :2:51 with "TypeError: not a callable function", leaving the WHOLE page —
  search bar included — invisible. js.rs glue gains **window.matchMedia** (MediaQueryList; matches only for ""/all/screen/only
  screen, listeners register-and-never-fire — no feature evaluation) and **element.dataset** (live Proxy over data-*, spec camel
  mapping siteTheme⇄data-site-theme, writes via ATTR_SET so they restyle, enumerable + delete). Plain <form action=/sp/search
  method=post> then submits through the existing forms.rs path. 3 units (matchMedia surface, dataset round-trip, reveal e2e) +
  tests/headless.rs pixel regression (same CSS: no script ⇒ box unpainted; reveal script ⇒ ~24 000 px paint, no TypeError); real
  startpage.com load logs ZERO js errors and paints; tab 225u + all suites green, ws check clean; ARCH §JavaScript synced.
## #58 Tab no longer dies on script-heavy pages (startpage.com crash): runtime's fetch_many spawned ONE scoped thread per
  `<script src>`; under a thread limit a script-rich page hit EAGAIN in thread::Builder::spawn (scope.spawn unwraps) and the process
  died. Now a CAPPED pull pool: MAX_SCRIPT_FETCHERS=8 workers grab indices off a shared AtomicUsize and write positional result slots
  (execution order unchanged); spawns go through Builder::spawn_scoped (io::Result, not panic) and the tab thread works the queue too,
  so EAGAIN means fewer workers — never a dead tab. 24-script e2e (3x the cap) proves every body runs in document order; tab 223u + all suites green, ws check clean.
## #57 Loading bar in the status bar's left-hand end: AppWindow gains `loading` + `load-progress` (0…11), `Core::tick_spinner`
  (120 ms timer, now pub) drives them from the **active tab's** `loading` state — the step only advances while a load is in flight and
  zeroes when one starts, `load_sweep_position` folds it to a triangle wave (out 0…11, back 11…0, never 3 ticks in one place); ui draws a
  96×4 recessed track (#3a3a3a) with a 24 px lit segment (#5b9dd9) shrinking at the far end, and the text column indents 106 px while it is up
  (rects live outside the layout so an empty one cannot push the spinner). Loading is one bool on the wire — no byte counts — so the sweep is
  the progress honestly. Sweep unit test + tests/loading_bar.rs (real ironsurf-tab: fast fixture idle → navigate to a 3 s blocking-script page →
  window shows the load in flight, segment travels, both drop when the tab finishes; :2.0 screenshots confirm 4 px/tick travel); shell 102u + all
  suites green, ws check clean; ARCH §Status bar synced.
## #56 Alt-1 … Alt-9 switch to the N-th tab, dynamically: keymap reports the digit only (`Shortcut::SelectTab(n)`);
  `tab_index_for_digit(n, tab_count)` resolves it **at press time** (1…8 = that position while it exists, 9 = **last** tab
  from nine on, no tab behind it ⇒ dropped), so closing a tab re-slots every shortcut by itself; inverse `tab_shortcut_digit`
  feeds the new `TabInfo.shortcut` so each tab paints the Alt-key reaching it today (positions behind the last show none).
  Alt-digits accepted plain, meta-escaped (`0xB1`…`0xB9`) and shifted (`!`…`(`); 3 keymap units + tests/tab_switch_shortcuts.rs
  (real App/fake_tab: 3 tabs → Alt-1/2/3, Alt-9 dropped, close front tab ⇒ old Alt-3 tab answers Alt-2, 11 tabs ⇒ Alt-9 = last,
  badges 8/""/9, close last ⇒ Alt-9 follows the end), welcome page + ARCH §ironsurf-shell synced; shell 104u green, ws check clean.
## #55 Fresh cache entries skip the network: storage gains pure `fresh_until(headers, cached_at)` — explicit
  freshness only (Cache-Control s-maxage>max-age>Expires via a tiny IMF-fixdate parser; no-store/no-cache/
  must-revalidate/proxy-revalidate ⇒ None; quoted values, folded headers, `-1`/garbage ⇒ revalidate), nothing new
  stored on CachedEntry; managed_http_fetch_report serves status<400 explicitly-fresh hits directly (from_cache, ONE
  net_ok on the original URL, duration 0, **no Set-Cookie re-capture**), everything else keeps the 304 path. 8 storage
  + 2 localhost tab tests (fresh ⇒ server sees exactly 1 request; no-store/no-cache/must-revalidate/proxy-revalidate ⇒
  always 2); storage 58 green, tab 222u, ws green; ARCH §Network layer + DevTools bullets synced.
## #54 `<script src>` bodies download CONCURRENTLY: js.rs splits extraction into a shared `walk_scripts` (type/src rules, notes
  kept in document order via `PendingScript::{Inline,External,Note}`) + shared `assemble_scripts`, driven from the unchanged
  `extract_scripts` (sequential, tests/compat) and new `extract_scripts_batch` (ONE `fetch_many(&[&Url])` call); runtime.rs runs fetch_many via
  `std::thread::scope` — one thread per URL on the shared client, jar/cache/UA/events/archive Arc-cloned, locked only per-operation, results positional
  so execution stays document order (≤1 script inline; concurrent cookie semantics noted). 5 batch unit tests; tab 220u + ws suites green.
## #53 No jar/cache mutex is held across the wire any more: netmod gains `JarAccess`/`CacheAccess` (impl'd for CookieJar/NetCache, `Mutex<..>`,
  `MutexGuard<..>`) and `managed_http_fetch_report`/`fetch_document_*_report` go generic over them, touching state only per operation
  (`cookie_header_at`/`capture_set_cookies_at`/`lookup_at`/`store_at`); spawn_http + document + `<script src>` + media pass the shared
  `Arc<Mutex<..>>` handles instead of guards, so a page's images/CSS/fonts download in parallel and `Tab.cache` is `provider.cache_handle()`
  (ONE NetCache per tab). New rendezvous unit test (503 if a lock serialises the pair), tab 215u + all suites green, ws check clean.
## #52 One shared FetchWorker per process: BlitzNetProvider::new now takes shared_worker() (the static OnceLock already
  used by fetch_document_report) instead of FetchWorker::new(), so a tab's document, scripts, subresources and media all ride
  ONE reqwest pool (keep-alive, TLS sessions, DNS cache) — previously two clients could never share connections. Grep-verified
  no other production FetchWorker::new/default/with_timeout remains (only test-only worker_for_test with_timeout left);
  with_worker() override kept for custom-timeout tests. tab all suites green, cargo check --workspace clean.
## #51 Transport tuning (tab): reqwest 0.12→0.13 (one reqwest in the tree with blitz-net; `rustls-tls`→`rustls`) + gzip/brotli/
  deflate/charset features; FetchWorker::built both clients gain pool_max_idle_per_host(16), connect_timeout(10s), tcp_nodelay; main
  client also HTTP/2 keep-alive interval 30s/timeout 10s + adaptive window (0.12's blocking builder lacked the keep-alive knobs — reason
  for the bump). No test touched Accept-Encoding (grep-verified); tab 256u green, ws check clean (KMS -dev libs reinstalled via apt).
## #50 Archive-server front page pre-traverses the timeline: `/` renders each visit row AND its saved sites as
  `<visit>/<host[:port]>/` child links under it (capped 250 visits x 100 entries, "… more" dim rows, unreadable
  visits note themselves, never break the page), so no timestamp needs a click to reach an archived page; deeper
  listings stay flat, symlinked folders are listed but never traversed (file_type, not metadata). 2 units + e2e
  wire assert + ARCH §Pages synced; shell 96u + all suites green (KMS -dev libs reinstalled via apt for linking).
## #48 A new tab opens in the address bar: every tab open (Ctrl-T, ＋, manager New tab, middle-click / OpenLinkInNewTab,
  menu open-in-new-tab) fills url-text with the spawn URL and bumps the SAME focus-url-token Ctrl-L / F6 use — the UrlBar takes
  focus with the **whole address highlighted**, so the first keystroke replaces it; a session's first tab still hands focus to
  the page (open_first_tab's page token lands last). New Core::url_draft (Cell<Option<TabId>>): while it names the active tab,
  sync_ui stops pushing that tab's URL, so its own load/redirect cannot clobber the half-typed address — ended by submit, Esc
  (which re-syncs the bar), a page pointer Down, another tab becoming active, that tab dying. LineEdit.has-focus reads stale
  till the next event, so the session is tracked in Rust, not slint. tests/new_tab_url_bar.rs (own binary — IRONSURF_DATA_DIR
  is process-global): token bump, keystroke replaces, announce cannot clobber, Esc restores, page click drops the draft; shell
  65u + every suite green; :2.0 real app — Ctrl-T then typing logs exactly `address bar: "example.com"`; ARCH §focus routing.
## #47 Archive mode (💾): common gains ToTab::SetArchiveMode{enabled} + FromTab::ArchivedFetch{url,is_document,bytes};
  tab archives NOTHING itself — archive.rs sink (AtomicBool + queue; --archive-mode seeds, SetArchiveMode moves, off drops
  the queue) fed by navigate Ok(<400, document), spawn_http (subresources), script-src, and media, drained like net_events;
  shell owns disk: archive.rs (pure: $HOME/Downloads/ironsurf-tabs default, ISO8601 %Y-%m-%dT%H:%M:%S%:z **tab-process
  SPAWN time** = visit_dir per Proc, host[:port]/index.html documents, sanitized subresource paths, traversal refused),
  💾 button + fourth never-stacking overlay (CheckBox persisted in ui.json with the Apply-validated root, status counts saves),
  pump writes in-flight-checked, spawn adds --archive-mode; fake_tab --expect-archive-mode + scripted fetches; tests:
  archive.rs 9u + e2e + real_tab_archive.rs (localhost server, doc+css on disk, one folder), tab 3u, common+shell+ws green.
## #46 Docs-only: README gains §Per-container User-Agent (UA button, Default + 5 presets + Custom, applies to every later
  request of the active tab's container incl. navigator.userAgent live, persists in containers.json) before Feedback; welcome page
  (netmod::WELCOME_PAGE) gains §User agent after Containers and loses §Network policy + §Try a form (form, {query} echo, and the
  query-echo arm of internal_page_bytes removed with them — no test referenced them); tab 196u green.
## #45 Per-container UA switcher in the shell (step 4/5): "UA" MiniButton right of "</>" → right-docked overlay (house pattern; manager/US/UA
  never stack). Default row + 5 BUILTIN_USER_AGENTS presets, container value highlighted (pure src/user_agents.rs, 4 units). Mutations via
  Store::set_container_user_agent for the ACTIVE tab's container, broadcast SetUserAgent to THAT container's live tabs only; Copy ⇒ effective
  UA (tab-default const when None), custom Apply keeps the draft on StoreError; spawn/respawn appends --user-agent per-spawn (extra_tab_args
  never mutated). fake_tab --expect-user-agent (launch --user-agent = current), tests/user_agent.rs e2e green; shell 57u + all suites, ws clean.
## #44 Per-container UA applied in the tab (step 3/5): effective UA cell (netmod::SharedUserAgent=Arc<Mutex<String>>) seeded by new
  --user-agent (absent/blank⇒USER_AGENT), replaced in place by SetUserAgent (Some(v)⇒v.trim(); empty/None⇒default; no reload). One cell
  feeds every request — documents (fetch_document* report forms take it; wrappers default), form POSTs, redirect hops, subresources
  (with_user_agent), media — and navigator.userAgent via live __ironsurfUserAgent over HostState.user_agent (same Arc as .dialog). 9 unit
  + tests/user_agent.rs 2 wire tests; tab 196u all green, ws check clean.
