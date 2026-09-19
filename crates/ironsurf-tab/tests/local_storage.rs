//! `window.localStorage` on the REAL tab, per origin (task: per-origin DOM
//! storage). Headless (no `$DISPLAY`, localhost only): a tiny HTTP fixture
//! answers on TWO loopback ports — two distinct origins over one set of
//! pages — and the tab is spawned with a temp `--profile-dir`. Asserts:
//!
//! * a page's `localStorage.setItem` lands in the container profile under
//!   `local-storage/` as ONE file per origin (the file name never contains
//!   the host in the clear),
//! * a second document on the SAME origin (fresh realm, any path) reads the
//!   stored value back,
//! * a document on the OTHER port (different origin, same host, same
//!   container) reads `null` — the area never crosses the origin boundary,
//! * after a tab RESTART (fresh process, same profile dir) the value is
//!   still there,
//! * a `file://` document (opaque origin, `SecurityError`) cannot touch the
//!   store — the JS error is reported and the store stays untouched.
//!
//! Run with: `cargo test -p ironsurf-tab --test local_storage`

use std::io::{Read, Write};
use std::net::TcpListener;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

use ironsurf_common::{read_frame, write_frame, FromTab, ToTab};

const DEADLINE: Duration = Duration::from_secs(60);

/// Stores a sentinel, then reports the area's length through the title.
const PAGE_SET: &str = r#"<!doctype html>
<html>
  <head><title>set page</title></head>
  <body style="margin:0;background:#ffffff">
    <script>
      try {
        localStorage.setItem("kept", "survivor");
        document.title = "set:" + localStorage.length;
      } catch (e) {
        document.title = "set-error:" + e.name;
      }
    </script>
  </body>
</html>
"#;

/// Reads the sentinel back through the title (`read:null` when the area is
/// empty — which is exactly what a foreign origin must see).
const PAGE_READ: &str = r#"<!doctype html>
<html>
  <head><title>read page</title></head>
  <body style="margin:0;background:#ffffff">
    <script>
      try {
        document.title = "read:" + localStorage.getItem("kept");
      } catch (e) {
        document.title = "read-error:" + e.name;
      }
    </script>
  </body>
</html>
"#;

/// Serves both pages on every request (the fixture is path-keyed, port-
/// agnostic — the second listener exists precisely to be a second origin).
fn serve(path: &str) -> &'static str {
    match path {
        "/set.html" => PAGE_SET,
        _ => PAGE_READ,
    }
}

/// Minimal HTTP/1.0 fixture on 127.0.0.1 (one request per connection);
/// returns the bound port. Kept alive for the whole test process.
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
            let head = String::from_utf8_lossy(&buf).to_string();
            let path = head
                .split_whitespace()
                .nth(1)
                .unwrap_or("/")
                .split('?')
                .next()
                .unwrap_or("/")
                .to_string();
            let body = serve(&path).to_string();
            let response = format!(
                "HTTP/1.0 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
        }
    });
    port
}

struct TabChild {
    child: Child,
    #[allow(dead_code)]
    stdin: ChildStdin,
    events: Receiver<FromTab>,
    seen: Vec<FromTab>,
}

impl TabChild {
    fn send(&mut self, msg: ToTab) {
        write_frame(&mut self.stdin, &msg).expect("write ToTab frame to tab stdin");
    }

    fn pump(&mut self) -> bool {
        match self.events.recv_timeout(Duration::from_millis(500)) {
            Ok(msg) => {
                self.seen.push(msg);
                true
            }
            Err(mpsc::RecvTimeoutError::Timeout) => false,
            Err(mpsc::RecvTimeoutError::Disconnected) => false,
        }
    }

    /// Every `TitleChanged` observed so far, newest last.
    fn titles(&self) -> Vec<String> {
        self.seen
            .iter()
            .filter_map(|msg| match msg {
                FromTab::TitleChanged { title: Some(t) } => Some(t.clone()),
                _ => None,
            })
            .collect()
    }

    /// Pump until `count` titles contain `needle` (later navigations re-enter
    /// the same titles, so plain "seen at least once" is not enough).
    fn wait_titles(&mut self, needle: &str, count: usize) {
        let deadline = Instant::now() + DEADLINE;
        loop {
            let have = self
                .titles()
                .iter()
                .filter(|t| t.contains(needle))
                .count();
            if have >= count {
                return;
            }
            if Instant::now() >= deadline {
                let status = self.child.try_wait().ok().flatten();
                panic!(
                    "timed out needing {count}× title containing {needle:?} \
                     (have {have}; child status: {status:?})\nseen titles: {:?}",
                    self.titles()
                );
            }
            self.pump();
        }
    }
}

fn spawn_tab(url: &str, profile_dir: &std::path::Path) -> TabChild {
    let exe = env!("CARGO_BIN_EXE_ironsurf-tab");
    let mut child = Command::new(exe)
        .env_remove("DISPLAY")
        .env_remove("WAYLAND_DISPLAY")
        .arg("--tab-id")
        .arg("71")
        .arg("--container-id")
        .arg("1")
        .arg("--profile-dir")
        .arg(profile_dir)
        .arg("--url")
        .arg(url)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(std::fs::File::create(profile_dir.join(format!("tab-stderr-{}.log", std::process::id()))).expect("stderr log"))
        .spawn()
        .expect("spawn ironsurf-tab");

    let stdout = child.stdout.take().expect("child stdout");
    let stdin = child.stdin.take().expect("child stdin");
    let (tx, events) = mpsc::channel();
    thread::spawn(move || {
        let mut reader = std::io::BufReader::new(stdout);
        while let Ok(msg) = read_frame::<_, FromTab>(&mut reader) {
            if tx.send(msg).is_err() {
                break;
            }
        }
    });

    TabChild {
        child,
        stdin,
        events,
        seen: Vec::new(),
    }
}

