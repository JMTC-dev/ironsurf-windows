//! End-to-end test of the **private-address rule** (AUDIT finding 5; the rule
//! that answers the PoC `audit_poc_nav::f4`).
//!
//! f4 proves a page script can walk the *tab* into `http://127.0.0.1:9/` — a
//! closed port on the loopback interface. This file tests the rule that refuses
//! it against a server that IS up: two HTTP servers on two loopback ports, so
//! "private address" and "same origin" are both real and the escape hatch is
//! demonstrated rather than asserted.
//!
//! 1. a page on a name `fetch`es a private address → the script never sees the
//!    body, and a DevTools console **warning** names both ends;
//! 2. a page's `location.href` points at a private address → the tab does not
//!    navigate, and the refusal is on the console (and in the status bar);
//! 3. the **user** opens the private address → it loads, silently;
//! 4. a page that lives on the private address fetches **its own origin** → the
//!    body arrives, silently.
//!
//! Run with: `cargo test -p ironsurf-tab --test private_addr`

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use ironsurf_common::{read_frame, write_frame, FromTab, ToTab};

const BIN: &str = env!("CARGO_BIN_EXE_ironsurf-tab");
const COLLECT: Duration = Duration::from_millis(1_800);

/// A name, not an address, so a page served from it is not itself on a private
/// address (the rule decides DNS names by the `.localhost` exception only).
fn host_str() -> &'static str {
    option_env!("IRONSURF_TEST_HOST").unwrap_or("lvh.me")
}

/// A canned-response HTTP server on one loopback port. Routes are added after
/// `bind`, because a page under test needs to name its own port.
struct Server {
    port: u16,
    routes: Arc<Mutex<Vec<(String, String)>>>,
    hits: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    joiner: Option<thread::JoinHandle<()>>,
}

impl Server {
    fn spawn() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let port = listener.local_addr().expect("local_addr").port();
        listener.set_nonblocking(true).expect("nonblocking");
        let routes: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(Vec::new()));
        let hits = Arc::new(AtomicUsize::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let (flag, counted) = (Arc::clone(&stop), Arc::clone(&hits));
        let served = Arc::clone(&routes);
        let joiner = thread::spawn(move || {
            while !flag.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let _ = stream.set_read_timeout(Some(Duration::from_millis(500)));
                        let _ = stream.set_write_timeout(Some(Duration::from_millis(500)));
                        let mut buf = [0u8; 8192];
                        let n = stream.read(&mut buf).unwrap_or(0);
                        let head = String::from_utf8_lossy(&buf[..n]).into_owned();
                        let path = head
                            .split_whitespace()
                            .nth(1)
                            .unwrap_or("/")
                            .split('?')
                            .next()
                            .unwrap_or("/")
                            .to_owned();
                        counted.fetch_add(1, Ordering::Relaxed);
                        let body = served
                            .lock()
                            .expect("routes mutex")
                            .iter()
                            .find(|(at, _)| *at == path)
                            .map(|(_, body)| body.clone())
                            .unwrap_or_else(|| "not found".to_owned());
                        let ctype = if path.ends_with(".html") {
                            "text/html; charset=utf-8"
                        } else {
                            "text/plain; charset=utf-8"
                        };
                        let response = format!(
                            "HTTP/1.0 200 OK\r\nContent-Type: {ctype}\r\n\
                             Access-Control-Allow-Origin: *\r\nContent-Length: {}\r\n\r\n{body}",
                            body.len()
                        );
                        let _ = stream.write_all(response.as_bytes());
                        let _ = stream.flush();
                    }
                    Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
        });
        Server {
            port,
            routes,
            hits,
            stop,
            joiner: Some(joiner),
        }
    }

    fn route(&self, path: &str, body: &str) {
        self.routes
            .lock()
            .expect("routes mutex")
            .push((path.to_owned(), body.to_owned()));
    }

    /// The URL of `path` on this server — a private address by construction.
    fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{path}", self.port)
    }

    /// The URL of `path` on this server, spelled with a NAME instead of the
    /// address, so it is not a private address.
    fn named_url(&self, path: &str) -> String {
        format!("http://{}:{}{path}", host_str(), self.port)
    }

    fn take_hits(&self) -> usize {
        self.hits.swap(0, Ordering::Relaxed)
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(joiner) = self.joiner.take() {
            let _ = joiner.join();
        }
    }
}

