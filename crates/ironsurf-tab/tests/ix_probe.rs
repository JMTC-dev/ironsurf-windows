//! Live probe against https://infosec.exchange (IGNORED by default; needs the
//! network). Injects a userscript that wraps the page's XMLHttpRequest and
//! `fetch`, then streams every console frame for ~2.5 minutes so we can see
//! which API calls the app makes after its route boots and what each returns.
//!
//! Run with:
//!   cargo test -p ironsurf-tab --test ix_probe -- --ignored --nocapture

use std::io::{Read, Write};
use std::net::TcpListener;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

use ironsurf_common::{read_frame, write_frame, FromTab, ToTab, Userscript};

const BLANK: &str = "<!doctype html><html><head><title>blank</title></head>\
                     <body style=\"margin:0;background:#ffffff\"><p>blank</p></body></html>";

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
            let response = format!(
                "HTTP/1.0 200 OK\r\nContent-Type: text/html\r\nContent-Length:\
                 {}\r\nConnection: close\r\n\r\n{}",
                BLANK.len(),
                BLANK
            );
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
        }
    });
    port
}

const PROBE: &str = r#"(function () {
  var t0 = Date.now();
  function log(m) { try { console.log("[PROBE +" + Math.round((Date.now() - t0) / 1000) + "s] " + m); } catch (e) {} }
  log("alive loc=" + location.href +
      " WS=" + typeof WebSocket + " ES=" + typeof EventSource +
      " fetch=" + typeof fetch + " XHR=" + typeof XMLHttpRequest +
      " localStorage=" + typeof localStorage +
      " IDB=" + (typeof indexedDB) + " crypto=" + (typeof crypto) +
      " subtle=" + (typeof (crypto && crypto.subtle)) +
      " BroadcastChannel=" + typeof BroadcastChannel +
      " matchMedia=" + typeof matchMedia + " RTL=" + typeof ResizeObserver +
      " IO=" + typeof IntersectionObserver + " MutationObserver=" + typeof MutationObserver);
  var P = XMLHttpRequest.prototype;
  if (P && P.send) {
    var open = P.open, send = P.send;
    P.open = function (m, u) { this._pm = m; this._pu = u; return open.apply(this, arguments); };
    P.send = function (b) {
      var self = this;
      try {
        send.apply(self, arguments);
        log("xhr " + (self._pm || "?") + " " + (self._pu || "?") +
            " type=" + self.responseType + " status=" + self.status +
            " len=" + ((self.responseText) || "").length +
            " ct=" + (self.getResponseHeader("content-type") || "?"));
      } catch (e) {
        log("xhr THREW " + (self._pu || "?") + " :: " + e);
        throw e;
      }
    };
  }
  var f = globalThis.fetch;
  if (typeof f === "function") {
    globalThis.fetch = function () {
      try {
        var u = (arguments[0] && arguments[0].url) || String(arguments[0]);
        var p = f.apply(this, arguments);
        if (p && typeof p.then === "function") {
          p.then(function (r) { log("fetch " + u + " -> " + r.status); },
                 function (e) { log("fetch " + u + " REJECT " + e); });
        }
        return p;
      } catch (e) { log("fetch sync-throw " + e); throw e; }
    };
  }
  // Click-through experiment: 20 s after we start, click the explore tabs
  // ("Trending"/"Live feed") so column mounts force fresh dispatches;
  // 45 s in, try a router push to the public timeline directly.
  function clickText(label) {
    var links = document.querySelectorAll("a, button, [role=tab]");
    for (var i = 0; i < links.length; i++) {
      var t = (links[i].textContent || "").trim();
      if (t === label || t.indexOf(label) === 0) {
        log("clicking <" + label + "> tag=" + links[i].tagName + " href=" + (links[i].href || "-"));
        try { links[i].click(); log("clicked " + label); return true; } catch (e) { log("click threw " + e); return false; }
      }
    }
    log("no element for <" + label + "> (links=" + links.length + ")");
    return false;
  }
  globalThis.__ironsurfSetTimeout(function () { clickText("Live feed"); }, 95000);
  globalThis.__ironsurfSetTimeout(function () { clickText("Trending"); }, 112000);
  var n = 0;
  setInterval(function () {
    n++;
    try {
      var html = document.body ? document.body.innerHTML : "";
      log("beat" + n + " loc=" + location.href +
          " statuses=" + document.querySelectorAll("[data-id^='statuses-']").length +
          " articles=" + document.querySelectorAll("article").length +
          " trendsItems=" + document.querySelectorAll(".trend, .pinned-hashtags-list li, .trends--list li").length +
          " loadingTxt=" + (html.indexOf("Loading") >= 0) +
          " errTxt=" + (html.indexOf("Something went wrong") >= 0));
    } catch (e) { log("beat-err " + e); }
  }, 12000);
})();"#;

