//! The DevTools **Inspector** pane against the REAL engine: the shell's own IPC
//! path (`TabProc`/`Pump`, the plumbing the pane consumes in production) driving
//! the workspace-built `ironsurf-tab` over a `127.0.0.1` fixture, asking it for
//! its document and then asking it what is under a point.
//!
//! `tests/inspector.rs` proves the shell keeps a correct view of a scripted
//! document. This proves the same wiring reaches Blitz, which is the only side
//! that can answer these:
//!
//! * `ToTab::SetInspect { open: true }` → a real [`InspectSnapshot`] of the
//!   fixture: a single `Document` root at depth 0, pre-order depths that never
//!   skip a level, the fixture's elements with their id/class attributes, real
//!   text rows with no whitespace-only filler, and `truncated: false` for a page
//!   this small;
//! * the boxes are viewport CSS px of the *live* layout — `#target`, positioned
//!   absolutely, arrives at its own coordinates with a positive width/height, so
//!   the pane can be believed when it paints a selection over the page;
//! * `<head>` is in the tree but never laid out (its box is `None`), and so is a
//!   `display:none` element — the pane must not invent a box for either;
//! * the ⌖ round trip: `ToTab::InspectPickAt` at a point **taken from the
//!   snapshot the tab just sent** (the centre of `#target`'s own box) answers
//!   `FromTab::InspectPicked` with that very node, and the snapshot that comes
//!   with it carries the same selection;
//! * arming the crosshair reports what is under the cursor as it changes, and
//!   disarming takes that report away (`InspectHover` `Some` → `None`);
//! * selecting a row of the current generation is accepted silently, and an id
//!   of a **dead generation** is ignored without an error and without ending the
//!   stream — the shell may be a message behind the page;
//! * closing the pane stops the walk: a full repaint cycle afterwards brings no
//!   further snapshots.
//!
//! Skips, like `tests/devtools_e2e.rs`, when the tab binary was not built with
//! the workspace. Headless: no `$DISPLAY`, localhost only.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};

use ironsurf_common::{
    ContainerId, FromTab, InspectKind, InspectNode, InspectSnapshot, MouseButton, PointerEvent,
    PointerKind, TabId, ToTab,
};
use ironsurf_shell::tabs::{Pump, TabProc};

/// The engine's first paint and first DOM walk are slow on cold caches.
const DEADLINE: Duration = Duration::from_secs(120);

/// A page with a tree worth inspecting: a nesting three deep with id and class
/// attributes, real text, an absolutely-positioned target box, an unlaid-out
/// `<head>`, a `display:none` element and whitespace between elements (which the
/// tab must drop rather than show as blank rows).
const PAGE: &str = r#"<!doctype html>
<html>
  <head>
    <title>Inspector e2e</title>
    <style>
      html, body { margin: 0; background: #ffffff; }
      #target { position: absolute; left: 20px; top: 30px; width: 120px; height: 80px; background: #00aa00; }
      #hidden { display: none; }
      .note { color: #000088; }
    </style>
  </head>
  <body>
    <section id="main">
      <p class="note">inspector paragraph</p>
      <div id="hidden">hidden text</div>
    </section>
    <div id="target" class="box note">inside the target</div>
  </body>
</html>
"#;

/// Minimal HTTP/1.0 fixture on 127.0.0.1 (one request per connection, kept alive
/// for the whole test process); returns the bound port.
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
            let response = http_response(200, "text/html", PAGE.as_bytes());
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
            let _ = head;
        }
    });
    port
}

fn http_response(status: u16, content_type: &str, body: &[u8]) -> String {
    format!(
        "HTTP/1.0 {status} OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    ) + &String::from_utf8_lossy(body)
}

/// The workspace-built `ironsurf-tab` (same resolution as `tests/devtools_e2e.rs`).
fn real_tab_bin() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let profile_dir = exe.parent()?.parent()?; // target/<profile>
    let candidate = profile_dir.join(format!("ironsurf-tab{}", std::env::consts::EXE_SUFFIX));
    candidate.is_file().then_some(candidate)
}

/// Everything the Inspector pane consumes, accumulated off the wire.
#[derive(Default)]
struct Feed {
    ready: bool,
    url: Option<String>,
    title: Option<String>,
    /// Every snapshot in arrival order; `.last()` is what the pane is showing.
    snapshots: Vec<InspectSnapshot>,
    hovers: Vec<Option<u64>>,
    picks: Vec<u64>,
    /// Painted frames off the wire: proof the tab is still working, used to show
    /// that a closed pane stops *snapshots* while the tab keeps painting.
    frames: usize,
}

