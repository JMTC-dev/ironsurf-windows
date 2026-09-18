# Task log
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
## #43 Per-container User-Agent persisted (step 2/5, storage only): Store::set_container_user_agent(id, Option<&str>) writes Container::user_agent
  into containers.json via the atomic save(), shaped like set_container_color; Some(v) stores v trimmed, all-blank Some("") and None both store
  None (= the tab default); validation precedes mutation (upsert_userscript pattern) so rejects leave memory + the file byte-identical — control
  chars ⇒ InvalidUserAgent, > MAX_USER_AGENT_CHARS ⇒ UserAgentTooLong{length,max}, unknown id ⇒ NoSuchContainer. Docs synced; tests/user_agent.rs
  8 green (reopen, clear, A/B independence, trim, blank⇒None, cap boundary, control chars, bogus id) + 42 pre-existing, ws check clean.
## #42 Per-container User-Agent contract (step 1/5, common only): ToTab::SetUserAgent{user_agent} — Some(v) = send
  exactly v as the HTTP User-Agent on every later request, None = the tab's built-in default; Container gains
  #[serde(default)] user_agent: Option<String> (legacy containers.json written without the key still loads, as None);
  presets BUILTIN_USER_AGENTS + builtin_user_agent_label/is_builtin_user_agent/MAX_USER_AGENT_CHARS=512. Fallout only:
  inert SetUserAgent arms (tab runtime, fake_tab), user_agent: None (storage literals), ARCH synced; common 30 green, ws clean.
## #41 JS DOM changes now land in the live document, always: every JS turn that can mutate the tree — page-script
  batch, each userscript run, timer batches, javascript: programs — settles (restyle + relayout + repaint),
  and the three silent gaps behind the report are closed in js.rs: document.write/writeln really parse (fragment
  appended under <body> via a new __ironsurfDomDocumentWrite native — never replaces the running document);
  element.style is a live CSSStyleDeclaration view over the `style` attribute (member writes via Proxy, cssText,
  set/get/removeProperty, and whole-string `document.body.style = "…"` — all through ATTR_SET, verified against
  stylo computed values after resolve); document.dir/lang mirror the <html> attrs. Root cause underneath: html5ever
  stores HTML attrs in the EMPTY namespace while qual_name_for_attr built ns!(html), so JS attribute writes next
  to a parsed attr PUSHED shadow duplicates (reads kept answering the stale value) — fixed the mapping + regression
  test. tab 185u (write/style/dir-attr/restyle units) + userscripts e2e “DOM reflector” (write+style+dir over the
  wire), common/storage green, workspace check clean; ARCH §JavaScript reflection bullet.
## #40 Userscript MENU in the shell (step 4/5): glyph-free "US" toolbar button right of ＋ toggles a right-docked overlay (container-manager pattern, never PopupWindow; the two never stack — each closes the other). Rows: enable CheckBox persisted at once via Store::set_userscript_enabled, bold name, dim ", "-joined patterns, ✕ delete; "+ New script" inline editor (name/patterns LineEdits — patterns comma/space-split — multi-line TextInput for code); a rejected Save keeps the draft, shows the StoreError on one line, script NOT added. Broadcast: every mutation AND tab spawn/respawn sends ToTab::SetUserscripts with all ENABLED scripts to every live tab (crashed/closed skipped, send failures ignored — the crash reporter owns those); disabled never sent. Pure src/userscripts.rs (rows, enabled-only payload, parse_patterns, tempdir-Store reopen round-trips); fake_tab --expect-userscripts echoes "userscripts:[ids]" on its status line; tests/userscripts.rs drives save/validate/toggle/re-toggle/delete/seed/manager-exclusion over the real pipe; state.rs console_rows covers "[userscript] Name: msg"; shell 53u + all suites, common 27, storage green; ARCH §ironsurf-shell bullet added.
## #39 Userscripts execute in the tab (step 3/5): Tab stores ToTab::SetUserscripts (replace-wholesale; empty clears) and run_scripts ends with run_userscripts(url) —
  once per document (incl. JS-redirect targets), userscripts_for_url filtered at run time, shell store order, same Boa realm via JsRuntime::run_userscript(name, code,
  doc) with HostState::userscript naming the running script for that evaluation only: its console.* ships as ConsoleLog source "userscript" "{name}: {text}" (page keeps
  "js" verbatim; ConsoleLine carries the author), a throw as "userscript"/error "{name}: {err}" + StatusText "Userscript error in {name}", caught per script so the tab
  lives. 3 js.rs units + tests/userscripts.rs 3 localhost wire tests (patcher title, 'Boom script' named, unmatched silent, page-first order, empty clears, redirect re-runs, confirm() answered + attributed, alive+frame); tab 180u + every suite green, workspace check clean (fake_tab gained the missing SetUserscripts arm); ARCH/common docs name the source.
## #37 Userscripts IPC contract (step 1/5, common only): Userscript{id,name,patterns,code,enabled} +
  ToTab::SetUserscripts{scripts} (shell sends all ENABLED scripts; tab filters per document URL every
  navigation; empty vec clears), userscript_matches + userscripts_for_url (case-insensitive; bare `*`;
  exact full-URL; `host/path` no-scheme sugar; `*domain[/path]` host-boundary wildcard — subdomains yes,
  notexample.com never; non-http(s) schemes exact-only). ARCH snippet synced; common 27 green (round-trip
  empty/one/unicode + table-driven matching + enabled filtering); tab/shell untouched (expected to gain
  the match arm in later steps).