/// List every `local-storage` area file's name + body (post-test inspection).
fn area_files(profile_dir: &std::path::Path) -> Vec<(String, String)> {
    let dir = profile_dir.join("local-storage");
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    entries
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            if !name.ends_with(".json") {
                return None;
            }
            let body = std::fs::read_to_string(e.path()).ok()?;
            Some((name, body))
        })
        .collect()
}

#[test]
fn local_storage_is_per_origin_persistent_and_dies_on_opaque_documents() {
    let port_a = spawn_local_http();
    let port_b = spawn_local_http();
    assert_ne!(port_a, port_b, "two listeners = two distinct origins");

    let dir = tempfile::tempdir().expect("tempdir");
    let profile = dir.path();

    // ── Tab 1: origin A stores a sentinel; same origin reads it back ──────
    let set_url = format!("http://127.0.0.1:{port_a}/set.html");
    let mut tab = spawn_tab(&set_url, profile);
    tab.wait_titles("set:1", 1);

    let read_same_origin = format!("http://127.0.0.1:{port_a}/read.html");
    tab.send(ToTab::Navigate {
        url: read_same_origin.clone(),
    });
    tab.wait_titles("read:survivor", 1);

    // ── Origin B (same host, other port) sees NOTHING of A's area ─────────
    let read_other_origin = format!("http://127.0.0.1:{port_b}/read.html");
    tab.send(ToTab::Navigate {
        url: read_other_origin,
    });
    tab.wait_titles("read:null", 1);

    // ── On-disk shape: one area file, hex name, sentinel inside ───────────
    // (the foreign-origin read above proved runtime isolation; the file
    // count proves one-area-per-origin persistence, and B never wrote).
    let files = area_files(profile);
    assert_eq!(files.len(), 1, "exactly one origin area on disk: {files:?}");
    let (name, body) = &files[0];
    assert!(
        !name.contains("127.0.0.1") && !name.contains("localhost"),
        "area file name must not carry the host in the clear: {name}"
    );
    assert!(body.contains("survivor"), "sentinel persisted: {body}");

    drop(tab);

    // ── Tab 2 (fresh process, same profile): durable across restarts ──────
    let mut tab2 = spawn_tab(&read_same_origin, profile);
    tab2.wait_titles("read:survivor", 1);
    let foreign = format!("http://127.0.0.1:{port_b}/read.html");
    tab2.send(ToTab::Navigate { url: foreign });
    tab2.wait_titles("read:null", 1);
    drop(tab2);
}

/// A `file://` document has no storage origin (opaque, per the security
/// rules): its script gets a `SecurityError` out of every localStorage
/// access — observable as the page's `read-error:SecurityError` title — and
/// the tab stays alive. A SECOND container profile additionally starts
/// empty: areas live per container, which the sandbox makes unbreakable.
#[test]
fn file_page_gets_security_error_and_other_container_sees_nothing() {
    let port = spawn_local_http();
    let dir = tempfile::tempdir().expect("tempdir");
    let profile_a = dir.path().join("container-a");
    std::fs::create_dir_all(&profile_a).expect("profile a");
    let start = format!("http://127.0.0.1:{port}/set.html");
    let mut tab = spawn_tab(&start, &profile_a);
    tab.wait_titles("set:1", 1);

    // The same page over file:// — opaque origin, every access must throw.
    // (The page must live INSIDE the container profile: that's all the
    // sandbox lets the tab read — see ironsurf-sandbox.)
    let file_page = profile_a.join("read-local.html");
    std::fs::write(&file_page, PAGE_READ).expect("write page");
    let file_url = url::Url::from_file_path(&file_page)
        .expect("file url")
        .to_string();
    tab.send(ToTab::Navigate { url: file_url });
    tab.wait_titles("read-error:SecurityError", 1);
    // …and the tab is still alive on top of it: the next load works.
    let read_http = format!("http://127.0.0.1:{port}/read.html");
    tab.send(ToTab::Navigate { url: read_http });
    tab.wait_titles("read:survivor", 1);
    drop(tab);

    // A fresh container profile (container B): container A's areas are not
    // there, and B's own writes land inside B only.
    let profile_b = dir.path().join("container-b");
    std::fs::create_dir_all(&profile_b).expect("profile b");
    assert_eq!(area_files(&profile_a).len(), 1, "A owns exactly one area");
    assert!(area_files(&profile_b).is_empty(), "B starts empty");
    let mut tab_b = spawn_tab(&start, &profile_b);
    tab_b.wait_titles("set:1", 1);
    let own_read = format!("http://127.0.0.1:{port}/read.html");
    tab_b.send(ToTab::Navigate { url: own_read });
    tab_b.wait_titles("read:survivor", 1);
    drop(tab_b);
    assert_eq!(area_files(&profile_b).len(), 1, "B keeps its areas in B");
    assert_eq!(
        area_files(&profile_a).len(),
        1,
        "B's writes never touched A's directory"
    );
}
