//! Userscripts end to end on the REAL engine: the full feature chain, from a
//! persisted Store on disk to script side effects observed over the shell's
//! own IPC plumbing.
//!
//! Modeled exactly on `tests/devtools_e2e.rs`: spawns the actual
//! `ironsurf-tab` binary against a tiny `std::net::TcpListener` fixture on
//! 127.0.0.1 and drives it through the shell's [`TabProc`]/[`Pump`] — the
//! exact path production uses. Two scripts live in a real
//! [`ironsurf_storage::Store`] (tempdir):
//!
//! * `Title bump` (pattern `*127.0.0.1`) sets `document.title` through the
//!   same DOM surface page scripts use → `TitleChanged` proves it ran;
//! * `Crash pad` throws → a `ConsoleLog { source: "userscript" }` whose
//!   message **names the throwing script** (the headline requirement), while
//!   the tab survives and keeps painting frames.
//!
//! The wire payload is built by the shell's own
//! [`enabled_userscripts`] — the same enabled-only reduction the "</>" menu
//! broadcasts. Disabling both scripts in the Store and re-sending empties the
//! tab: the next reload shows every side effect gone.
//!
//! Headless: no `$DISPLAY` required, localhost only. Skips (like
//! `devtools_e2e.rs`) when the tab binary was not built with the workspace.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};

use ironsurf_common::{ContainerId, FromTab, TabId, ToTab};
use ironsurf_shell::userscripts::enabled_userscripts;
use ironsurf_shell::tabs::{Pump, TabProc};
use ironsurf_storage::Store;

/// First paint of the real engine is slow on cold caches (same budget as
/// `devtools_e2e.rs`, which waits for several legs at once). Overridable
/// with `IRONSURF_E2E_DEADLINE_SECS` for quick local debugging runs.
fn deadline() -> Duration {
    std::env::var("IRONSURF_E2E_DEADLINE_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .map(Duration::from_secs)
        .unwrap_or(Duration::from_secs(90))
}

/// The page both scripts target (pattern `*127.0.0.1`). Its own title and
/// its own `console.log` are the controls: the userscript must beat the
/// title, and the page's log must keep its `"js"` source verbatim.
const PAGE: &str = r#"<!doctype html>
<html>
  <head><title>Userscript e2e</title></head>
  <body style="margin:0;background:#ffffff">
    <p>target</p>
    <script>console.log("page-line");</script>
  </body>
</html>
"#;

/// Minimal HTTP/1.0 fixture on 127.0.0.1 (one request per connection, kept
/// alive for the whole test process); returns the bound port.
fn spawn_local_http() -> u16 {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind loopback");
    let port = listener.local_addr().expect("addr").port();
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut buf: Vec<u8> = Vec::new();
            let mut byte = [0u8; 1];
            while !buf.ends_with(b"\r\n\r\n") {
                match stream.read(&mut byte) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => buf.push(byte[0]),
                }
                if buf.len() > 64 * 1024 {
                    break;
                }
            }
            let response = http_response(200, "text/html", PAGE.as_bytes());
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
        }
    });
    port
}

fn http_response(status: u16, content_type: &str, body: &[u8]) -> String {
    let reason = match status {
        200 => "OK",
        404 => "Not Found",
        _ => "Status",
    };
    format!(
        "HTTP/1.0 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    ) + &String::from_utf8_lossy(body)
}

/// The workspace-built `ironsurf-tab` (same resolution as `devtools_e2e.rs`).
fn real_tab_bin() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let profile_dir = exe.parent()?.parent()?; // target/<profile>
    let candidate = profile_dir.join(format!("ironsurf-tab{}", std::env::consts::EXE_SUFFIX));
    candidate.is_file().then_some(candidate)
}

/// The feed accumulated off the wire (console entries in arrival order, one
/// title per document/script pass, every painted frame counted).
#[derive(Default)]
struct Feed {
    ready: bool,
    url: Option<String>,
    titles: Vec<Option<String>>,
    console: Vec<(String, String, String)>, // (source, level, message)
    frames: usize,
}

impl Feed {
    fn absorb(&mut self, msg: FromTab) {
        match msg {
            FromTab::Ready { .. } => self.ready = true,
            FromTab::UrlChanged { url } => self.url = url,
            FromTab::TitleChanged { title } => self.titles.push(title), // Option<String> per contract
            FromTab::ConsoleLog { source, level, message } => {
                self.console.push((source, level, message))
            }
            FromTab::Frame { .. } => self.frames += 1,
            _ => {}
        }
    }

    fn console_has(&self, source: &str, level: &str, msg_pred: impl Fn(&str) -> bool) -> bool {
        self.console
            .iter()
            .any(|(s, l, m)| s == source && l == level && msg_pred(m))
    }

    fn dump(&self) -> String {
        format!(
            "ready={} url={:?}\ntitles: {:?}\nconsole: {:?}\nframes: {}",
            self.ready, self.url, self.titles, self.console, self.frames
        )
    }
}

