//! The popup blocker on the REAL engine over real IPC (task: block popups — a
//! website opening new tabs without a user interaction).
//!
//! A tab can never open a window by itself; the shell owns every tab. So the
//! whole feature is visible on the wire, and this drives the shell side by
//! hand:
//!
//! * `window.open` from a load handler — nobody clicked — is **refused**: the
//!   page's own `w === null` reads `true`, one `FromTab::PopupBlocked` (with the
//!   resolved target) arrives, and no `FromTab::OpenLinkInNewTab` ever does. The
//!   tab keeps its document; nothing is loaded anywhere;
//! * the same call from a link's real `click` handler is **allowed**: the click
//!   the shell forwarded *is* the user interaction, and it buys exactly one
//!   `FromTab::OpenLinkInNewTab`;
//! * a user click on `<a target="_blank">` opens a new tab **and does not
//!   navigate this one** — no `UrlChanged` for the href, the title stays home;
//! * a **Ctrl-left click** on a plain link (`⌘-left` on macOS) asks for the same
//!   new tab and does not move this one either — the middle click's gesture, from
//!   the keyboard (`ToTab::Pointer` carries the modifiers for exactly this);
//! * `ToTab::SetPopupPolicy` is the shell's hand on the lever: allow-list the
//!   document's origin and the load-time popup opens with no input at all, and a
//!   policy change reaches the page that is already loaded (no reload);
//! * `javascript:`/`data:` targets are refused however the policy reads, and
//!   their payload never travels to the shell whole.
//!
//! Headless (no `$DISPLAY`, localhost only). Run with:
//! `cargo test -p ironsurf-tab --test popups`

use std::io::{Read, Write};
use std::net::TcpListener;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

use ironsurf_common::{read_frame, write_frame, FromTab, Modifiers, MouseButton, PointerEvent,
                      PointerKind, ToTab};

const DEADLINE: Duration = Duration::from_secs(60);
/// How long the harness waits for a message that must **never** come. Kept
/// short: the absence of a message is proven against everything the tab had
/// already said by the time the expected one arrived, and this only backstops
/// the (already-concluded) ordering.
const SILENCE: Duration = Duration::from_millis(400);

/// Opens a tab the moment it loads, with no user anywhere near it. Reports what
/// its own `window.open` returned, so the page's view of the refusal is part of
/// the assertion.
const PAGE_ONLOAD: &str = r##"<!doctype html>
<html>
  <head><title>onload</title></head>
  <body style="margin:0;background:#ffffff">
    <p>ad page</p>
    <script>
      var w = window.open("/promo?from=load", "_blank");
      document.title = "seen:" + (w === null) + ":" + (w && w.location.href);
    </script>
  </body>
</html>
"##;

/// A big red block that opens a tab from its own `click` handler. This is the
/// case a rule of thumb ("never allow script `window.open`") would get wrong:
/// the user clicked, and the page answering that click may open its window.
const PAGE_CLICK: &str = r##"<!doctype html>
<html>
  <head><title>clickme</title></head>
  <body style="margin:0;background:#ffffff">
    <a id="go" href="#" style="display:block;width:400px;height:80px;background:#ff0000">click me</a>
    <script>
      document.getElementById("go").addEventListener("click", function () {
        window.open("/promo?from=click", "_blank");
      });
    </script>
  </body>
</html>
"##;

/// A plain `<a target="_blank">`: the user's click must open a new tab without
/// moving this one (the engine's `NavigationProvider` knows nothing about
/// `target`, so this is where that would silently go wrong).
const PAGE_BLANK: &str = r##"<!doctype html>
<html>
  <head><title>home</title></head>
  <body style="margin:0;background:#ffffff">
    <a id="ext" href="/next" target="_blank" style="display:block;width:400px;height:80px;background:#00ff00">open elsewhere</a>
  </body>
</html>
"##;

