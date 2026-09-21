//! Live forensic probe against https://infosec.exchange (IGNORED; needs the
//! network). Injects a userscript BEFORE the app scripts that:
//!   (a) wraps setTimeout/setInterval/clear*/rAF with a schedule/cancel audit
//!       (gated to after t+60 s so boot noise stays out of the log), keeping a
//!       live-timer ledger;
//!   (b) wraps fetch + XMLHttpRequest (status + /api/ body prefixes);
//!   (c) every 10 s beats location/article-count/live-timer-ledger;
//!   (d) at t+105 s walks the React fiber tree for the redux Provider store,
//!       dumps the immutable state (timelines, hashtags, me, alerts) and wraps
//!       store.dispatch to log every subsequent action type;
//!   (e) at t+112 s clicks the explore "Latest"/"Live feed" tab and re-dumps
//!       the store at t+125 s, so we see whether a click dispatches anything.
//!
//! Run with:
//!   cargo test -p ironsurf-tab --test ix_forensic -- --ignored --nocapture

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

const PROBE: &str = r##"// ==UserScript==
// @name     forensic
// @run-at   document-start
// ==/UserScript==
(function () {
  var t0 = Date.now();
  function ts() { return Math.round((Date.now() - t0) / 1000); }
  function log(m) { try { console.log("[FX +" + ts() + "s] " + m); } catch (e) {} }
  function stk() {
    try {
      var s = (new Error()).stack || "";
      return s.split("\n").slice(2, 5).join(" | ").replace(/\s+/g, " ").slice(0, 230);
    } catch (e) { return "?"; }
  }
  log("env WS=" + typeof WebSocket + " ES=" + typeof EventSource + " rIC=" + typeof requestIdleCallback +
      " IO=" + typeof IntersectionObserver + " perf=" + typeof performance +
      " MC=" + typeof MessageChannel + " matchMedia=" + typeof matchMedia);

  // ---- (a) timer surveillance -------------------------------------------
  var gated = false, schedBudget = 500;
  var live = {}, liveCount = 0, order = [];
  var _st = window.setTimeout, _si = window.setInterval;
  // ---- (a-1) react-redux context spy ------------------------------------
  // React 19: createElement(Context, props, value) where Context.$$typeof is
  // Symbol.for('react.context'); hook context values through a Proxy to catch
  // store/subscription/getServerState reads that return null during renders.
  try {
    var CTXSYM = Symbol.for("react.context");
    var patched = 0, poll = _si(function () {
      var ks = Object.keys(window).filter(function (k) {
        try { return /^_r[a-z0-9]{3,8}$/.test(k) && window[k] && window[k].length >= 3 && typeof window[k][0] === "object"; } catch (e2) { return false; }
      });
      var found = null;
      for (var ki = 0; ki < ks.length; ki++) { try { if (window[ks[ki]].createElement) { found = ks[ki]; break; } } catch (e3) {} }
      if (!found) return;
      _si(function () {}, 1); // noop
      var R = window[found];
      if (R.__ispied) return;
      try { R.__ispied = true; } catch (e4) { log("spy: R frozen, trying defineProperty"); try { Object.defineProperty(R, "__ispied", { value: true }); } catch (e5) {} }
      log("spy: React found on " + found);
      var _ce = R.createElement.bind(R);
      R.createElement = function (type, props) {
        try {
          if (type && type.$$typeof === CTXSYM && arguments.length > 2 && arguments[2] && typeof arguments[2] === "object" && !arguments[2].__proxy) {
            var v0 = arguments[2], seen = {};
            var px = new Proxy(v0, {
              get: function (t2, p2) {
                var val = t2[p2];
                if ((val === null || val === undefined) && typeof p2 === "string" && !(p2 in seen)) {
                  seen[p2] = 1;
                  log("PROVIDER-READ null member=" + p2 + " keys=" + Object.keys(t2).join(",") + " @ " + stk());
                }
                return val;
              }
            });
            try { Object.defineProperty(px, "__proxy", { value: 1 }); } catch (e6) {}
            arguments[2] = px;
            patched++;
          }
        } catch (e7) { if (!log.failed) { try { log("spy-ce-err " + e7); } catch (e8) { log.failed = 1; } } }
        return _ce(type, props, arguments[2], arguments[3]);
      };
      patched = 1;
      clearInterval(poll);
      log("spy: createElement wrapped, polling stopped");
    }, 300);
    _st(function () { log("spy: status patched=" + patched); }, 60000);
  } catch (e) { log("spy-err " + e); }

  // ---- (a0) initial-state bootstrap forensics ---------------------------
  try {
    if (!localStorage.getItem("mastodon-settings")) {
      localStorage.setItem("mastodon-settings", JSON.stringify({
        tag_misleading_links: false, rewrite_mentions: "no",
        content_warnings: { filter: null, shared_state: false },
        media: {}, notifications: {}, status_icons: {},
      }));
      log("SEEDED mastodon-settings");
    }
  } catch (e) { log("seed-err " + e); }
  _st(function () {
    try {
      var el = document.getElementById("initial-state");
      var tc = el && el.textContent;
      log("init-el=" + !!el + " tcType=" + typeof tc + " tcLen=" + (tc ? tc.length : -1));
      if (tc && tc.length > 2) {
        var dd = JSON.parse(tc);
        log("init-keys=" + Object.keys(dd).length + " hasLocalSettings=" + ("local_settings" in dd));
      }
      log("ls-raw=" + String(localStorage.getItem("mastodon-settings")).slice(0, 60));
    } catch (e) { log("init-dump-err " + e); }
  }, 3000);


  var _ct = window.clearTimeout, _ci = window.clearInterval;
  var _raf = window.requestAnimationFrame, _caf = window.cancelAnimationFrame;
  function addLive(k, d) {
    liveCount++; order.push(k); live[k] = d;
    if (gated && schedBudget-- > 0) log("SCHED " + d);
  }
  function delLive(k) {
    if (live[k]) { liveCount--; delete live[k]; var i = order.indexOf(k); if (i >= 0) order.splice(i, 1); }
  }
  window.setTimeout = function (fn, ms) {
    var id = _st.apply(window, arguments);
    addLive("t" + id, "tmo#" + id + " ms=" + ms + " @" + stk());
    return id;
  };
  window.setInterval = function (fn, ms) {
    var id = _si.apply(window, arguments);
    addLive("i" + id, "int#" + id + " ms=" + ms + " @" + stk());
    return id;
  };
  window.clearTimeout = function (id) {
    if (gated && live["t" + id]) log("CLEAR tmo#" + id + " (" + live["t" + id] + ") by " + stk());
    delLive("t" + id);
    return _ct.apply(window, arguments);
  };
  window.clearInterval = function (id) {
    if (gated && live["i" + id]) log("CLEAR int#" + id + " (" + live["i" + id] + ") by " + stk());
    delLive("i" + id);
    return _ci.apply(window, arguments);
  };
  window.requestAnimationFrame = function (fn) {
    var id = _raf.apply(window, arguments);
    if (gated) addLive("r" + id, "raf#" + id + " @" + stk());
    return id;
  };
  window.cancelAnimationFrame = function (id) {
    delLive("r" + id);
    return _caf ? _caf.apply(window, arguments) : undefined;
  };
  _st(function () { gated = true; log("timer surveillance ARMED; live-so-far=" + liveCount); }, 55000);

  // ---- (b) network wrappers ----------------------------------------------
  var PX = XMLHttpRequest.prototype;
  if (PX && PX.send) {
    var _open = PX.open, _send = PX.send;
    PX.open = function (m, u) { this._m = m; this._u = String(u); return _open.apply(this, arguments); };
    PX.send = function (b) {
      var self = this;
      try { _send.apply(self, arguments); } catch (e) { log("xhr THREW " + self._u + " :: " + e); throw e; }
      var body = "";
      try { if ((self._u || "").indexOf("/api/") >= 0 && self.responseType === "") body = (self.responseText || "").slice(0, 140); } catch (e) {}
      log("xhr " + self._m + " " + self._u + " -> " + self.status + " len=" + ((self.responseText || "").length) + (body ? " body=" + JSON.stringify(body) : ""));
    };
  }
  var _fetch = window.fetch;
  if (typeof _fetch === "function") {
    window.fetch = function () {
      var u = (arguments[0] && arguments[0].url) || String(arguments[0]);
      var p;
      try { p = _fetch.apply(this, arguments); } catch (e) { log("fetch sync-throw " + u + " " + e); throw e; }
      if (p && p.then) p.then(function (r) { log("fetch " + u + " -> " + r.status); },
                               function (e) { log("fetch " + u + " REJECT " + e); });
      return p;
    };
  }

  // ---- store forensics ----------------------------------------------------
  var store = null, dumped = 0;
  function findStore() {
    var rootEl = document.getElementById("root") || document.body.firstElementChild;
    if (!rootEl) return null;
    var keys = Object.keys(rootEl).filter(function (k) { return k.indexOf("__react") === 0 || k === "_reactRootContainer"; });
    log("root keys: " + keys.join(","));
    var fiber = null;
    for (var i = 0; i < keys.length && !fiber; i++) {
      var v = rootEl[keys[i]];
      if (v && v.current) fiber = v.current; else if (v) fiber = v;
    }
    var seen = 0, q = [fiber];
    while (q.length && seen < 30000) {
      var f = q.shift(); seen++;
      if (!f) continue;
      var p = f.memoizedProps;
      if (p && p.store && typeof p.store.dispatch === "function" && typeof p.store.getState === "function") {
        log("store found after " + seen + " fibers");
        return p.store;
      }
      if (f.child) q.push(f.child);
      if (f.sibling) q.push(f.sibling);
    }
    log("store NOT found (fibers scanned=" + seen + ")");
    return null;
  }
  function g(get, path) { try { var v = get; for (var i = 0; i < path.length && v != null; i++) v = typeof v.get === "function" ? v.get(path[i]) : v[path[i]]; return v; } catch (e) { return "<err " + e + ">"; } }
  function sizeOf(v) { if (v == null) return "?"; if (typeof v.size === "number") return v.size; if (Array.isArray(v)) return v.length; return "?"; }
  function dumpStore(tag) {
    if (!store) return;
    var S;
    try { S = store.getState(); } catch (e) { log(tag + " getState THREW " + e); return; }
    var keys = null;
    try {
      if (typeof S.keySeq === "function") keys = S.keySeq().toArray().join(",");
      else if (typeof S.keys === "function") keys = Array.from(S.keys()).join(",");
    } catch (e) { keys = "<keyerr " + e + ">"; }
    if (keys === null) keys = Object.keys(S).join(",");
    log(tag + " keys: " + keys);
    var tl = g(S, ["timelines"]);
    if (tl && typeof tl.keySeq === "function") {
      tl.keySeq().toArray().forEach(function (name) {
        var t = tl.get(name);
        log(tag + " timeline[" + name + "] entries=" + sizeOf(g(t, ["entries"])) +
            " isFetching=" + g(t, ["isFetching"]) + " hasMore=" + g(t, ["hasMore"]) +
            " next=" + String(g(t, ["next"])).slice(0, 40));
      });
    } else { log(tag + " timelines: " + JSON.stringify(String(tl)).slice(0, 80)); }
    log(tag + " hashtags=" + sizeOf(g(S, ["hashtags", "items"])) + " accounts=" + sizeOf(g(S, ["accounts", "items"])) +
        " statuses=" + sizeOf(g(S, ["statuses", "items"])) + " me=" + String(g(S, ["me"])) +
        " alerts=" + sizeOf(g(S, ["alerts"])) + " pushFailed=" + String(g(S, ["pushFailed"])));
    try {
      var tr = S.getIn ? S.getIn(["trends"]).toJS() : null;
      log(tag + " trends=" + JSON.stringify(tr).slice(0, 900));
    } catch (e) { log(tag + " trends dump threw " + e); }
    try {
      log(tag + " bodySays trend=" + (document.body.textContent.indexOf("Trending") >= 0) +
          " silent=" + (document.body.textContent.indexOf("silentsunday") >= 0) +
          " articles=" + document.querySelectorAll("article").length);
    } catch (e) { /* ignore */ }
    try {
      var plain = S.toJS ? S.toJS() : null;
      if (plain) {
        var j = JSON.stringify(plain);
        log(tag + " toJS len=" + j.length);
        for (var off = 0; off < j.length && off < 8000; off += 1600) log(tag + " | " + j.slice(off, off + 1600));
      } else { log(tag + " no toJS on state"); }
    } catch (e) { log(tag + " toJS threw " + e); }
  }
  var actions = [];
  function wrapDispatch() {
    try {
      var d = store.dispatch;
      store.dispatch = function (a) {
        var t = (a && a.type) || "?";
        actions.push(t);
        if (actions.length < 120) log("DISPATCH " + t + " " + stk().slice(0, 150));
        return d.apply(store, arguments);
      };
      log("dispatch wrapped");
    } catch (e) { log("dispatch wrap failed " + e); }
  }

  function fire(t, payload) {
    try { store.dispatch(Object.assign({ type: t }, payload || {})); log("fired " + t); } catch (e) { log("fire " + t + " threw " + e); }
  }

  // ---- beats + timed forensics --------------------------------------------
  var n = 0;
  _si(function () {
    n++;
    try {
      var d = document;
      log("beat" + n + " rs=" + d.readyState + " loc=" + location.href +
          " arts=" + d.querySelectorAll("article").length +
          " trendsLi=" + d.querySelectorAll(".pinned-hashtags-list__item, .trend, li.item").length +
          " loadingTxt=" + (d.body && d.body.innerHTML.indexOf("Loading") >= 0) +
          " liveTimers=" + liveCount + " gated=" + gated);
      if (liveCount > 0) {
        var shown = 0;
        for (var oi = Math.max(0, order.length - 12); oi < order.length && shown < 12; oi++, shown++) {
          log("  live " + live[order[oi]]);
        }
      }
      if (n >= 2 && !store && dumped < 1) {
        store = findStore();
        if (store) {
          dumpStore("S" + dumped);
          wrapDispatch();
          dumped++;
          // Decisive test: drive the trends saga BY HAND. If REQUEST -> XHR ->
          // SUCCESS -> render happens when dispatched now, the boot-time stall
          // is not the saga pipeline but whatever mounted before us.
          _st(function () {
            fire("TRENDS_TAGS_FETCH_REQUEST", { limit: 12, beyond: false });
            fire("TRENDS_STATUSES_FETCH_REQUEST", { limit: 12, beyond: false });
          }, 2000);
          _st(function () { dumpStore("after-manual-trends"); }, 12000);
          _st(function () {
            try {
              var st0 = store.getState ? store.getState() : null;
              var ls = st0 && st0.getIn ? st0.getIn(["local_settings"]) : (st0 ? st0.local_settings : undefined);
              log("state.local_settings=" + (ls === undefined ? "UNDEFINED" : ls === null ? "NULL" :
                  (typeof ls.get === "function" ? "Immutable(size=" + ls.size + ") rm=" + ls.get("rewrite_mentions", "DEF") :
                   "plain:" + JSON.stringify(ls).slice(0, 80))));
              var topKeys = st0 && st0.keySeq ? st0.keySeq().toArray().slice(0, 60).join(",") : Object.keys(st0 || {}).slice(0, 60).join(",");
              log("state-keys=" + topKeys);
            } catch (e) { log("ls-dump-err " + e); }
          }, 2500);
        }
      }
    } catch (e) { log("beat-err " + e); }
  }, 10000);
  _si(function () { if (store) dumpStore("re"); }, 30000);
  _st(function () {
    var labels = ["Latest", "Live feed", "Trending"];
    var links = document.querySelectorAll("a, button, [role=tab]");
    outer: for (var li = 0; li < labels.length; li++) {
      for (var i = 0; i < links.length; i++) {
        var t = (links[i].textContent || "").trim();
        if (t === labels[li]) {
          log("clicking <" + labels[li] + "> tag=" + links[i].tagName + " href=" + (links[i].href || "-") +
              " typeofClick=" + typeof links[i].click + " typeofDispatch=" + typeof links[i].dispatchEvent);
          try {
            if (typeof links[i].click === "function") { links[i].click(); log("click ok"); }
            else { var ev = new MouseEvent("click", { bubbles: true, cancelable: true, view: window }); links[i].dispatchEvent(ev); log("dispatchEvent ok"); }
          } catch (e) { log("click threw " + e + " STACK: " + String((e && e.stack) || "").replace(/\n/g, " ¶ ").slice(0, 900)); }
          break outer;
        }
      }
    }
  }, 105000);
  _st(function () {
    log("post-click: " + actions.length + " actions seen; tail=" + actions.slice(-30).join(","));
    dumpStore("post-click");
    // Last-ditch probe: hit the public timeline endpoint by hand.
    try {
      var x = new XMLHttpRequest();
      x.open("GET", "/api/v1/timelines/public?limit=5&only_media=false&local=true", false);
      x.send(null);
      log("manual timelines/public?local=true -> " + x.status + " len=" + (x.responseText || "").length);
      var y = new XMLHttpRequest();
      y.open("GET", "/api/v1/trends/tags?limit=5", false);
      y.send(null);
      log("manual trends/tags -> " + y.status + " len=" + (y.responseText || "").length);
    } catch (e) { log("manual timelines/public threw " + e); }
  }, 125000);
})();"##;

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
#[ignore = "live network forensics against infosec.exchange"]
fn forensic_infosec_explore() {
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
                name: "forensic".into(),
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

    let stop = Instant::now() + Duration::from_secs(230);
    let mut probe_lines = 0usize;
    while Instant::now() < stop {
        match tab.events.recv_timeout(Duration::from_millis(1000)) {
            Ok(FromTab::ConsoleLog {
                source,
                level,
                message,
            }) => {
                println!("[{:5.1} {source}:{level}] {message}", start.elapsed().as_secs_f64());
                if message.contains("FX +") {
                    probe_lines += 1;
                }
            }
            Ok(FromTab::UrlChanged { url }) => println!("[url] {url:?}"),
            Ok(FromTab::TitleChanged { title }) => println!("[title] {title:?}"),
            Ok(FromTab::LoadingChanged { loading }) => {
                println!("[loading={loading} t={:.1}]", start.elapsed().as_secs_f64())
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