struct TabChild {
    child: Child,
    stdin: ChildStdin,
    events: Receiver<FromTab>,
}

impl Drop for TabChild {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
#[ignore = "live network probe against infosec.exchange"]
fn probe_infosec_trending() {
    let port = spawn_local_http();
    let profile = tempfile::tempdir().expect("profile dir");
    let exe = env!("CARGO_BIN_EXE_ironsurf-tab");
    let mut child = Command::new(exe)
        .env_remove("DISPLAY")
        .env_remove("WAYLAND_DISPLAY")
        .arg("--tab-id")
        .arg("99")
        .arg("--container-id")
        .arg("1")
        .arg("--profile-dir")
        .arg(profile.path())
        .arg("--log")
        .arg("debug")
        .arg("--url")
        .arg(format!("http://127.0.0.1:{port}/probe-page"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(std::fs::File::create(profile.path().join("tab-stderr.log")).expect("stderr log"))
        .spawn()
        .expect("spawn ironsurf-tab");

    let stdout = child.stdout.take().expect("stdout");
    let stdin = child.stdin.take().expect("stdin");
    let (tx, events) = mpsc::channel();
    thread::spawn(move || {
        let mut reader = std::io::BufReader::new(stdout);
        while let Ok(msg) = read_frame::<_, FromTab>(&mut reader) {
            if tx.send(msg).is_ok() {
                // keep reading
            } else {
                break;
            }
        }
    });

    let mut tab = TabChild {
        child,
        stdin,
        events,
    };

    // Wait for the blank page, install the probe, then navigate for real.
    let start = Instant::now();
    let deadline = start + Duration::from_secs(30);
    loop {
        match tab.events.recv_timeout(Duration::from_millis(500)) {
            Ok(FromTab::LoadingChanged { loading: false }) => break,
            Ok(_) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => panic!("tab died early"),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if Instant::now() > deadline {
                    panic!("blank page never loaded");
                }
            }
        }
    }

    write_frame(
        &mut tab.stdin,
        &ToTab::SetUserscripts {
            scripts: vec![Userscript {
                id: 1,
                name: "probe".into(),
                patterns: vec!["*".into()],
                code: PROBE.into(),
                enabled: true,
            }],
        },
    )
    .expect("write userscripts");
    write_frame(
        &mut tab.stdin,
        &ToTab::Navigate {
            url: "https://infosec.exchange".to_string(),
        },
    )
    .expect("write navigate");

    // Stream console + navigation frames for ~150 s.
    let stop = Instant::now() + Duration::from_secs(165);
    let mut probe_lines = 0usize;
    while Instant::now() < stop {
        match tab.events.recv_timeout(Duration::from_millis(1000)) {
            Ok(FromTab::ConsoleLog {
                source,
                level,
                message,
            }) => {
                println!("[{source}:{level}] {message}");
                if message.contains("PROBE") {
                    probe_lines += 1;
                }
            }
            Ok(FromTab::UrlChanged { url }) => println!("[url] {url:?}"),
            Ok(FromTab::TitleChanged { title }) => println!("[title] {title:?}"),
            Ok(FromTab::LoadingChanged { loading }) => {
                println!("[loading={} t={:?}]", loading, start.elapsed())
            }
            Ok(FromTab::Error { message }) => println!("[ERR] {message}"),
            Ok(_) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
    }
    println!("probe_lines={probe_lines}");
    drop(tab);
    assert!(probe_lines > 0, "probe userscript never ran");
}
