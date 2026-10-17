# TASKLOG

- **Security audit (high severity) — done.** Report: `docs/SECURITY_AUDIT.md`.
  18 PoC tests assert the secure behaviour and fail today: 6 findings, top two
  CRITICAL: (1) a web page reads the local disk via `location.href="file://…"`
  (no scheme gate in `follow_pending_js_nav`); (2) `location.href="ironsurf://
  archive?root=%2Fetc&host=0.0.0.0&port=…"` makes the shell export any folder
  over HTTP (runtime.rs:1262 → app.rs:3980 → normalize_root/host). Also: archive
  control socket = unauthenticated GET mutations; `percent_decode` char-boundary
  panic (path decoded twice) kills the server; no CORS on responses (cross-origin
  body + session-cookie read); no private-address rule; `container-id` unused.