/// A plain `<a href>` with no `target`: an ordinary click moves **this** tab, and
/// the gesture this file cares about — a Ctrl-left click (⌘-left on macOS) —
/// must open the href in a **new** tab instead of here. The document's own click
/// listener logs each click's modifier flags, so the test sees what the page saw
/// (`ctrlKey` / `metaKey`) — proof the chord reached the page's scripts and not
/// only the tab. It logs rather than writes the title because the title is this
/// test's navigation probe.
const PAGE_PLAIN: &str = r##"<!doctype html>
<html>
  <head><title>plain</title></head>
  <body style="margin:0;background:#ffffff">
    <a id="plain" href="/next" style="display:block;width:400px;height:80px;background:#0000ff">plain link</a>
    <script>
      // After the anchor, or getElementById would answer null here.
      document.getElementById("plain").addEventListener("click", function (e) {
        document.title = "mods:" + e.ctrlKey + "/" + e.metaKey;
      });
    </script>
  </body>
</html>
"##;

/// Reaches for targets that are script, or a whole document inside a URL —
/// with the master switch off, so only the scheme rule can stop it. The `data:`
/// document is deliberately huge: its body must never reach the shell whole.
const PAGE_SCRIPT_TARGETS: &str = r##"<!doctype html>
<html>
  <head><title>scripts</title></head>
  <body style="margin:0;background:#ffffff">
    <script>
      var pad = "";
      while (pad.length < 5000) pad += "abcdefghij";
      var data = window.open("data:text/html,<h1>" + pad + "</h1>");
      var via = window.open("javascript:alert(1)");
      document.title = "tried:" + (data === null) + ":" + (via === null);
    </script>
  </body>
</html>
"##;

/// Where a script-less "click a link" click would navigate, if it ever did.
const PAGE_LANDED: &str = "<!doctype html>\n<html><head><title>landed</title></head><body>landed</body></html>\n";

/// The centre of every 400×80 block this file's pages paint at the top of a
/// `margin:0` body.
const BLOCK_CENTRE: (f32, f32) = (200.0, 40.0);

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
            let response = match path.as_str() {
                "/" | "/index.html" => http_response(200, "text/html", PAGE_ONLOAD.as_bytes()),
                "/click" => http_response(200, "text/html", PAGE_CLICK.as_bytes()),
                "/blank" => http_response(200, "text/html", PAGE_BLANK.as_bytes()),
                "/plain" => http_response(200, "text/html", PAGE_PLAIN.as_bytes()),
                "/next" => http_response(200, "text/html", PAGE_LANDED.as_bytes()),
                "/scripts" => http_response(200, "text/html", PAGE_SCRIPT_TARGETS.as_bytes()),
                // The popup target: it exists (a real tab would load it), but
                // nothing here depends on its body.
                "/promo" => {
                    http_response(200, "text/html", b"<title>promo</title>")
                }
                _ => http_response(404, "text/plain", b"no such route"),
            };
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

struct TabChild {
    child: Child,
    stdin: Option<ChildStdin>,
    events: Receiver<FromTab>,
    seen: Vec<FromTab>,
}

impl Drop for TabChild {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl TabChild {
    fn send(&mut self, msg: ToTab) {
        write_frame(self.stdin.as_mut().expect("stdin open"), &msg)
            .expect("write ToTab frame to tab stdin");
    }

    fn click(&mut self, x: f32, y: f32) {
        self.click_with(x, y, Modifiers::NONE);
    }

    /// The same three events with the modifier bits the user was holding — the
    /// shell forwards them verbatim from its `TouchArea`, and this is the only
    /// way the tab can tell a click from a Ctrl-click.
    fn click_with(&mut self, x: f32, y: f32, mods: Modifiers) {
        let at = |kind| ToTab::Pointer(PointerEvent { x, y, kind, button: MouseButton::Left, mods });
        self.send(at(PointerKind::Move));
        self.send(at(PointerKind::Down));
        self.send(at(PointerKind::Up));
    }

