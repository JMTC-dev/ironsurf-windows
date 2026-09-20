//! `fetch`/`Request` inits with NULLABLE members present-as-null (task: the
//! axios fetch-adapter crash — after `document.cookie` landed, Mastodon's
//! axios reached its fetch adapter and died again two layers deeper).
//!
//! boa_runtime 0.22 converts every fetch/Request init through `TryFromJs`,
//! whose `Option<T>` demuxes only `undefined`: `{signal: null}` died as
//! `TypeError: cannot convert value to a Object` and `{body: null}` as
//! "Request constructor: body is not a supported type" — while browsers
//! read both as "no signal" / "no body" (and Mastodon's axios sends
//! exactly that shape: a null-prototype init, null-ish signal and data on
//! GETs). The prelude now normalises present-but-null `signal`/`body` to
//! undefined through a shallow copy before the raw converter sees them;
//! this suite proves the end-to-end behaviour on the REAL tab:
//!
//! * `fetch(url, {signal: null, body: null})` answers 200 (response-side
//!   echo — `Request` itself exposes no getters in this engine),
//! * the full axios-shaped null-prototype init fetches 200 and the caller's
//!   object is not mutated,
//! * the normalisation copy KEEPS real bodies: a POST with `{signal: null,
//!   body: "PAYLOAD-42"}` arrives on the server byte-intact,
//! * a real `AbortSignal` still aborts (normalisation never swallows it),
//! * a `Request` constructed from the axios-shaped init works as fetch
//!   input (`instanceof` holds through the wrapper).
//!
//! Run with: `cargo test -p ironsurf-tab --test fetch_init`

use std::io::{Read, Write};
use std::net::TcpListener;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

use ironsurf_common::{read_frame, write_frame, FromTab, ToTab};

const DEADLINE: Duration = Duration::from_secs(60);

/// Bodies the fixture `POST /echo` requests carried, per arrival — the
/// wire-side record proving the normalisation copy kept real payloads.
fn echoed() -> &'static std::sync::Mutex<Vec<String>> {
    static ECHOED: std::sync::OnceLock<std::sync::Mutex<Vec<String>>> =
        std::sync::OnceLock::new();
    ECHOED.get_or_init(|| std::sync::Mutex::new(Vec::new()))
}

/// Builds the probe page for `port`; every scenario runs over this origin.
fn page(port: u16) -> String {
    format!(
        r#"<!doctype html>
<html>
  <head><title>fetch-init page</title></head>
  <body style="margin:0;background:#ffffff">
    <script>
      var B = "http://127.0.0.1:{port}";
      var fails = [];
      async function main() {{
        // 1. present-but-null signal AND body: browsers read "no signal,
        // no body" — this must fetch, not throw the conversion TypeError.
        try {{
          var r1 = await fetch(B + "/ok", {{ signal: null, body: null }});
          var t1 = await r1.text();
          if (r1.status !== 200 || t1 !== "OK-BODY") fails.push("r1:" + r1.status + ":" + t1);
        }} catch (e) {{ fails.push("r1:" + e.message); }}
        // 2. the exact axios shape (null-prototype init and headers, null
        // signal + body, unknown members tolerated), and purity.
        try {{
          var h = Object.assign(Object.create(null), {{ Accept: "application/json, text/plain, */*" }});
          var P = Object.assign(Object.create(null), {{
            signal: null, method: "GET", headers: h, body: null,
            duplex: "half", credentials: "same-origin",
          }});
          var r2 = await fetch(B + "/ok", P);
          if (r2.status !== 200) fails.push("r2:" + r2.status);
          if (P.signal !== null || P.body !== null) fails.push("r2-mutated");
        }} catch (e) {{ fails.push("r2:" + e.message); }}
        // 3. a REAL body beside a null signal must survive the copy.
        try {{
          var r3 = await fetch(B + "/echo", {{ method: "POST", signal: null, body: "PAYLOAD-42" }});
          var t3 = await r3.text();
          if (t3 !== "PAYLOAD-42") fails.push("r3:" + t3.slice(0, 24));
        }} catch (e) {{ fails.push("r3:" + e.message); }}
        // 4. an already-aborted real signal still rejects.
        try {{
          var ac = new AbortController();
          ac.abort("stop");
          await fetch(B + "/ok", {{ signal: ac.signal }});
          fails.push("r4-resolved");
        }} catch (e) {{ /* the abort is the pass */ }}
        // 5. PATH-RELATIVE resources resolve against the document base
        // like browsers (the Mastodon API style) — not "builder error".
        try {{
          var r6 = await fetch("/ok", {{ signal: null }});
          var t6 = await r6.text();
          if (r6.status !== 200 || t6 !== "OK-BODY") fails.push("r6:" + r6.status);
          var q6 = new Request("/ok");
          var r6b = await fetch(q6);
          if (r6b.status !== 200) fails.push("r6b:" + r6b.status);
        }} catch (e) {{ fails.push("r6:" + e.message); }}
        // 6. Request built from the axios-shaped init is valid fetch input
        // (the class surface survived the wrapper).
        try {{
          var q = new Request(B + "/ok", {{ signal: null, method: "GET", body: null }});
          var okIo = q instanceof Request && Request.prototype.isPrototypeOf(q);
          var r5 = await fetch(q);
          if (!okIo || r5.status !== 200) fails.push("r5:" + okIo + ":" + r5.status);
        }} catch (e) {{ fails.push("r5:" + e.message); }}
        document.title = fails.length === 0 ? "FETCHINIT-OK" : "FETCHINIT-FAIL:" + fails.join(",");
      }}
      main().catch(function (e) {{ document.title = "FETCHINIT-THROW:" + e.message; }});
    </script>
  </body>
</html>"#,
        port = port
    )
}

