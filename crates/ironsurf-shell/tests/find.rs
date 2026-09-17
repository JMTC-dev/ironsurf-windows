//! Find-in-page across the pipe (task: the `Ctrl-F` find bar): the shell's
//! three commands and the tab's counter report, driven through the real
//! `TabProc`/`Pump` plumbing against the scripted `fake_tab` child — the same
//! messages the bar sends, with no engine and no X.
//!
//! The fake tab searches its own canned page text (three `page`s in it, and no
//! `zebra`), standing in for the engine — which searches the document's
//! laid-out text and paints the highlights themselves (see `ironsurf-tab`'s
//! `src/find.rs`, where the yellow-everywhere/light-blue-current behaviour is
//! asserted on rendered pixels).

use std::path::Path;

use ironsurf_common::{ContainerId, FromTab, TabId, ToTab};
use ironsurf_shell::state::TabState;
use ironsurf_shell::tabs::{Pump, PumpedMessage, TabProc};

/// The query the fake page answers with three hits.
const HITTING: &str = "page";
/// A query nothing on that page matches.
const MISSING: &str = "zebra";

/// Poll the pump until `pred` holds (or the clock runs out), returning every
/// message seen on the way.
fn wait_until(pump: &mut Pump, pred: impl Fn(&[PumpedMessage]) -> bool) -> Vec<PumpedMessage> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let mut acc = Vec::new();
    loop {
        acc.extend(pump.drain());
        if pred(&acc) {
            return acc;
        }
        if std::time::Instant::now() > deadline {
            panic!("timed out waiting for a FindResult; got: {acc:?}");
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

/// Wait for the tab's next find report — `(query, matches, active)`, the
/// counter the bar shows — reducing everything seen into `state` on the way.
fn next_report(pump: &mut Pump, state: &mut TabState) -> (String, usize, usize) {
    let seen = wait_until(pump, |m| {
        m.iter()
            .any(|(_, msg)| matches!(msg, FromTab::FindResult { .. }))
    });
    let mut last = None;
    for (_, msg) in &seen {
        if matches!(msg, FromTab::FindResult { .. }) {
            state.reduce(msg);
            if let FromTab::FindResult { query, matches, active } = msg {
                last = Some((query.clone(), *matches, *active));
            }
        }
    }
    last.expect("predicate saw a FindResult")
}

/// One round trip per command, in the order the find bar issues them: search →
/// next → previous (wrapping past the first) → a query that matches nothing →
/// an emptied box.
#[test]
fn find_commands_round_trip_and_the_counter_follows() {
    let dir = tempfile::tempdir().unwrap();
    let bin = env!("CARGO_BIN_EXE_fake_tab");
    let mut pump = Pump::new();
    let proc = TabProc::spawn(
        Path::new(bin),
        TabId(7),
        ContainerId(1),
        dir.path(),
        Some("https://example.com"),
        &[],
        &pump.sender(),
    )
    .unwrap();
    let mut state = TabState::default();
    wait_until(&mut pump, |m| {
        m.iter().any(|(_, m)| matches!(m, FromTab::Ready { .. }))
    });

    // Searching reports every hit, with the first one active (1-based), tagged
    // with the query the counts belong to.
    proc.send(ToTab::SetFind { query: HITTING.into() }).unwrap();
    assert_eq!(next_report(&mut pump, &mut state), (HITTING.into(), 3, 1));
    assert_eq!(state.find.label(), "1 / 3");

    // Enter walks forward…
    proc.send(ToTab::FindNext).unwrap();
    assert_eq!(next_report(&mut pump, &mut state), (HITTING.into(), 3, 2));
    assert_eq!(state.find.label(), "2 / 3");

    // …and Shift-Enter backwards, wrapping from the first hit to the last.
    proc.send(ToTab::FindPrevious).unwrap();
    assert_eq!(next_report(&mut pump, &mut state), (HITTING.into(), 3, 1));
    proc.send(ToTab::FindPrevious).unwrap();
    assert_eq!(next_report(&mut pump, &mut state), (HITTING.into(), 3, 3));
    assert_eq!(state.find.label(), "3 / 3");

    // A query that matches nothing still answers — the bar must not sit waiting
    // for a count that is never coming — and it says so plainly.
    proc.send(ToTab::SetFind { query: MISSING.into() }).unwrap();
    assert_eq!(next_report(&mut pump, &mut state), (MISSING.into(), 0, 0));
    assert_eq!(state.find.label(), "No results");
    proc.send(ToTab::FindNext).unwrap();
    assert_eq!(next_report(&mut pump, &mut state), (MISSING.into(), 0, 0));

    // Closing the bar (which empties the query) clears everything.
    proc.send(ToTab::SetFind { query: String::new() }).unwrap();
    assert_eq!(next_report(&mut pump, &mut state), (String::new(), 0, 0));
    assert_eq!(state.find.label(), "");

    let mut proc = proc;
    proc.destroy();
    assert_eq!(proc.child_mut().wait().unwrap().code(), Some(0));
}