impl Feed {
    fn absorb(&mut self, msg: FromTab) {
        match msg {
            FromTab::Ready { .. } => self.ready = true,
            FromTab::UrlChanged { url } => self.url = url,
            FromTab::TitleChanged { title } => self.title = title,
            FromTab::InspectSnapshot { snapshot } => self.snapshots.push(snapshot),
            FromTab::InspectHover { node } => self.hovers.push(node),
            FromTab::InspectPicked { node } => self.picks.push(node),
            FromTab::Frame { .. } => self.frames += 1,
            _ => {}
        }
    }

    fn latest(&self) -> Option<&InspectSnapshot> {
        self.snapshots.last()
    }

    fn dump(&self) -> String {
        let shapes: Vec<String> = self
            .snapshots
            .iter()
            .map(|s| format!("gen {} × {} node(s)", s.generation, s.nodes.len()))
            .collect();
        format!(
            "ready={} url={:?} title={:?}\nsnapshots: {shapes:?}\nhovers: {:?}\npicks: {:?}\n",
            self.ready, self.url, self.title, self.hovers, self.picks
        )
    }
}

/// Drain the pump until `pred` holds over the accumulated feed.
fn wait_for(what: &str, pump: &mut Pump, feed: &mut Feed, pred: impl Fn(&Feed) -> bool) {
    let deadline = Instant::now() + DEADLINE;
    loop {
        for (_, msg) in pump.drain() {
            feed.absorb(msg);
        }
        if pred(feed) {
            return;
        }
        if Instant::now() >= deadline {
            panic!("timed out waiting for {what}\n{}", feed.dump());
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn find_tag<'a>(snap: &'a InspectSnapshot, tag: &str) -> Option<&'a InspectNode> {
    snap.nodes
        .iter()
        .find(|n| n.kind == InspectKind::Element && n.tag == tag)
}

fn find_id<'a>(snap: &'a InspectSnapshot, id: &str) -> Option<&'a InspectNode> {
    snap.nodes.iter().find(|n| {
        n.kind == InspectKind::Element && n.attrs.iter().any(|(k, v)| k == "id" && v == id)
    })
}

