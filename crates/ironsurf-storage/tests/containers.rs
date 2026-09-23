//! Container registry lifecycle + cross-container isolation tests.

use ironsurf_common::ContainerId;
use ironsurf_storage::{Store, StoreError};
use url::Url;

fn open(root: &std::path::Path) -> Store {
    Store::open(root).expect("open store")
}

#[test]
fn open_creates_default_personal_container() {
    let tmp = tempfile::tempdir().unwrap();
    let store = open(tmp.path());

    assert_eq!(store.containers().len(), 1);
    let def_id = store.default_container_id();
    let def = store.container(def_id).unwrap();
    assert_eq!(def.name, "personal");
    assert!(def.is_default);
    assert!(tmp.path().join("containers.json").exists());

    // Profile dir exists (slug + id) with the reserved subdirs.
    let dir = store.profile_dir(def_id);
    assert!(dir.is_dir());
    assert!(dir.join("net-cache").is_dir());
    assert!(dir.join("local-storage").is_dir());
    assert!(dir
        .file_name()
        .unwrap()
        .to_string_lossy()
        .ends_with(&format!("-{}", def_id.0)));

    // Re-open is stable: still exactly one container.
    drop(store);
    let store = open(tmp.path());
    assert_eq!(store.containers().len(), 1);
    assert_eq!(store.default_container_id(), def_id);
}

#[test]
fn create_rename_recolor_delete_lifecycle() {
    let tmp = tempfile::tempdir().unwrap();
    let mut store = open(tmp.path());

    let id = store.create_container("Work", "#FF0000").unwrap();
    assert_ne!(id, store.default_container_id());
    let c = store.container(id).unwrap();
    assert_eq!(c.name, "Work");
    assert_eq!(c.color, "#ff0000"); // normalized lowercase
    let dir = store.profile_dir(id);
    assert!(dir.is_dir());

    store.rename_container(id, "Personal 2").unwrap();
    assert_eq!(store.container(id).unwrap().name, "Personal 2");

    store.set_container_color(id, "#00ff7f").unwrap();
    assert_eq!(store.container(id).unwrap().color, "#00ff7f");

    // Bogus color falls back to neutral gray.
    store.set_container_color(id, "not-a-color").unwrap();
    assert_eq!(store.container(id).unwrap().color, "#9d9ba4");

    // Unknown ids error out.
    assert!(matches!(
        store.rename_container(ContainerId(999), "x"),
        Err(StoreError::NoSuchContainer(_))
    ));
    assert!(matches!(
        store.create_container("  ", "#ffffff"),
        Err(StoreError::EmptyName)
    ));

    // Persistence: a fresh Store sees the mutations.
    drop(store);
    let store = open(tmp.path());
    assert_eq!(store.containers().len(), 2);
    assert_eq!(store.container(id).unwrap().name, "Personal 2");

    let mut store = store;
    // Deleting the default is refused.
    assert!(matches!(
        store.delete_container(store.default_container_id()),
        Err(StoreError::CannotDeleteDefault)
    ));

    // Delete removes the entry and wipes the profile dir.
    store.delete_container(id).unwrap();
    assert!(store.container(id).is_none());
    assert!(!dir.exists());
    assert_eq!(store.containers().len(), 1);
}

#[test]
fn rename_carries_profile_data_to_new_slug_dir() {
    use ironsurf_storage::ContainerStore;
    use url::Url;

    let tmp = tempfile::tempdir().unwrap();
    let mut store = open(tmp.path());
    let id = store.create_container("Old Name", "#ff0000").unwrap();
    let old_dir = store.profile_dir(id);

    let ca = ContainerStore::open(&store, id);
    let mut jar = ca.cookie_jar();
    jar.set_from_set_cookie(&Url::parse("https://example.com/").unwrap(), "k=v; Path=/", 1);
    jar.save().unwrap();
    assert!(old_dir.join("cookies.json").exists());

    store.rename_container(id, "New Name").unwrap();
    let new_dir = store.profile_dir(id);
    assert_ne!(old_dir, new_dir);
    assert!(!old_dir.exists(), "old slug dir should be gone after rename");
    // Data followed the container; the container store can still read it.
    let jar = ContainerStore::open(&store, id).cookie_jar();
    assert_eq!(jar.cookie_header(&Url::parse("https://example.com/").unwrap(), 1).as_deref(), Some("k=v"));
}

