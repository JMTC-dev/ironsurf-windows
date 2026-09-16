# Task log
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
## #38 Userscripts persisted in storage (step 2/5, storage only): global (not per-container) Vec<Userscript> at
  <root>/userscripts.json, sibling of containers.json — same pattern: load-if-exists (missing = empty, never
  seeded), write_atomic temp+rename, loaded at Store::open and kept in memory. API on Store: userscripts(),
  next_userscript_id() (max+1, 1 empty), upsert_userscript (insert/replace by caller-supplied id; name stored
  trimmed), set_userscript_enabled + delete_userscript (unknown id ⇒ NoSuchUserscript, tested). Validation: EmptyUserscriptName/EmptyUserscriptPatterns/EmptyUserscriptCode + UserscriptNameTooLong (200 chars) / UserscriptCodeTooLarge (1 MiB); rejected upserts leave list + file untouched. lib.rs layout comment synced; tests/userscripts.rs 8 green (+34 pre-existing), common 27 green. tab/shell untouched (steps 3–5).
## #37 Userscripts IPC contract (step 1/5, common only): Userscript{id,name,patterns,code,enabled} +
  ToTab::SetUserscripts{scripts} (shell sends all ENABLED scripts; tab filters per document URL every
  navigation; empty vec clears), userscript_matches + userscripts_for_url (case-insensitive; bare `*`;
  exact full-URL; `host/path` no-scheme sugar; `*domain[/path]` host-boundary wildcard — subdomains yes,
  notexample.com never; non-http(s) schemes exact-only). ARCH snippet synced; common 27 green (round-trip
  empty/one/unicode + table-driven matching + enabled filtering); tab/shell untouched (expected to gain
  the match arm in later steps).
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