/// Drain the pump until `pred` holds over the accumulated feed; on timeout
/// the panic also carries the tab's captured stderr (crash/panic forensics).
fn wait_for(
    what: &str,
    pump: &mut Pump,
    feed: &mut Feed,
    stderr: &dyn Fn() -> String,
    pred: impl Fn(&Feed) -> bool,
) {
    let deadline = Instant::now() + deadline();
    loop {
        for (_, msg) in pump.drain() {
            feed.absorb(msg);
        }
        if pred(feed) {
            return;
        }
        if Instant::now() >= deadline {
            panic!(
                "timed out waiting for {what}\n{}\ntab stderr:\n{}",
                feed.dump(),
                stderr()
            );
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn real_tab_runs_stored_userscripts_and_their_removal_is_observable() {
    let Some(tab_bin) = real_tab_bin() else {
        eprintln!("skip: ironsurf-tab not built (cargo build --workspace first)");
        return;
    };
    let port = spawn_local_http();
    let url = format!("http://127.0.0.1:{port}/");

    // ── Two scripts in a real Store (the menu's own persistence layer) ──────
    // Exactly one script per Save round-trip: the id is read back from the
    // Store *after* the previous upsert landed. (Reading `next_userscript_id`
    // twice up-front hands the same id out twice and the second script would
    // silently replace the first — the trap that ate the bump in the first
    // version of this test.)
    let store_dir = tempfile::tempdir().expect("store dir");
    let mut store = Store::open(store_dir.path()).expect("open store");
    let bump = ironsurf_common::Userscript {
        id: store.next_userscript_id(),
        name: "Title bump".into(),
        patterns: vec!["*127.0.0.1".into()],
        code: "document.title = 'Bumped by Title bump';".into(),
        enabled: true,
    };
    store.upsert_userscript(bump.clone()).expect("upsert Title bump");
    let crash = ironsurf_common::Userscript {
        id: store.next_userscript_id(),
        name: "Crash pad".into(),
        patterns: vec!["*127.0.0.1".into()],
        code: "throw new Error('kaput');".into(),
        enabled: true,
    };
    store.upsert_userscript(crash.clone()).expect("upsert Crash pad");

    // ── Spawn the real tab (no start URL: scripts land before the Navigate) ─
    let mut pump = Pump::new();
    let mut proc = TabProc::spawn(
        &tab_bin,
        TabId(12),
        ContainerId(1),
        tempfile::tempdir().expect("profile dir").path(),
        None,
        &[],
        &pump.sender(),
    )
    .expect("spawn real ironsurf-tab");
    let mut feed = Feed::default();
    wait_for("Ready", &mut pump, &mut feed, &|| proc.stderr_tail(), |f| f.ready);

    // The shell ships all ENABLED scripts (its production payload builder).
    proc.send(ToTab::SetUserscripts { scripts: enabled_userscripts(&store.userscripts()) })
        .expect("send SetUserscripts");
    proc.send(ToTab::Navigate { url: url.clone() }).expect("send Navigate");

    // (a) The matching script ran: its document.title write beat the page's.
    wait_for(
        "userscript-bumped title",
        &mut pump,
        &mut feed,
        &|| proc.stderr_tail(),
        |f| {
            f.url.as_deref() == Some(url.as_str())
                && f.titles.iter().any(|t| t.as_deref() == Some("Bumped by Title bump"))
        },
    );

    // (b) The throw names its script — the headline requirement.
    wait_for(
        "userscript error naming 'Crash pad'",
        &mut pump,
        &mut feed,
        &|| proc.stderr_tail(),
        |f| {
            f.console_has("userscript", "error", |m| {
                m.starts_with("Crash pad:") && m.contains("kaput")
            })
        },
    );
    // The page's own console line is untouched: source "js", verbatim.
    assert!(
        feed.console_has("js", "log", |m| m == "page-line"),
        "page console line must keep source \"js\": {}",
        feed.dump()
    );

    // (c) One bad script never kills the tab: alive, and frames keep painting.
    assert!(
        proc.try_wait_status().is_none(),
        "tab must survive the throwing userscript: {}",
        proc.stderr_tail()
    );
    wait_for(
        "frames after the userscript pass",
        &mut pump,
        &mut feed,
        &|| proc.stderr_tail(),
        |f| f.frames >= 2,
    );

    // ── (d) Disable both in the Store: the next reload shows it all gone ────
    store.set_userscript_enabled(bump.id, false).expect("disable Title bump");
    store.set_userscript_enabled(crash.id, false).expect("disable Crash pad");
    let baseline_console = feed.console.len();
    let baseline_titles = feed.titles.len();
    // Enabled-only ⇒ the wire now carries an empty set, which clears the tab.
    let payload = enabled_userscripts(&store.userscripts());
    assert!(payload.is_empty(), "disabled scripts must never be sent: {payload:?}");
    proc.send(ToTab::SetUserscripts { scripts: payload }).expect("send clear");
    proc.send(ToTab::Reload).expect("send Reload");

    wait_for(
        "reload with no userscripts (title back to the page's)",
        &mut pump,
        &mut feed,
        &|| proc.stderr_tail(),
        |f| {
            f.titles[baseline_titles..]
                .iter()
                .any(|t| t.as_deref() == Some("Userscript e2e"))
        },
    );
    // Side effects gone: no userscript-attributed console line after the
    // baseline (the throwing script no longer runs — its error would have
    // doubled otherwise), and the title stayed the page's own.
    assert!(
        !feed.console[baseline_console..]
            .iter()
            .any(|(s, _, _)| s == "userscript"),
        "no userscript may run after the clear: {}",
        feed.dump()
    );
    assert!(
        !feed.titles[baseline_titles..]
            .iter()
            .any(|t| t.as_deref() == Some("Bumped by Title bump")),
        "the disabled script must not re-bump the title: {}",
        feed.dump()
    );

    // Clean shutdown (same path the shell's ✕ uses).
    assert!(proc.try_wait_status().is_none(), "tab alive before Destroy: {}", proc.stderr_tail());
    proc.destroy();
    let status = proc.child_mut().wait().expect("wait for tab exit");
    assert_eq!(status.code(), Some(0), "stderr: {}", proc.stderr_tail());
}
