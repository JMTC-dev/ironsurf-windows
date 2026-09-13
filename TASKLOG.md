# Task log

## #0 planning — ARCHITECTURE.md + AGENTS.md; apt APT_EXIT=0; X at :2.0, no GPU (Servo-era notes superseded by #6)
## #1 servfox-common — full IPC contract (ids, Container, FromTab/ToTab, Pointer/Key/Scroll, Modifiers
u8, u32-LE+bincode frames, default_url). 11 green.
## #2 servfox-storage — Registry, ContainerStore, RFC6265 CookieJar (Secure https-only, Max-Age>Expires),
LRU NetCache (256MB/1000), history.jsonl. 28 green.
## #3 servfox-sandbox — prctl NO_NEW_PRIVS+PDEATHSIG, setrlimits, Landlock 0.4.7 BestEffort (probe the
syscall directly). sbox-probe LIVE HERE. 7 green.
## #4+#5 Servo spike+runtime — SUPERSEDED by #6; gotchas in docs/SERVO_INTEGRATION.md
## #6 pivot Servo→Blitz+vello_cpu — blitz@0.3.0-beta.2 + anyrender 0.13 + anyrender_vello_cpu 0.17;
spike.rs headless parse→resolve→render_to_buffer proof; e2e #[ignore].
## #7a docs/BLITZ_API_NOTES.md — vendor-verified cheat-sheet: NetHandler=1 fn; nav via NavigationProvider
hook; set_viewport not resize(); has_changes() INVERTED; Doc not Send/Sync→one thread; vello=premul RGBA8.
## #7b netmod — BlitzNetProvider (jar/cache behind Mutex; reqwest-0.12-rustls workers; error⇒drop
handler) + fetch_document: https-upgrade+block page, cookies, ETag. 16 green.
## #7c Blitz IPC runtime core — TabArgs CLI (+--offline-fixtures fallback), sandbox ON (degrades never
aborts), 1-thread poll[stdin,self-pipe], nav/back/fwd/reload/resize(set_viewport)/SetVisible, HtmlDocument
per nav→settle→vello ≤30fps, Title/Url/History dedupe, Shell+Nav hooks, jars saved on Destroy/EOF. NOTE:
servfox-shell fails PRE-EXISTING at HEAD (slint 1.x drift) — task #9 owns it.
## #7d done — inputs + link-click navigation (ACHIEVED, no limits hit)
handle_input LIVE: Pointer Move/Down/Up/Over/Out→UiEvent (CSS-px page coords, held-buttons tracked),
Key→KeyDown(text rides input)+KeyUp, Scroll→Wheel (≤3 notch=Lines×20px else Pixels); each input→
handle_ui_event+resolve+repaint (≤30fps, SetVisible). Link nav via the wired NavigationProvider hook
(no fallback needed): e2e click fixture <a>→UrlChanged page2+title+Back; +input-survival test (Tab/
Shift-Tab/print/scroll, frames keep coming). Tab 27 green; shell still #9. Next: #7e final validation.
