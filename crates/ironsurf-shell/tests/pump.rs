//! Headless pump integration test: the real [`TabProc`]/[`Pump`] plumbing
//! against the scripted `fake_tab` child process (same framing as the real
//! `ironsurf-tab`, no engine, no X). Lives in `tests/` because
//! `CARGO_BIN_EXE_fake_tab` is only defined for integration-test targets.

use std::path::Path;

use ironsurf_common::{ContainerId, FromTab, TabId, ToTab};
use ironsurf_shell::tabs::{Pump, PumpedMessage, TabProc};

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

/// A tab child that dies (panics) without saying `Closed`: the shell must
/// notice via `try_wait_status`, capture the stderr (panic + backtrace) it
/// would otherwise `/dev/null`, and the dead stdin must fail loudly — never
/// silently swallow commands destined for the corpse.
#[test]
fn crashed_tab_reports_exit_status_and_backtrace() {
    let dir = tempfile::tempdir().unwrap();
    let bin = env!("CARGO_BIN_EXE_fake_tab");
    let mut pump = Pump::new();

    let mut proc = TabProc::spawn(
        Path::new(bin),
        TabId(7),
        ContainerId(1),
        dir.path(),
        Some("about:blank"),
        &["--crash"],
        &pump.sender(),
    )
    .unwrap();

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);

    // 1. The broken pipe the user reported: commands keep working while the
    //    child is alive and must fail with EPIPE the moment it is gone —
    //    exactly what `Core::send_active` turns into a “Tab N has crashed”
    //    status line (while stdin is still open, hence polled first).
    let err = loop {
        match proc.send(ToTab::Reload) {
            Ok(()) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            Ok(()) => panic!("the crashing tab never refused a command"),
            Err(err) => break err,
        }
    };
    assert_eq!(err.raw_os_error(), Some(32), "expected EPIPE (broken pipe), got {err}");

    // 2. The crash poll `Core::check_crashes` runs every drain tick.
    let status = loop {
        if let Some(status) = proc.try_wait_status() {
            break status;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the crashing tab never exited"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    };
    assert_eq!(status.code(), Some(101));

    // 3. The captured stderr tail carries the panic message and the backtrace
    //    (the crash report the shell prints instead of letting tabs die quiet).
    let tail = wait_for_stderr_tail(&proc);
    assert!(tail.contains("panicked"), "no panic line in captured stderr: {tail:?}");
    assert!(tail.contains("backtrace"), "no backtrace in captured stderr: {tail:?}");
}

/// The stderr pump thread may lag the exit slightly — poll the tail buffer.
fn wait_for_stderr_tail(proc: &TabProc) -> String {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let tail = proc.stderr_tail();
        if !tail.is_empty() || std::time::Instant::now() > deadline {
            return tail;
        }
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
