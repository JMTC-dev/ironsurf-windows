//! Hover cursor on the wire (task: cursor icon): the scripted `fake_tab`
//! answers pointer moves over its fake page with `FromTab::CursorChanged`
//! — the pointing hand at x ≥ 100, the plain arrow elsewhere and on `Out`,
//! reported only on change, exactly like the real tab derives it from blitz.
//! The pump must carry every transition through, in order, and the change
//! detection must keep identical shapes silent.

use std::path::Path;

use ironsurf_common::{
    ContainerId, CursorIcon, FromTab, MouseButton, PointerEvent, PointerKind, TabId, ToTab,
};
use ironsurf_shell::tabs::{Pump, PumpedMessage, TabProc};

#[test]
fn fake_tab_reports_the_cursor_only_on_change() {
    let dir = tempfile::tempdir().unwrap();
    let bin = env!("CARGO_BIN_EXE_fake_tab");
    let mut pump = Pump::new();

    let mut proc = TabProc::spawn(
        Path::new(bin),
        TabId(3),
        ContainerId(1),
        dir.path(),
        Some("https://example.com"),
        &["--cursor"],
        &pump.sender(),
    )
    .unwrap();

    // The scripted preamble must arrive before anything is answered.
    let msgs = wait_until(&mut pump, |m| {
        m.iter().any(|(_, m)| matches!(m, FromTab::Ready { .. }))
    });
    assert!(!msgs.is_empty(), "fake_tab never said Ready");

    let pointer = |x: f32, kind: PointerKind| {
        proc.send(ToTab::Pointer(PointerEvent { x, y: 20.0, kind, button: MouseButton::Left }))
            .unwrap()
    };

    // Two moves inside the fake "link" (x ≥ 100) ask for the hand once…
    pointer(150.0, PointerKind::Move);
    pointer(160.0, PointerKind::Move);
    // …and two moves off it (plus leaving the page) want the arrow once.
    pointer(50.0, PointerKind::Move);
    pointer(60.0, PointerKind::Move);
    pointer(60.0, PointerKind::Out);

    // The feed is exactly the transitions, in page order.
    let feed = cursor_feed_until(&mut pump, &[CursorIcon::Pointer, CursorIcon::Default]);
    assert_eq!(feed, vec![CursorIcon::Pointer, CursorIcon::Default]);

    // …and a quiet wire afterwards: the change detection swallowed every
    // same-shape move (none of the five moves may add anything more).
    std::thread::sleep(std::time::Duration::from_millis(250));
    let stragglers: Vec<CursorIcon> = pump
        .drain()
        .into_iter()
        .filter_map(|(_, m)| match m {
            FromTab::CursorChanged { cursor } => Some(cursor),
            _ => None,
        })
        .collect();
    assert!(stragglers.is_empty(), "unchanged shapes must stay silent: {stragglers:?}");

    proc.destroy();
    let status = proc.child_mut().wait().unwrap();
    assert_eq!(status.code(), Some(0));
}

/// Collects `CursorChanged` shapes off the pump until the feed equals
/// `expected` (in order and no further along).
fn cursor_feed_until(pump: &mut Pump, expected: &[CursorIcon]) -> Vec<CursorIcon> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let mut feed = Vec::new();
    loop {
        for (_, msg) in pump.drain() {
            if let FromTab::CursorChanged { cursor } = msg {
                feed.push(cursor);
            }
        }
        assert!(feed.starts_with(expected) || expected.starts_with(feed.as_slice()), "bad feed order: {feed:?}");
        if feed == expected {
            return feed;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "cursor feed {feed:?} never reached {expected:?}"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

fn wait_until(pump: &mut Pump, pred: impl Fn(&[PumpedMessage]) -> bool) -> Vec<PumpedMessage> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let mut acc = Vec::new();
    loop {
        acc.extend(pump.drain());
        if pred(&acc) {
            return acc;
        }
        if std::time::Instant::now() > deadline {
            panic!("timed out waiting for message; got: {acc:?}");
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}