/// Minimal HTTP/1.1 fixture on 127.0.0.1 (Connection: close), three routes:
/// `/` the probe page, `/ok` a static 200, `/echo` answering with the exact
/// request body it received. Returns the bound port.
fn spawn_local_http() -> u16 {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind loopback");
    let port = listener.local_addr().expect("addr").port();
    let body = page(port);
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
                .to_string();
            let content_length: usize = head
                .lines()
                .find(|line| line.to_ascii_lowercase().starts_with("content-length:"))
                .and_then(|line| line["content-length:".len()..].trim().parse().ok())
                .unwrap_or(0);
            // Read the request body (if any) off the same connection.
            let got = buf.len().saturating_sub(head.find("\r\n\r\n").map_or(0, |i| i + 4));
            let mut payload = vec![0u8; content_length.saturating_sub(got).max(0)];
            if !payload.is_empty() {
                let _ = stream.read_exact(&mut payload);
            }
            if path == "/echo" {
                echoed().lock().unwrap().push(String::from_utf8_lossy(&payload).to_string());
            }
            let (body_bytes, ctype) = match path.as_str() {
                "/" => (body.as_bytes().to_vec(), "text/html; charset=utf-8"),
                "/ok" => (b"OK-BODY".to_vec(), "text/plain; charset=utf-8"),
                "/echo" => (payload, "text/plain; charset=utf-8"),
                _ => (b"nope".to_vec(), "text/plain; charset=utf-8"),
            };
            let response = format!(
                "HTTP/1.1 200 OK\r\nConnection: close\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\n\r\n",
                body_bytes.len()
            );
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.write_all(&body_bytes);
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
    fn pump(&mut self) -> bool {
        match self.events.recv_timeout(Duration::from_millis(500)) {
            Ok(msg) => {
                self.seen.push(msg);
                true
            }
            Err(_) => false,
        }
    }

    fn titles(&self) -> Vec<String> {
        self.seen
            .iter()
            .filter_map(|msg| match msg {
                FromTab::TitleChanged { title: Some(t) } => Some(t.clone()),
                _ => None,
            })
            .collect()
    }

    fn wait_title(&mut self, needle: &str) {
        let deadline = Instant::now() + DEADLINE;
        loop {
            if self.titles().iter().any(|t| t.contains(needle)) {
                return;
            }
            if Instant::now() >= deadline {
                let status = self.child.try_wait().ok().flatten();
                panic!(
                    "timed out waiting for title containing {needle:?} \
                     (child status: {status:?})\nseen titles: {:?}",
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
        .arg("73")
        .arg("--container-id")
        .arg("1")
        .arg("--profile-dir")
        .arg(profile_dir)
        .arg("--url")
        .arg(url)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(
            std::fs::File::create(profile_dir.join(format!(
                "tab-fetch-init-stderr-{}.log",
                std::process::id()
            )))
            .expect("stderr log"),
        )
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

#[test]
fn null_signal_and_body_fetch_like_browsers_real_signals_and_bodies_survive() {
    let port = spawn_local_http();
    let profile = tempfile::tempdir().expect("tempdir");
    let mut tab = spawn_tab(&format!("http://127.0.0.1:{port}/"), profile.path());

    // Every scenario above runs on load; the title is the verdict.
    tab.wait_title("FETCHINIT-OK");

    // And the POST body really crossed the wire byte-intact despite the
    // init having been normalised (a shallow-copy regression would show a
    // missing/empty echo here, not in the title — the title already pins
    // the client-side view).
    let deadline = Instant::now() + DEADLINE;
    loop {
        let seen = echoed().lock().unwrap().clone();
        if seen.iter().any(|body| body == "PAYLOAD-42") {
            break;
        }
        if Instant::now() >= deadline {
            panic!("the POST body never arrived on the wire; echoed: {seen:?}");
        }
        tab.pump();
    }
}
