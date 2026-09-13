//! Headless pump integration test: the real [`TabProc`]/[`Pump`] plumbing
//! against the scripted `fake_tab` child process (same framing as the real
//! `servfox-tab`, no engine, no X). Lives in `tests/` because
//! `CARGO_BIN_EXE_fake_tab` is only defined for integration-test targets.

use std::path::Path;

use servfox_common::{ContainerId, FromTab, TabId, ToTab};
use servfox_shell::tabs::{Pump, PumpedMessage, TabProc};

/// End-to-end pump against the scripted fake tab: Ready arrives,
/// Navigate echoes back a UrlChanged, Destroy exits the child with 0.
#[test]
fn pump_with_fake_tab_process() {
    let dir = tempfile::tempdir().unwrap();
    let bin = env!("CARGO_BIN_EXE_fake_tab");
    let mut pump = Pump::new();

    let proc = TabProc::spawn(
        Path::new(bin),
        TabId(42),
        ContainerId(1),
        dir.path(),
        Some("https://example.com"),
        &[],
        &pump.sender(),
    )
    .unwrap();

    // Wait for the scripted preamble.
    let msgs = wait_until(&mut pump, |m| {
        m.iter().any(|(_, m)| matches!(m, FromTab::Ready { .. }))
            && m.iter().any(|(_, m)| matches!(m, FromTab::UrlChanged { .. }))
    });
    assert!(msgs
        .iter()
        .any(|(id, m)| *id == TabId(42) && matches!(m, FromTab::Ready { tab_id, .. } if *tab_id == TabId(42))));
    assert!(msgs
        .iter()
        .any(|(_, m)| matches!(m, FromTab::UrlChanged { url } if url.as_deref() == Some("https://example.com"))));
    assert!(msgs
        .iter()
        .any(|(_, m)| matches!(m, FromTab::TitleChanged { title } if title.as_deref() == Some("Fake — https://example.com"))));

    // Navigate must be answered with a UrlChanged for the new URL.
    proc.send(ToTab::Navigate { url: "about:blank".into() }).unwrap();
    let msgs = wait_until(&mut pump, |m| {
        m.iter().any(|(_, m)| {
            matches!(m, FromTab::UrlChanged { url } if url.as_deref() == Some("about:blank"))
        })
    });
    assert!(!msgs.is_empty());

    // Clean shutdown: Destroy => process exits 0.
    let mut proc = proc;
    proc.destroy();
    let status = proc.child_mut().wait().unwrap();
    assert_eq!(status.code(), Some(0));
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