#[test]
fn the_real_tab_hands_the_pane_a_real_document_and_a_real_pick() {
    let Some(tab_bin) = real_tab_bin() else {
        eprintln!("skip: ironsurf-tab not built (cargo build --workspace first)");
        return;
    };
    let port = spawn_local_http();
    let profile = tempfile::tempdir().expect("profile dir");
    let url = format!("http://127.0.0.1:{port}/");

    let mut pump = Pump::new();
    let mut proc = TabProc::spawn(
        &tab_bin,
        TabId(12),
        ContainerId(1),
        profile.path(),
        Some(&url),
        &[],
        &pump.sender(),
    )
    .expect("spawn real ironsurf-tab");
    let mut feed = Feed::default();

    wait_for(
        "Ready + the fixture's url and title",
        &mut pump,
        &mut feed,
        |f| {
            f.ready
                && f.url.as_deref() == Some(url.as_str())
                && f.title.as_deref() == Some("Inspector e2e")
        },
    );

    // ── the document: the pane asks, the engine walks ───────────────────────
    // Nothing was sent before this: the tab owns the walk and starts on command.
    assert!(feed.snapshots.is_empty(), "the pane was never opened: {}", feed.dump());
    proc.send(ToTab::SetInspect { open: true }).expect("send to the tab");
    wait_for("the fixture's document tree", &mut pump, &mut feed, |f| {
        f.snapshots
            .last()
            .is_some_and(|s| s.url == url && !s.nodes.is_empty())
    });

    let snap = feed.latest().expect("snapshot").clone();
    assert_eq!(snap.url, url, "the tree is stamped with the document it walked");
    assert!(snap.generation >= 1, "generation: {}", snap.generation);
    assert!(!snap.truncated, "a six-element page is not truncated: {:?}", snap.nodes.len());

    // One root, at depth 0, and the doctype the fixture declares.
    let roots: Vec<&InspectNode> = snap
        .nodes
        .iter()
        .filter(|n| n.parent.is_none() && n.kind == InspectKind::Document)
        .collect();
    assert_eq!(roots.len(), 1, "exactly one document root: {:?}", snap.nodes.len());
    assert_eq!(roots[0].depth, 0);
    assert!(
        snap.nodes
            .iter()
            .any(|n| n.kind == InspectKind::Doctype && n.text == "html"),
        "the fixture's `<!doctype html>` is a row: {:?}",
        snap.nodes.iter().take(4).map(|n| (&n.tag, &n.text)).collect::<Vec<_>>()
    );

    // Pre-order invariants the pane indents by: never a jump of more than one
    // level down, and every child deeper than its parent.
    for (a, b) in snap.nodes.iter().zip(snap.nodes.iter().skip(1)) {
        assert!(
            b.depth <= a.depth + 1,
            "depths skip a level between {:?} and {:?}",
            (a.tag.as_str(), a.depth),
            (b.tag.as_str(), b.depth)
        );
    }
    for n in &snap.nodes {
        if let Some(parent) = n.parent {
            let p = snap.nodes.iter().find(|m| m.id == parent).expect("a parent row");
            assert!(n.depth > p.depth, "{:?} hangs off {:?}", (n.tag.as_str(), n.depth), p.depth);
        }
    }

    // The fixture's elements, with the attributes the pane renders.
    for tag in ["html", "head", "body", "title", "section", "p", "div"] {
        assert!(find_tag(&snap, tag).is_some(), "no <{tag}> in {:?}", shape(&snap));
    }
    let target = find_id(&snap, "target").expect("#target");
    assert_eq!(target.tag, "div");
    assert!(
        target
            .attrs
            .iter()
            .any(|(k, v)| k == "class" && v.split_whitespace().eq(["box", "note"])),
        "class in document order: {:?}",
        target.attrs
    );
    let main = find_id(&snap, "main").expect("#main");
    let paragraph = find_tag(&snap, "p").expect("<p>");
    assert_eq!(paragraph.parent, Some(main.id), "the paragraph hangs off its section");

    // Real text rows — and no whitespace-only filler, which is what makes a
    // formatted document's tree readable rather than blank.
    let texts: Vec<&str> = snap
        .nodes
        .iter()
        .filter(|n| n.kind == InspectKind::Text)
        .map(|n| n.text.as_str())
        .collect();
    assert!(
        texts.iter().any(|t| t.contains("inspector paragraph")),
        "the paragraph's text: {texts:?}"
    );
    assert!(
        texts.iter().any(|t| t.contains("inside the target")),
        "the target's text: {texts:?}"
    );
    assert!(
        !texts.iter().any(|t| t.trim().is_empty()),
        "whitespace-only text is dropped: {texts:?}"
    );

    // ── boxes: viewport CSS px of the live layout, and honest absences ──────
    // CSS geometry conformance is the tab crate's business; what the pane needs
    // is that these numbers are the engine's *own* layout — per element, in
    // viewport CSS px, finite — and that a point taken from a box names that
    // element back, which is the next section. `<head>` and `display:none` show
    // that an absent box is reported as absent rather than invented.
    let box_of = |n: &InspectNode| n.bounds.expect("a laid-out box");
    let target_box = box_of(target);
    let root_box = box_of(find_tag(&snap, "html").expect("<html>"));
    assert!(
        target_box.width.is_finite()
            && target_box.height.is_finite()
            && target_box.x.is_finite()
            && target_box.y.is_finite(),
        "boxes are finite: {target_box:?}"
    );
    assert!(
        (target_box.width - 120.0).abs() <= 1.0 && (target_box.height - 80.0).abs() <= 1.0,
        "#target's box is its own declared 120×80: {target_box:?}"
    );
    assert!(
        target_box.x >= 0.0 && target_box.y >= 0.0,
        "an element at the top of an unscrolled page is not off-screen: {target_box:?}"
    );
    assert_ne!(
        (target_box.x, target_box.y),
        (root_box.x, root_box.y),
        "the box is the element's own, not a copy of the viewport root: {root_box:?}"
    );
    assert!(root_box.width > 0.0, "the document root spans the viewport: {root_box:?}");
    // `<head>` (never laid out, its children included) and `display:none` are in
    // the tree with no box.
    assert!(
        find_tag(&snap, "head").expect("<head>").bounds.is_none(),
        "a never-laid-out subtree has no box"
    );
    assert!(
        find_tag(&snap, "title")
            .expect("<title>")
            .bounds
            .is_none(),
        "neither does anything under `<head>`"
    );
    assert!(
        find_id(&snap, "hidden")
            .map(|n| n.bounds.is_none())
            .unwrap_or(false),
        "display:none has no box"
    );
    assert!(
        box_of(find_id(&snap, "main").expect("#main")).width > 0.0,
        "a laid-out sibling does have one"
    );

    // ── ⌖: a point from the tab's own snapshot names that node back ─────────
    let centre = (
        target_box.x + target_box.width / 2.0,
        target_box.y + target_box.height / 2.0,
    );
    proc.send(ToTab::InspectPickAt { x: centre.0, y: centre.1 }).expect("send to the tab");
    wait_for("the pick names #target", &mut pump, &mut feed, |f| {
        !f.picks.is_empty()
    });
    let picked = *feed.picks.last().expect("InspectPicked");
    assert_eq!(
        picked, target.id,
        "the centre of #target's reported box resolves to #target"
    );
    let with_selection = feed.latest().expect("the snapshot answering the pick");
    assert_eq!(
        with_selection.selected,
        Some(picked),
        "the same generation carries the new selection"
    );
    assert_eq!(
        with_selection.generation, snap.generation,
        "a pick is not a new document"
    );
    assert!(
        with_selection
            .nodes
            .iter()
            .any(|n| n.id == picked && n.kind == InspectKind::Element),
        "the picked id is an element of this tree"
    );

    // ── the crosshair reports the cursor, and stops when disarmed ───────────
    proc.send(ToTab::SetInspectPick { enabled: true }).expect("send to the tab");
    proc.send(ToTab::Pointer(PointerEvent {
        x: centre.0,
        y: centre.1,
        kind: PointerKind::Move,
        button: MouseButton::Left,
    }))
    .expect("send the hover the armed crosshair needs");
    wait_for("the hover report reaches the pane", &mut pump, &mut feed, |f| {
        f.hovers.iter().rev().any(|h| *h == Some(target.id))
    });
    proc.send(ToTab::SetInspectPick { enabled: false }).expect("send to the tab");
    wait_for("disarm takes the hover away", &mut pump, &mut feed, |f| {
        f.hovers.last().copied() == Some(None)
    });

    // ── selection: this generation is accepted, a dead one is ignored ───────
    proc.send(ToTab::InspectSelect { node: Some(main.id) }).expect("send to the tab");
    // A row id from a generation this tab has thrown away: "ignored, never an
    // error" — the tab keeps streaming and stays alive.
    proc.send(ToTab::InspectSelect { node: Some(u64::MAX) }).expect("send to the tab");
    std::thread::sleep(Duration::from_millis(250));
    for (_, msg) in pump.drain() {
        feed.absorb(msg);
    }
    assert!(
        proc.try_wait_status().is_none(),
        "the tab survived a stale row id: {}",
        proc.stderr_tail()
    );

    // ── closing the pane stops the walk, and nothing resumes it ─────────────
    proc.send(ToTab::SetInspect { open: false }).expect("send to the tab");
    let after_close = feed.snapshots.len();
    // Frames keep coming (the tab is still painting the page it was told to
    // show); snapshots must not. Poll for two seconds of real traffic rather
    // than trusting a sleep: what matters is that the walk stayed off.
    let frames_before = feed.frames;
    let quiet_until = Instant::now() + Duration::from_secs(2);
    while Instant::now() < quiet_until {
        for (_, msg) in pump.drain() {
            feed.absorb(msg);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        feed.frames > frames_before,
        "the tab kept painting after the pane closed ({} frames seen): {}",
        feed.frames,
        feed.dump()
    );
    assert_eq!(
        feed.snapshots.len(),
        after_close,
        "a closed pane is not owed a document: {}",
        feed.dump()
    );

    proc.destroy();
    let status = proc.child_mut().wait().expect("wait for tab exit");
    assert_eq!(status.code(), Some(0), "stderr: {}", proc.stderr_tail());
}

/// A compact `(kind, tag, depth)` view for failure messages.
fn shape(snap: &InspectSnapshot) -> Vec<(&'static str, &str, usize)> {
    snap.nodes
        .iter()
        .map(|n| {
            (
                match n.kind {
                    InspectKind::Element => "element",
                    InspectKind::Text => "text",
                    InspectKind::Comment => "comment",
                    InspectKind::Doctype => "doctype",
                    InspectKind::Document => "document",
                },
                n.tag.as_str(),
                n.depth,
            )
        })
        .collect()
}
