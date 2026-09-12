# Task log

## #0 done — planning
Wrote ARCHITECTURE.md (crate contracts) + AGENTS.md (env notes). apt install of Servo/X deps
running in background (log /tmp/apt.log, expect APT_EXIT=0).
Next-task notes: `servo = 0.5.0` resolves on crates.io (defaults: baked-in-resources,
bundled_freetype, clipboard, js_jit; keep multiprocess OFF). apt fix applied (libxcursor1),
re-running — /tmp/apt.log must end APT_EXIT=0 before building servfox-tab. X at :2.0, no GPU.
## #1 done — workspace root + servfox-common
Created root Cargo.toml (members crates/*, workspace deps slint/serde/bincode/thiserror/url,
profile.release debug=false) + .gitignore. crates/servfox-common: full contract (ids, Container,
FromTab/ToTab all variants, Pointer/Key/Scroll events, Modifiers u8-newtype w/ helpers,
write_frame/read_frame u32-LE+bincode w/ MAX_FRAME_BYTES guard both directions, default_url).
11 tests pass (`cargo test -p servfox-common`). Next: #2 servfox-storage.
## #2 done — servfox-storage
Registry (atomic containers.json, default personal, create/rename/recolor/delete w/ default-
protected + slug dir move on rename), ContainerStore, RFC6265-subset CookieJar (Secure set+send
https-only, domain-suffix w/ leading-dot host-only convention, RFC path-match, Max-Age>Expires,
expiry pruning, cookies.json), in-mem LRU NetCache (&self, 256MB/1000 evict), history.jsonl,
StoreError. 28 tests green (`cargo test -p servfox-storage`); workspace green. Next: #3 servfox-sandbox.