    /// Block until any already-queued or future `FromTab` satisfies `pred`.
    fn wait_for(&mut self, what: &str, pred: impl Fn(&FromTab) -> bool) {
        let deadline = Instant::now() + DEADLINE;
        loop {
            if self.seen.iter().any(&pred) {
                return;
            }
            if Instant::now() >= deadline {
                panic!("timed out waiting for {what}; seen {:#?}", self.seen);
            }
            match self.events.recv_timeout(Duration::from_millis(500)) {
                Ok(msg) => self.seen.push(msg),
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    while let Ok(msg) = self.events.try_recv() {
                        self.seen.push(msg);
                    }
                    if self.seen.iter().any(&pred) {
                        return;
                    }
                    panic!("stdout closed while waiting for {what}; seen {:#?}", self.seen);
                }
            }
        }
    }

    /// Everything received so far, plus whatever a short quiet period still has
    /// to deliver — the "and nothing else happened" half of an assertion.
    fn drain(&mut self) {
        let quiet = Instant::now() + SILENCE;
        while Instant::now() < quiet {
            match self.events.recv_timeout(Duration::from_millis(50)) {
                Ok(msg) => self.seen.push(msg),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
    }

    fn titles(&self) -> Vec<String> {
        self.seen
            .iter()
            .filter_map(|msg| match msg {
                FromTab::TitleChanged { title } => title.clone(),
                _ => None,
            })
            .collect()
    }

    fn opened(&self) -> Vec<String> {
        self.seen
            .iter()
            .filter_map(|msg| match msg {
                FromTab::OpenLinkInNewTab { url } => Some(url.clone()),
                _ => None,
            })
            .collect()
    }

    fn blocked(&self) -> Vec<String> {
        self.seen
            .iter()
            .filter_map(|msg| match msg {
                FromTab::PopupBlocked { url } => Some(url.clone()),
                _ => None,
            })
            .collect()
    }
}

/// Spawn a tab. `url = None` starts it on the shell's default page, which is
/// what the policy tests use so they can install a policy *before* navigating
/// to the page that asks for a window.
fn spawn_tab(url: Option<&str>, profile_dir: &std::path::Path) -> TabChild {
    let exe = env!("CARGO_BIN_EXE_ironsurf");
    let mut command = Command::new(exe);
    command
        .env_remove("DISPLAY")
        .env_remove("WAYLAND_DISPLAY")
        .arg("--tab-id")
        .arg("42")
        .arg("--container-id")
        .arg("1")
        .arg("--profile-dir")
        .arg(profile_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(std::fs::File::create(profile_dir.join("tab-stderr.log")).expect("stderr log"));
    if let Some(url) = url {
        command.arg("--url").arg(url);
    }
    let mut child = command.spawn().expect("spawn ironsurf-tab");

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

    TabChild { child, stdin: Some(stdin), events, seen: Vec::new() }
}

/// The case the task is about: an ad page opens a tab while it loads, and nobody
/// asked. The page is answered `null`, the tab reports the refusal with the
/// target it refused, and nothing is opened.
#[test]
fn a_window_open_on_load_is_blocked_and_reported() {
    let port = spawn_local_http();
    let profile = tempfile::tempdir().expect("profile dir");
    let url = format!("http://127.0.0.1:{port}/");
    let mut tab = spawn_tab(Some(&url), profile.path());

    // The page ran and saw the refusal with its own eyes.
    tab.wait_for("the page's own `w === null`", |msg| {
        matches!(msg, FromTab::TitleChanged { title: Some(t) } if t.starts_with("seen:true"))
    });
    // …and the shell is told, with the resolved target, so it can show the user
    // what was refused and offer to allow the site.
    tab.wait_for("PopupBlocked(/promo?from=load)", |msg| {
        matches!(msg, FromTab::PopupBlocked { url } if url.ends_with("/promo?from=load"))
    });
    tab.drain();
    assert!(
        tab.opened().is_empty(),
        "a refused popup must never reach the shell as a request to open: {:?}",
        tab.opened()
    );
    // The notice's URL is the document-relative target, not a bare path.
    let reported = tab.blocked()[0].clone();
    assert!(
        reported.starts_with("http://127.0.0.1:") && reported.ends_with("/promo?from=load"),
        "the refused URL is reported resolved, got {reported}"
    );
    // And this tab never left its page: no `UrlChanged` off `/`, no promo title.
    assert!(
        !tab.titles().iter().any(|title| title == "promo" || title == "landed"),
        "a blocked popup changed nothing on screen: {:?}",
        tab.titles()
    );
}

/// The other half of the rule: the user clicked, and the window that click
/// asked for is allowed. Exactly one — a second `window.open` from the same
/// gesture would be the page deciding for the user.
#[test]
fn a_window_open_from_a_real_click_is_allowed() {
    let port = spawn_local_http();
    let profile = tempfile::tempdir().expect("profile dir");
    let url = format!("http://127.0.0.1:{port}/click");
    let mut tab = spawn_tab(Some(&url), profile.path());

    // Make sure the page is up before clicking, and that no popup was requested
    // before the click (the handler must not have fired on load).
    tab.wait_for("the page's title", |msg| {
        matches!(msg, FromTab::TitleChanged { title: Some(t) } if t == "clickme")
    });
    tab.drain();
    assert!(tab.opened().is_empty(), "no click yet: {:?}", tab.opened());

    let (x, y) = BLOCK_CENTRE;
    tab.click(x, y);

    tab.wait_for("OpenLinkInNewTab(/promo?from=click)", |msg| {
        matches!(msg, FromTab::OpenLinkInNewTab { url } if url.ends_with("/promo?from=click"))
    });
    tab.drain();
    // The page got a window object back, not a refusal: `window.open` answers
    // `null` exactly when the gate refuses, and a refusal is always reported —
    // so "one request to open, nothing refused" *is* the page's view.
    assert!(
        tab.blocked().is_empty(),
        "a click-authorized popup is not blocked: {:?}",
        tab.blocked()
    );
    assert_eq!(
        tab.opened().len(),
        1,
        "one click, one window — and the click itself asked for nothing else \
         (`href=\"#\"` is not a navigation), so a rule that opened a tab per \
         click would double up: {:?}",
        tab.opened()
    );
}

/// A user clicking `<a target="_blank">` is the user opening a tab. The engine
/// reports that click as a navigation of this tab (it knows nothing about
/// `target`), and the tab must turn it into a new-tab request instead of
/// following it.
#[test]
fn a_target_blank_click_opens_a_tab_without_navigating_this_one() {
    let port = spawn_local_http();
    let profile = tempfile::tempdir().expect("profile dir");
    let url = format!("http://127.0.0.1:{port}/blank");
    let mut tab = spawn_tab(Some(&url), profile.path());

    tab.wait_for("the page's title", |msg| {
        matches!(msg, FromTab::TitleChanged { title: Some(t) } if t == "home")
    });
    tab.drain();
    assert!(tab.opened().is_empty());

    let (x, y) = BLOCK_CENTRE;
    tab.click(x, y);

    tab.wait_for("OpenLinkInNewTab(/next) from the target=_blank click", |msg| {
        matches!(msg, FromTab::OpenLinkInNewTab { url } if url.ends_with("/next"))
    });
    tab.drain();
    // …and this tab stayed on `home`: the href was never navigated.
    assert!(
        !tab.seen.iter().any(|msg| matches!(
            msg,
            FromTab::UrlChanged { url: Some(u) } if u.ends_with("/next")
        )),
        "a `target=_blank` click must not navigate the tab the user is on"
    );
    assert!(
        !tab.titles().iter().any(|title| title == "landed"),
        "the clicked page must not be loaded here: {:?}",
        tab.titles()
    );
    assert!(
        !tab.seen.iter().any(|msg| matches!(msg, FromTab::PopupBlocked { .. })),
        "the user clicked the link: that is the interaction the blocker waits for"
    );
}

/// The keyboard's version of the middle click: **Ctrl-left** on a plain link
/// (`⌘-left` on macOS) opens it in a new tab and leaves this tab on its page.
/// The same link clicked plain still navigates, which is what makes the new tab
/// above the modifier's doing and not "links never navigate here".
#[test]
fn a_ctrl_click_on_a_link_opens_a_new_tab_and_stays_put() {
    let port = spawn_local_http();
    let profile = tempfile::tempdir().expect("profile dir");
    let url = format!("http://127.0.0.1:{port}/plain");
    let mut tab = spawn_tab(Some(&url), profile.path());

    tab.wait_for("the page's title", |msg| {
        matches!(msg, FromTab::TitleChanged { title: Some(t) } if t == "plain")
    });
    tab.drain();
    assert!(tab.opened().is_empty(), "no click yet: {:?}", tab.opened());

    let (x, y) = BLOCK_CENTRE;
    tab.click_with(x, y, Modifiers::NONE.with_ctrl());

    tab.wait_for("OpenLinkInNewTab(/next) from the Ctrl-click", |msg| {
        matches!(msg, FromTab::OpenLinkInNewTab { url } if url.ends_with("/next"))
    });
    tab.drain();
    assert_eq!(tab.opened().len(), 1, "one Ctrl-click, one new tab: {:?}", tab.opened());
    assert!(
        !tab.seen.iter().any(|msg| matches!(
            msg,
            FromTab::UrlChanged { url: Some(u) } if u.ends_with("/next")
        )),
        "a Ctrl-click must not navigate the tab the user is on"
    );
    assert!(
        !tab.titles().iter().any(|title| title == "landed"),
        "the target must not load here: {:?}",
        tab.titles()
    );

    // The page saw the modifier too — the gesture reached its scripts with the
    // flags set, which is how a page gets to cancel it (`e.preventDefault()`).
    // It says so in its own title, which only arrives because the chord path
    // flushes the page's writes like every other JS turn does.
    tab.wait_for("the page's own `ctrlKey === true`", |msg| {
        matches!(msg, FromTab::TitleChanged { title: Some(t) } if t == "mods:true/false")
    });

    // The same modifier over what is not a link stays an ordinary click: still no
    // new tab (the y below the link is the page's own background).
    tab.click_with(x, y + 200.0, Modifiers::NONE.with_ctrl());
    tab.drain();
    assert_eq!(tab.opened().len(), 1, "a Ctrl-click on nothing opens nothing: {:?}", tab.opened());

    // The control case: the very same link, clicked with nothing held, does move
    // this tab.
    tab.click(x, y);
    tab.wait_for("the plain click navigating to /next", |msg| {
        matches!(msg, FromTab::UrlChanged { url: Some(u) } if u.ends_with("/next"))
    });
    tab.drain();
    assert!(
        !tab.seen.iter().any(|msg| matches!(msg, FromTab::PopupBlocked { .. })),
        "nothing in this test is a popup: {:?}",
        tab.blocked()
    );
}

/// The shell's lever (task: the user can allow a site): allow-list the
/// document's origin and the same load-time `window.open` — with no input at all
/// — becomes a request to open. `SetPopupPolicy` is sent before the load, which
/// is what the shell does at tab spawn.
#[test]
fn an_allowed_origin_may_open_whenever_it_likes() {
    let port = spawn_local_http();
    let profile = tempfile::tempdir().expect("profile dir");
    let url = format!("http://127.0.0.1:{port}/");
    let origin = format!("http://127.0.0.1:{port}");
    let mut tab = spawn_tab(None, profile.path());

    tab.send(ToTab::SetPopupPolicy { block: true, allowed: vec![origin] });
    tab.send(ToTab::Resize { width: 1024, height: 768, scale: 1.0 });
    tab.send(ToTab::Navigate { url });

    tab.wait_for("OpenLinkInNewTab(/promo?from=load) with no input", |msg| {
        matches!(msg, FromTab::OpenLinkInNewTab { url } if url.ends_with("/promo?from=load"))
    });
    tab.drain();
    assert!(tab.blocked().is_empty(), "the allow-list covers this page: {:?}", tab.blocked());
    // The page still got a window object back, not a refusal.
    tab.wait_for("the page's own `w !== null`", |msg| {
        matches!(msg, FromTab::TitleChanged { title: Some(t) } if t.starts_with("seen:false:"))
    });
}

/// The lever has the other direction too: the master switch off opens the door
/// for everyone, and — the part that matters for a user sitting on a page —
/// a policy change reaches the document that is **already loaded**, with no
/// reload in sight.
#[test]
fn switching_blocking_off_reaches_the_page_already_loaded() {
    let port = spawn_local_http();
    let profile = tempfile::tempdir().expect("profile dir");
    let url = format!("http://127.0.0.1:{port}/");
    let mut tab = spawn_tab(Some(&url), profile.path());

    // First: blocked, as ever.
    tab.wait_for("the first refusal", |msg| {
        matches!(msg, FromTab::PopupBlocked { url } if url.ends_with("/promo?from=load"))
    });
    assert!(tab.opened().is_empty());

    // Then: switch blocking off and let the page try again by reloading itself
    // through the shell. The policy applied without a reload; this reload is
    // only how the page gets to call `window.open` a second time.
    tab.send(ToTab::SetPopupPolicy { block: false, allowed: Vec::new() });
    tab.send(ToTab::Reload);
    tab.wait_for("the allowed request after the switch", |msg| {
        matches!(msg, FromTab::OpenLinkInNewTab { url } if url.ends_with("/promo?from=load"))
    });
    tab.drain();
    assert!(
        tab.blocked().is_empty() || tab.blocked().len() == 1,
        "the second load must not be refused: {:?}",
        tab.blocked()
    );
}

/// A `javascript:`/`data:` target is not a page. Whatever the policy says, it is
/// refused — and the megabytes of a `data:` document never reach the shell
/// whole, so the notice stays one line.
#[test]
fn a_script_carrying_target_is_refused_and_capped() {
    let port = spawn_local_http();
    let profile = tempfile::tempdir().expect("profile dir");
    // A page that reaches for a `data:` document and a `javascript:` target,
    // with the master switch OFF, so only the scheme rule can stop it.
    let url = format!("http://127.0.0.1:{port}/scripts");
    let mut tab = spawn_tab(None, profile.path());
    tab.send(ToTab::SetPopupPolicy { block: false, allowed: Vec::new() });
    tab.send(ToTab::Resize { width: 1024, height: 768, scale: 1.0 });
    tab.send(ToTab::Navigate { url });

    tab.wait_for("the huge data: popup refused", |msg| {
        matches!(msg, FromTab::PopupBlocked { url } if url.starts_with("data:text/html"))
    });
    tab.wait_for("the javascript: popup refused", |msg| {
        matches!(msg, FromTab::PopupBlocked { url } if url.starts_with("javascript:"))
    });
    tab.drain();
    assert!(tab.opened().is_empty(), "a refused target never opens: {:?}", tab.opened());
    let huge = tab
        .blocked()
        .into_iter()
        .find(|url| url.starts_with("data:"))
        .expect("the data: refusal");
    assert!(
        huge.len() <= ironsurf_common::MAX_POPUP_URL_CHARS + '…'.len_utf8(),
        "a megabyte `data:` target is capped for the notice, got {} bytes",
        huge.len()
    );
    assert!(huge.ends_with('…'), "and the cut is marked");
}
