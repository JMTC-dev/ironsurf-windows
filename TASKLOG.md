# Task log

## #0 done — planning
Wrote ARCHITECTURE.md (crate contracts) + AGENTS.md (env notes); apt deps for Servo/X
(/tmp/apt.log → APT_EXIT=0). X at :2.0, no GPU. (Servo-era notes superseded by #6 pivot.)
## #1 done — workspace root + servfox-common
Root Cargo.toml (members crates/*, workspace deps slint/serde/bincode/thiserror/url,
profile.release debug=false) + .gitignore. servfox-common: full contract (ids, Container,
FromTab/ToTab variants, Pointer/Key/Scroll events, Modifiers u8-newtype, write_frame/read_frame
u32-LE+bincode w/ MAX_FRAME_BYTES both ways, default_url). 11 tests green. Next: #2 storage.
## #2 done — servfox-storage
Registry (atomic containers.json, default personal, rename w/ slug dir move), ContainerStore,
RFC6265-subset CookieJar (Secure https-only set+send, domain-suffix host-only convention, RFC
path-match, Max-Age>Expires, pruning, cookies.json), LRU NetCache (256MB/1000), history.jsonl.
28 tests green; workspace green. Next: #3 servfox-sandbox.
## #3 done — servfox-sandbox
prctl NO_NEW_PRIVS+PDEATHSIG, setrlimit NOFILE/NPROC/optional-AS (clamped to hard cap),
Landlock via landlock 0.4.7 BestEffort (v1 handled set; Refer dropped — v2 bit broke add_rule).
landlock 0.4.7 has NO KernelVersion::is_available(); probe syscall directly. sbox-probe bin:
exit 42 enforced / 40 graceful; LIVE HERE — forbidden profile-B read EACCES, ro ok, writes ok.
`cargo test -p servfox-sandbox` 7 green; workspace green. Next: #4 servfox-tab (Servo embed).
## #4+#5 done — servfox-tab Servo spike + runtime (SUPERSEDED by #6 pivot)
Servo 0.5 SoftwareRenderingContext spike, then full IPC runtime (sandbox ON, ToTab/FromTab,
≤30fps frames, netmod managed layer). Servo-specific gotchas preserved in docs/SERVO_INTEGRATION.md.
## #6 done — servfox-tab engine pivot: Servo→Blitz+vello_cpu (spike green)
Cargo.toml -servo/-dpi, +blitz-{dom,html,net,paint,traits}@0.3.0-beta.2 +anyrender 0.13
+anyrender_vello_cpu 0.17 (+kurbo/peniko types). spike.rs: from_html(DocumentConfig{base_url,
Viewport::new(w*s,h*s,s,Light),blitz_net})→resolve→render_to_buffer::<VelloCpuImageRenderer>
+paint_scene, buf==w*h*4, PPM proof; main.rs=CLI stub exit0, runtime/wake deleted, netmod+8 tests
kept, e2e #[ignore]; spike green headless, tree free of mozjs/wgpu/servo. Next: #7 Blitz IPC runtime.
