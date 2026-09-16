//! Per-container User-Agent: `containers.json` round-trips, clearing,
//! per-container independence, blank-means-default, and validation.

use ironsurf_common::MAX_USER_AGENT_CHARS;
use ironsurf_storage::{Store, StoreError};

fn open(root: &std::path::Path) -> Store {
    Store::open(root).expect("open store")
}

fn ua(store: &Store, id: ironsurf_common::ContainerId) -> String {
    store
        .container(id)
        .and_then(|c| c.user_agent.clone())
        .expect("container exists")
}

fn registry(root: &std::path::Path) -> String {
    std::fs::read_to_string(root.join("containers.json")).expect("read containers.json")
}

const CUSTOM: &str = "Mozilla/5.0 (X11; Linux x86_64; rv:999.0) Gecko/20100101 IronSurf/1.0";

#[test]
fn set_user_agent_round_trips_and_survives_reopen() {
    let tmp = tempfile::tempdir().unwrap();
    let mut store = open(tmp.path());
    let id = store.default_container_id();
    assert_eq!(store.container(id).unwrap().user_agent, None);

    store.set_container_user_agent(id, Some(CUSTOM)).unwrap();
    assert_eq!(ua(&store, id), CUSTOM);
    assert!(tmp.path().join("containers.json").exists());

    drop(store);
    assert_eq!(ua(&open(tmp.path()), id), CUSTOM);
}

#[test]
fn none_clears_the_override_on_disk() {
    let tmp = tempfile::tempdir().unwrap();
    let mut store = open(tmp.path());
    let id = store.default_container_id();

    store.set_container_user_agent(id, Some(CUSTOM)).unwrap();
    store.set_container_user_agent(id, None).unwrap();
    assert_eq!(store.container(id).unwrap().user_agent, None);

    drop(store);
    assert_eq!(open(tmp.path()).container(id).unwrap().user_agent, None);
}

#[test]
fn overrides_are_per_container() {
    let tmp = tempfile::tempdir().unwrap();
    let mut store = open(tmp.path());
    let a = store.default_container_id();
    let b = store.create_container("work", "#ff0000").unwrap();

    store.set_container_user_agent(a, Some(CUSTOM)).unwrap();
    assert_eq!(ua(&store, a), CUSTOM);
    assert_eq!(store.container(b).unwrap().user_agent, None, "B stays default");

    store.set_container_user_agent(b, Some("bot/1.0")).unwrap();
    assert_eq!(ua(&store, a), CUSTOM, "A is untouched by B's write");

    // Clearing A leaves B alone.
    store.set_container_user_agent(a, None).unwrap();
    assert_eq!(store.container(a).unwrap().user_agent, None);
    assert_eq!(ua(&store, b), "bot/1.0");

    drop(store);
    let store = open(tmp.path());
    assert_eq!(store.container(a).unwrap().user_agent, None);
    assert_eq!(ua(&store, b), "bot/1.0");
}

#[test]
fn values_are_stored_trimmed() {
    let tmp = tempfile::tempdir().unwrap();
    let mut store = open(tmp.path());
    let id = store.default_container_id();

    store
        .set_container_user_agent(id, Some("   \u{3000}Mozilla/5.0 custom \u{00a0} "))
        .unwrap();
    assert_eq!(ua(&store, id), "Mozilla/5.0 custom");
    drop(store);
    assert_eq!(
        open(tmp.path()).container(id).unwrap().user_agent,
        Some("Mozilla/5.0 custom".into())
    );
}

#[test]
fn blank_input_stores_none_meaning_default() {
    let tmp = tempfile::tempdir().unwrap();
    let mut store = open(tmp.path());
    let id = store.default_container_id();

    store.set_container_user_agent(id, Some(CUSTOM)).unwrap();
    for blank in ["", "   ", "\u{00a0}\u{3000}"] {
        store.set_container_user_agent(id, Some(blank)).unwrap();
        assert_eq!(
            store.container(id).unwrap().user_agent,
            None,
            "{blank:?} means the built-in default"
        );
    }

    drop(store);
    assert_eq!(open(tmp.path()).container(id).unwrap().user_agent, None);
}

#[test]
fn too_long_is_rejected_without_touching_state_or_file() {
    let tmp = tempfile::tempdir().unwrap();
    let mut store = open(tmp.path());
    let id = store.default_container_id();
    store.set_container_user_agent(id, Some(CUSTOM)).unwrap();
    let before = registry(tmp.path());

    let huge = "u".repeat(MAX_USER_AGENT_CHARS + 1);
    assert!(matches!(
        store.set_container_user_agent(id, Some(&huge)),
        Err(StoreError::UserAgentTooLong { length, max })
            if length == MAX_USER_AGENT_CHARS + 1 && max == MAX_USER_AGENT_CHARS
    ));
    assert_eq!(ua(&store, id), CUSTOM, "rejected write leaves memory intact");
    assert_eq!(registry(tmp.path()), before, "rejected write leaves the file intact");

    // Boundary: exactly at the cap is accepted.
    let at_cap = "u".repeat(MAX_USER_AGENT_CHARS);
    store.set_container_user_agent(id, Some(&at_cap)).unwrap();
    assert_eq!(ua(&store, id), at_cap);
}

#[test]
fn control_characters_are_rejected() {
    let tmp = tempfile::tempdir().unwrap();
    let mut store = open(tmp.path());
    let id = store.default_container_id();
    store.set_container_user_agent(id, Some(CUSTOM)).unwrap();
    let before = registry(tmp.path());

    for bad in ["Mozilla\r\nGET /evil", "ua\u{7}bell", "line1\nline2", "\u{1}hidden"] {
        assert!(
            matches!(
                store.set_container_user_agent(id, Some(bad)),
                Err(StoreError::InvalidUserAgent)
            ),
            "{bad:?} must be rejected"
        );
    }
    assert_eq!(ua(&store, id), CUSTOM);
    assert_eq!(registry(tmp.path()), before);
}

#[test]
fn unknown_container_id_errors() {
    let tmp = tempfile::tempdir().unwrap();
    let mut store = open(tmp.path());
    let bogus = ironsurf_common::ContainerId(999);
    assert!(matches!(
        store.set_container_user_agent(bogus, Some(CUSTOM)),
        Err(StoreError::NoSuchContainer(ironsurf_common::ContainerId(999)))
    ));
    assert!(matches!(
        store.set_container_user_agent(bogus, None),
        Err(StoreError::NoSuchContainer(_))
    ));
    // Nothing was written for a container that does not exist.
    drop(store);
    let store = open(tmp.path());
    assert!(store.container(bogus).is_none());
}