#[test]
fn profile_dirs_are_distinct_per_container() {
    let tmp = tempfile::tempdir().unwrap();
    let mut store = open(tmp.path());
    let a = store.create_container("acct a", "#ff0000").unwrap();
    let b = store.create_container("acct b", "#00ff00").unwrap();
    let da = store.profile_dir(a);
    let db = store.profile_dir(b);
    assert_ne!(da, db);
    // Slug names differ and each embeds the container id.
    assert!(da.to_string_lossy().contains("acct-a-"));
    assert!(db.to_string_lossy().contains("acct-b-"));
}

/// The headline isolation test: cookies written in container A are invisible
/// to container B (separate jars, separate dirs on disk).
#[test]
fn cookie_jars_are_isolated_between_containers() {
    use ironsurf_storage::ContainerStore;

    let tmp = tempfile::tempdir().unwrap();
    let mut store = open(tmp.path());
    let a = store.create_container("A", "#ff0000").unwrap();
    let b = store.create_container("B", "#00ff00").unwrap();
    let url = Url::parse("https://example.com/feed").unwrap();

    let ca = ContainerStore::open(&store, a);
    let cb = ContainerStore::open(&store, b);
    assert_ne!(ca.profile_dir(), cb.profile_dir());

    let mut jar_a = ca.cookie_jar();
    jar_a.set_from_set_cookie(&url, "who=A; Path=/; Secure", 1_000);
    jar_a.save().unwrap();

    // A fresh jar for A sees its cookie; a fresh jar for B does not.
    let jar_a = ca.cookie_jar();
    let jar_b = cb.cookie_jar();
    assert_eq!(jar_a.cookie_header(&url, 1_000).as_deref(), Some("who=A"));
    assert_eq!(jar_b.cookie_header(&url, 1_000), None);
    assert!(jar_b.cookies().is_empty());

    // On-disk state is separate too.
    let a_file = ca.profile_dir().join("cookies.json");
    let b_file = cb.profile_dir().join("cookies.json");
    assert!(a_file.exists());
    assert!(a_file
        .to_string_lossy()
        .contains(&ca.profile_dir().display().to_string()));
    let a_text = std::fs::read_to_string(&a_file).unwrap();
    assert!(a_text.contains("who") && !a_text.contains("\"B\""));
    assert!(!b_file.exists() || !std::fs::read_to_string(&b_file).unwrap().contains("who"));

    // B writes its own cookie; A never sees it.
    let mut jar_b = cb.cookie_jar();
    jar_b.set_from_set_cookie(&url, "who=B; Path=/", 1_000);
    jar_b.save().unwrap();
    assert_eq!(cb.cookie_jar().cookie_header(&url, 1_000).as_deref(), Some("who=B"));
    assert_eq!(ca.cookie_jar().cookie_header(&url, 1_000).as_deref(), Some("who=A"));
}

// Browsing history is no longer per container (task: browsing history): it
// lives at the data root, shared by every container. The old assertion that
// each profile kept its own `history.jsonl` is replaced by the reverse —
// profile dirs never hold history any more; see tests/history.rs for the
// shared log itself.
#[test]
fn profiles_hold_no_history() {
    use ironsurf_storage::ContainerStore;

    let tmp = tempfile::tempdir().unwrap();
    let mut store = open(tmp.path());
    let a = store.create_container("A", "#ff0000").unwrap();
    let b = store.default_container_id();
    ironsurf_storage::History::open(tmp.path())
        .record("https://example.com/one", Some("One"))
        .unwrap();
    for id in [a, b] {
        let cs = ContainerStore::open(&store, id);
        assert!(
            !cs.profile_dir().join("history.jsonl").exists(),
            "history must not live under profiles/ any more"
        );
    }
    assert!(tmp.path().join("history.jsonl").exists());
}