static PROFILE_SEQ: AtomicUsize = AtomicUsize::new(0);

struct Tab {
    child: Child,
    stdin: std::process::ChildStdin,
    events: mpsc::Receiver<FromTab>,
    seen: Vec<FromTab>,
    profile: PathBuf,
}

impl Tab {
    fn open(url: &str) -> Self {
        let profile = std::env::temp_dir().join(format!(
            "ironsurf-private-addr-{}-{}",
            std::process::id(),
            PROFILE_SEQ.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&profile).expect("profile dir");
        let mut child = Command::new(BIN)
            .env_remove("DISPLAY")
            .env_remove("WAYLAND_DISPLAY")
            .arg("--tab-id")
            .arg("1")
            .arg("--container-id")
            .arg("1")
            .arg("--profile-dir")
            .arg(&profile)
            .arg("--url")
            .arg(url)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(std::fs::File::create(profile.join("tab-stderr.log")).expect("stderr log"))
            .spawn()
            .expect("spawn tab");
        let stdin = child.stdin.take().expect("stdin");
        let stdout = child.stdout.take().expect("stdout");
        let (tx, events) = mpsc::channel();
        thread::spawn(move || {
            let mut reader = std::io::BufReader::new(stdout);
            while let Ok(msg) = read_frame::<_, FromTab>(&mut reader) {
                if tx.send(msg).is_err() {
                    break;
                }
            }
        });
        let mut tab = Tab {
            child,
            stdin,
            events,
            seen: Vec::new(),
            profile,
        };
        tab.send(ToTab::Resize {
            width: 800,
            height: 600,
            scale: 1.0,
        });
        tab
    }

    fn send(&mut self, msg: ToTab) {
        write_frame(&mut self.stdin, &msg).expect("write ToTab frame");
        self.stdin.flush().expect("flush stdin");
    }

    fn collect(&mut self, for_how_long: Duration) {
        let deadline = Instant::now() + for_how_long;
        while Instant::now() < deadline {
            match self.events.recv_timeout(Duration::from_millis(100)) {
                Ok(msg) => self.seen.push(msg),
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
    }

    fn matching(&self, pred: impl Fn(&FromTab) -> bool) -> Vec<&FromTab> {
        self.seen.iter().filter(|msg| pred(msg)).collect()
    }

    fn title(&self) -> String {
        self.seen
            .iter()
            .rev()
            .find_map(|msg| match msg {
                FromTab::TitleChanged { title, .. } => Some(title.clone().unwrap_or_default()),
                _ => None,
            })
            .unwrap_or_default()
    }

    fn warnings(&self) -> Vec<String> {
        self.seen
            .iter()
            .filter_map(|msg| match msg {
                FromTab::ConsoleLog {
                    source,
                    level,
                    message,
                    ..
                } if level == "warn" => Some(format!("{source}: {message}")),
                _ => None,
            })
            .collect()
    }
}

impl Drop for Tab {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.profile);
    }
}

/// A page that reads a URL with `fetch` and puts the outcome in the title: if
/// the title says `read:`, the page's script got the body.
fn fetch_page(target: &str) -> String {
    format!(
        "<!doctype html><html><head><title>wait</title></head>\
         <body style=\"margin:0;background:#ffffff\"><p>fetching</p><script>\n\
           fetch(\"{target}\").then(function (r) {{ return r.text(); }})\n\
             .then(function (t) {{ document.title = \"read:\" + t; }})\n\
             .catch(function (e) {{ document.title = \"rejected\"; }});\n\
         </script></body></html>"
    )
}

/// FINDING 5, end to end: a page on a name asks `fetch` for a body that lives
/// on a private address. The script never sees the bytes, and the tab says why
/// on the DevTools console instead of the leak loading silently.
#[test]
fn a_web_pages_fetch_of_a_private_address_is_blocked_with_a_warning() {
    let lan = Server::spawn();
    lan.route("/api/lan", "LAN reached");
    let web = Server::spawn();
    web.route("/web/fetch.html", &fetch_page(&lan.url("/api/lan")));

    let mut tab = Tab::open(&web.named_url("/web/fetch.html"));
    tab.collect(COLLECT);

    assert!(
        !tab.title().contains("LAN reached"),
        "a page on the web read a private address into JavaScript: title {:?}",
        tab.title()
    );
    let warnings = tab.warnings();
    assert!(
        !warnings.is_empty(),
        "the refusal printed no console warning; seen {} messages, warnings {:?}",
        tab.seen.len(),
        warnings
    );
    let message = warnings.join(" | ");
    assert!(
        message.contains("private address"),
        "the warning must name the rule: {message:?}"
    );
    assert!(
        message.contains(&lan.url("/api/lan")),
        "the warning must name the refused URL ({}): {message:?}",
        lan.url("/api/lan")
    );
    assert!(
        message.contains(&format!("{}:{}", host_str(), web.port)),
        "the warning must say which page asked ({}:{}): {message:?}",
        host_str(),
        web.port
    );
}

/// FINDING 5 in the PoC's own shape: the page drives the TAB to a private
/// address with `location.href`. The tab stays where the user left it, and both
/// the console and the status bar say why.
#[test]
fn a_web_pages_navigation_to_a_private_address_is_blocked_with_a_warning() {
    let lan = Server::spawn();
    lan.route("/api/lan", "LAN reached");
    let web = Server::spawn();
    web.route(
        "/web/jump.html",
        &format!(
            "<!doctype html><html><head><title>stayed</title></head>\
             <body style=\"margin:0;background:#ffffff\"><p>jumping</p><script>\n\
               location.href = \"{}\";\n\
             </script></body></html>",
            lan.url("/api/lan")
        ),
    );

    let mut tab = Tab::open(&web.named_url("/web/jump.html"));
    tab.collect(COLLECT);

    assert!(
        tab.matching(|msg| {
            matches!(msg, FromTab::UrlChanged { url: Some(seen) } if seen == &lan.url("/api/lan"))
        })
            .is_empty(),
        "a page's location.href walked the tab to {} (finding 5, f4 shape)",
        lan.url("/api/lan")
    );
    assert!(
        !tab.warnings().is_empty(),
        "no console warning for the refused navigation"
    );
    assert!(
        !tab.matching(|msg| matches!(msg, FromTab::StatusText { text: Some(text) } if text.contains("private address")))
            .is_empty(),
        "the status bar never reported the refusal to the user"
    );
    // Nothing was served from the private end for the refused navigation: the
    // rule answers before the socket opens, so the leak never happens at all.
    assert_eq!(
        lan.take_hits(),
        0,
        "the refused navigation still asked the private server for bytes"
    );
}

/// The browser's own UI still reaches a private address — this is how the user's
/// address bar and the tab's own `ironsurf://archive` page work.
#[test]
fn the_user_may_still_open_a_private_address() {
    let lan = Server::spawn();
    lan.route("/api/lan", "LAN reached");
    let url = lan.url("/api/lan");

    let mut tab = Tab::open(&url);
    tab.collect(COLLECT);

    assert!(
        !tab
            .matching(|msg| matches!(msg, FromTab::UrlChanged { url: Some(seen) } if seen == &url))
            .is_empty(),
        "an address-bar navigation to {url} never loaded"
    );
    assert!(
        tab.warnings().is_empty(),
        "a user navigation to a private address should be silent: {:?}",
        tab.warnings()
    );
    assert!(lan.take_hits() >= 1, "the document was never fetched");
}

/// The same-origin escape hatch, on the wire: a page that IS on the private
/// address fetches its own origin and gets its body back, with no warning.
#[test]
fn a_local_page_may_still_fetch_its_own_origin() {
    let lan = Server::spawn();
    lan.route("/api/own", "own origin");
    // Fetched by ABSOLUTE url, so the same-origin escape hatch is what is
    // being exercised (not a relative URL that could not be anything else).
    lan.route("/local/fetch.html", &fetch_page(&lan.url("/api/own")));
    let mut tab = Tab::open(&lan.url("/local/fetch.html"));
    tab.collect(COLLECT);

    assert_eq!(
        tab.title(),
        "read:own origin",
        "a local page could not fetch its own origin (warnings {:?})",
        tab.warnings()
    );
    assert!(
        tab.warnings().is_empty(),
        "a same-origin fetch on the local network should not warn: {:?}",
        tab.warnings()
    );
}
