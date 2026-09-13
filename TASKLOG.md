# Task log

## #0 done — planning
ARCHITECTURE.md + AGENTS.md; apt deps (APT_EXIT=0). X at :2.0, no GPU. (Servo-era notes superseded by #6.)
## #1 done — workspace root + servfox-common
Root Cargo.toml (members crates/*, workspace deps, profile.release) + servfox-common: full IPC contract
(ids, Container, FromTab/ToTab, Pointer/Key/Scroll, Modifiers u8, u32-LE+bincode frames, default_url). 11 green.
## #2 done — servfox-storage
Registry (atomic containers.json), ContainerStore, RFC6265 CookieJar (Secure https-only set+send,
domain/host-only, Max-Age>Expires, cookies.json), LRU NetCache (256MB/1000), history.jsonl. 28 green.
## #3 done — servfox-sandbox
prctl NO_NEW_PRIVS+PDEATHSIG, setrlimits, Landlock via 0.4.7 BestEffort (probe syscall directly —
crate has no is_available()). sbox-probe LIVE HERE: forbidden reads EACCES, ro ok, writes ok. 7 green.
## #4+#5 done — Servo spike+runtime (SUPERSEDED by #6; gotchas kept in docs/SERVO_INTEGRATION.md)
## #6 done — engine pivot Servo→Blitz+vello_cpu (spike green)
Cargo: -servo/-dpi, +blitz-{dom,html,net,paint,traits}@0.3.0-beta.2, +anyrender 0.13, +anyrender_vello_cpu 0.17.
spike.rs = parse→resolve→render_to_buffer::<VelloCpuImageRenderer>+paint_scene headless proof; e2e #[ignore].
## #7a done — docs/BLITZ_API_NOTES.md (vendor-verified API cheat-sheet)
NetHandler=1 fn (bytes), no Response/ResourceType; nav via NavigationProvider hook; set_viewport not
resize(); has_changes() INVERTED; Doc not Send/Sync→drive one thread; vello buf = premultiplied RGBA8.
## #7b done — netmod Blitz NetProvider + managed fetch_document
BlitzNetProvider (jar/cache behind Mutex; reqwest-0.12-rustls worker threads; error⇒drop handler).
fetch_document = https-upgrade+block page, cookie inject/capture, NetCache ETag. 16 unit tests green.
## #7c done — Blitz IPC runtime core (runtime.rs + main.rs), e2e green
New runtime.rs: TabArgs CLI (+--offline-fixtures basename fallback), sandbox ON (landlock LIVE here,
degrades never aborts), 1-thread poll[stdin,self-pipe], ToTab nav/back/fwd/reload/resize(set_viewport)/
SetVisible, HtmlDocument per nav→settle→vello frames ≤30fps, Title/Url/History dedupe, Shell+Nav hooks
wired, jars saved on Destroy/EOF; handle_input left as the 7d extension point. Both e2e tests un-ignored
and pass WITH sandbox ON; tab 25 green. NOTE: servfox-shell build fails PRE-EXISTING at HEAD (slint
1.x syntax drift) — task #9 owns that fix. Next: #7d input + link clicks.
