//! Per-container proxy: `containers.json` round-trips, clearing,
//! per-container independence, blank-means-direct, dangling names, and
//! validation. Mirrors `tests/user_agent.rs` — the proxy field follows the
//! exact same registry contract, storing a definition **name**, not a URL.

use ironsurf_common::MAX_PROXY_NAME_CHARS;
use ironsurf_storage::{Store, StoreError};

fn open(root: &std::path::Path) -> Store {
    Store::open(root).expect("open store")
}

fn proxy(store: &Store, id: ironsurf_common::ContainerId) -> Option<String> {
    store
        .container(id)
        .expect("container exists")
        .proxy
        .clone()
}

#[test]
fn set_proxy_round_trips_and_survives_reopen() {
    let tmp = tempfile::tempdir().unwrap();
    let mut store = open(tmp.path());
    let id = store.default_container_id();
    assert_eq!(proxy(&store, id), None, "a fresh container connects direct");

    store.set_container_proxy(id, Some("work-socks")).unwrap();
    assert_eq!(proxy(&store, id).as_deref(), Some("work-socks"));

    drop(store);
    assert_eq!(proxy(&open(tmp.path()), id).as_deref(), Some("work-socks"));
}

#[test]
fn none_clears_the_choice_on_disk() {
    let tmp = tempfile::tempdir().unwrap();
    let mut store = open(tmp.path());
    let id = store.default_container_id();

    store.set_container_proxy(id, Some("work-socks")).unwrap();
    store.set_container_proxy(id, None).unwrap();
    assert_eq!(proxy(&store, id), None);

    drop(store);
    assert_eq!(proxy(&open(tmp.path()), id), None);
}

#[test]
fn blank_input_stores_none() {
    let tmp = tempfile::tempdir().unwrap();
    let mut store = open(tmp.path());
    let id = store.default_container_id();

    store.set_container_proxy(id, Some("work-socks")).unwrap();
    store.set_container_proxy(id, Some("   ")).unwrap();
    assert_eq!(proxy(&store, id), None, "all-blank means direct");
    // A tab inside a name is a control character — rejected, like the UA.
    let err = store
        .set_container_proxy(id, Some("bad\tname"))
        .unwrap_err();
    assert!(matches!(err, StoreError::InvalidProxyName), "{err}");

    // Surrounding blanks are trimmed, not rejected.
    store.set_container_proxy(id, Some("  vpn  ")).unwrap();
    assert_eq!(proxy(&store, id).as_deref(), Some("vpn"));
}

#[test]
fn choices_are_per_container() {
    let tmp = tempfile::tempdir().unwrap();
    let mut store = open(tmp.path());
    let a = store.default_container_id();
    let b = store.create_container("work", "#ff0000").unwrap();

    store.set_container_proxy(a, Some("home-socks")).unwrap();
    store.set_container_proxy(b, Some("work-http")).unwrap();
    assert_eq!(proxy(&store, a).as_deref(), Some("home-socks"));
    assert_eq!(proxy(&store, b).as_deref(), Some("work-http"));

    store.set_container_proxy(b, None).unwrap();
    assert_eq!(proxy(&store, a).as_deref(), Some("home-socks"), "untouched");
    assert_eq!(proxy(&store, b), None);
}

#[test]
fn a_dangling_name_is_stored_as_is() {
    // The definitions live elsewhere (ui.json) and may be deleted later;
    // the registry keeps the name and the shell resolves it to direct.
    let tmp = tempfile::tempdir().unwrap();
    let mut store = open(tmp.path());
    let id = store.default_container_id();
    store.set_container_proxy(id, Some("gone")).unwrap();
    assert_eq!(proxy(&store, id).as_deref(), Some("gone"));
}

#[test]
fn validation_rejects_and_changes_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let mut store = open(tmp.path());
    let id = store.default_container_id();
    store.set_container_proxy(id, Some("keep-me")).unwrap();

    let err = store
        .set_container_proxy(id, Some("bad\tname"))
        .unwrap_err();
    assert!(matches!(err, StoreError::InvalidProxyName), "{err}");

    let too_long = "p".repeat(MAX_PROXY_NAME_CHARS + 1);
    let err = store.set_container_proxy(id, Some(&too_long)).unwrap_err();
    assert!(
        matches!(err, StoreError::ProxyNameTooLong { length, max } if length == MAX_PROXY_NAME_CHARS + 1 && max == MAX_PROXY_NAME_CHARS),
        "{err}"
    );

    // Rejected calls left both the memory and the file untouched.
    assert_eq!(proxy(&store, id).as_deref(), Some("keep-me"));
    drop(store);
    assert_eq!(proxy(&open(tmp.path()), id).as_deref(), Some("keep-me"));

    // The boundary itself is accepted.
    let exact = "p".repeat(MAX_PROXY_NAME_CHARS);
    open(tmp.path());
    let mut store = open(tmp.path());
    store.set_container_proxy(id, Some(&exact)).unwrap();
    assert_eq!(proxy(&store, id).as_deref(), Some(exact.as_str()));
}

#[test]
fn unknown_container_is_no_container() {
    let tmp = tempfile::tempdir().unwrap();
    let mut store = open(tmp.path());
    let err = store
        .set_container_proxy(ironsurf_common::ContainerId(999), Some("x"))
        .unwrap_err();
    assert!(matches!(err, StoreError::NoSuchContainer(_)), "{err}");
}

#[test]
fn legacy_registry_loads_with_proxy_none() {
    let tmp = tempfile::tempdir().unwrap();
    // A containers.json written before the proxy field existed.
    std::fs::write(
        tmp.path().join("containers.json"),
        r##"[{"id":7,"name":"old","color":"#ff5500","is_default":true,"user_agent":null}]"##,
    )
    .unwrap();
    let store = open(tmp.path());
    let id = ironsurf_common::ContainerId(7);
    assert_eq!(proxy(&store, id), None);
    // And it can be set after the upgrade.
    let mut store = store;
    store.set_container_proxy(id, Some("vpn")).unwrap();
    assert_eq!(proxy(&store, id).as_deref(), Some("vpn"));
}
