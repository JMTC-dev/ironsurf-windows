# Task log

## #0 done — planning
Wrote ARCHITECTURE.md (crate contracts) + AGENTS.md (env notes). apt install of Servo/X deps
running in background (log /tmp/apt.log, expect APT_EXIT=0).
Next-task notes: `servo = 0.5.0` resolves on crates.io (defaults: baked-in-resources,
bundled_freetype, clipboard, js_jit; keep multiprocess OFF). apt fix applied (libxcursor1),
re-running — /tmp/apt.log must end APT_EXIT=0 before building servfox-tab. X at :2.0, no GPU.
