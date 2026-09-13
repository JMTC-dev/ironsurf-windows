//! RFC6265-subset cookie rules: Secure-over-https-only (set *and* send),
//! domain-suffix / host-only matching, path-prefix matching, expiry pruning.

use ironsurf_storage::CookieJar;
use url::Url;

const T: i64 = 1_700_000_000; // a fixed "now"

fn url(s: &str) -> Url {
    Url::parse(s).unwrap()
}

fn jar_with(set_cookie: &str, request: &str) -> CookieJar {
    let mut jar = CookieJar::new();
    jar.set_from_set_cookie(&url(request), set_cookie, T);
    jar
}

#[test]
fn secure_cookie_never_sent_over_http() {
    let jar = jar_with("s=1; Path=/; Secure", "https://example.com/");
    assert_eq!(jar.cookies().len(), 1);
    assert!(jar.cookies()[0].secure);
    // https: sees it...
    assert_eq!(
        jar.cookie_header(&url("https://example.com/"), T).as_deref(),
        Some("s=1")
    );
    // http: must not, even on the exact same host/path.
    assert_eq!(jar.cookie_header(&url("http://example.com/"), T), None);
    assert_eq!(jar.cookie_header(&url("http://example.com/x"), T), None);
}

#[test]
fn set_cookie_with_secure_over_http_is_rejected() {
    let jar = jar_with("evil=1; Path=/; Secure", "http://example.com/");
    assert!(jar.cookies().is_empty(), "Secure cookie must not be set over http");
    // Not even when queried over https later.
    assert_eq!(jar.cookie_header(&url("https://example.com/"), T), None);
}

#[test]
fn non_secure_cookie_sent_over_both() {
    let jar = jar_with("p=1; Path=/", "https://example.com/");
    assert!(jar.cookie_header(&url("http://example.com/"), T).is_some());
    assert!(jar.cookie_header(&url("https://example.com/"), T).is_some());
}

#[test]
fn domain_attribute_suffix_matching() {
    // Domain=example.com set from www.example.com.
    let jar = jar_with(
        "d=1; Domain=example.com; Path=/; Secure",
        "https://www.example.com/",
    );
    assert_eq!(jar.cookies().len(), 1);
    assert_eq!(jar.cookies()[0].domain, ".example.com");
    assert!(jar.cookie_header(&url("https://example.com/"), T).is_some());
    assert!(jar.cookie_header(&url("https://www.example.com/"), T).is_some());
    assert!(jar.cookie_header(&url("https://deep.sub.example.com/"), T).is_some());
    // Non-matching hosts never see it — including lookalike suffixes.
    assert_eq!(jar.cookie_header(&url("https://other.com/"), T), None);
    assert_eq!(jar.cookie_header(&url("https://notexample.com/"), T), None);
}

#[test]
fn host_only_cookie_matches_exact_host_only() {
    // No Domain attribute => host-only, exact host match.
    let jar = jar_with("h=1; Path=/; Secure", "https://www.example.com/");
    assert_eq!(jar.cookies()[0].domain, "www.example.com");
    assert!(jar.cookie_header(&url("https://www.example.com/"), T).is_some());
    assert_eq!(jar.cookie_header(&url("https://example.com/"), T), None);
    assert_eq!(jar.cookie_header(&url("https://sub.www.example.com/"), T), None);
}

#[test]
fn domain_attribute_must_be_suffix_of_request_host() {
    let jar = jar_with("x=1; Domain=evil.com; Path=/", "https://example.com/");
    assert!(jar.cookies().is_empty());
}

#[test]
fn path_prefix_matching() {
    let jar = jar_with("p=1; Path=/foo; Secure", "https://example.com/");
    assert!(jar.cookie_header(&url("https://example.com/foo"), T).is_some());
    assert!(jar.cookie_header(&url("https://example.com/foo/bar"), T).is_some());
    assert_eq!(jar.cookie_header(&url("https://example.com/"), T), None);
    assert_eq!(jar.cookie_header(&url("https://example.com/foobar"), T), None);
    // Path="/" matches everything.
    let root = jar_with("r=1; Path=/; Secure", "https://example.com/");
    assert!(root.cookie_header(&url("https://example.com/any/thing"), T).is_some());
}

#[test]
fn default_cookie_path_is_request_directory() {
    // Set from /a/b/c with no Path= => cookie path "/a/b/".
    let jar = jar_with("d=1; Secure", "https://example.com/a/b/c");
    assert_eq!(jar.cookies()[0].path, "/a/b/");
    assert!(jar.cookie_header(&url("https://example.com/a/b/"), T).is_some());
    assert!(jar.cookie_header(&url("https://example.com/a/b/c"), T).is_some());
    assert_eq!(jar.cookie_header(&url("https://example.com/a"), T), None);
}

#[test]
fn expires_prunes_on_access() {
    let jar = jar_with("e=1; Path=/; Secure; Expires=Wed, 09 Jun 3021 10:18:14 GMT", "https://example.com/");
    assert!(jar.cookie_header(&url("https://example.com/"), T).is_some());
    // Way past the (year-3021) expiry: gone.
    assert_eq!(
        jar.cookie_header(&url("https://example.com/"), 999_999_999_999),
        None
    );
}

#[test]
fn already_expired_set_cookie_never_sent() {
    let jar = jar_with("g=1; Path=/; Expires=Wed, 09 Jun 2021 10:18:14 GMT", "https://example.com/");
    // Stored as expired but pruned at any access time after its expiry.
    assert_eq!(jar.cookie_header(&url("https://example.com/"), T + 1), None);
}

#[test]
fn max_age_sets_expiry_and_zero_deletes() {
    let mut jar = jar_with("m=1; Path=/; Secure; Max-Age=60", "https://example.com/");
    assert_eq!(jar.cookies()[0].expires_unix, Some(T + 60));
    assert!(jar.cookie_header(&url("https://example.com/"), T + 30).is_some());
    assert_eq!(jar.cookie_header(&url("https://example.com/"), T + 61), None);

    // Max-Age takes precedence over a later Expires.
    jar.set_from_set_cookie(
        &url("https://example.com/"),
        "m=1; Path=/; Secure; Max-Age=10; Expires=Wed, 09 Jun 3021 10:18:14 GMT",
        T,
    );
    assert_eq!(jar.cookies().len(), 1);
    assert_eq!(jar.cookies()[0].expires_unix, Some(T + 10));

    // Max-Age<=0 deletes the cookie.
    jar.set_from_set_cookie(&url("https://example.com/"), "m=1; Path=/; Max-Age=0", T);
    assert!(jar.cookies().is_empty());
}

#[test]
fn replacement_same_name_domain_path_overwrites() {
    let mut jar = jar_with("v=1; Path=/; Secure", "https://example.com/");
    jar.set_from_set_cookie(&url("https://example.com/"), "v=2; Path=/; Secure", T);
    assert_eq!(jar.cookies().len(), 1);
    assert_eq!(
        jar.cookie_header(&url("https://example.com/"), T).as_deref(),
        Some("v=2")
    );
}

#[test]
fn header_lists_multiple_cookies_longest_path_first() {
    let mut jar = CookieJar::new();
    let base = url("https://example.com/");
    jar.set_from_set_cookie(&base, "a=1; Path=/; Secure", T);
    jar.set_from_set_cookie(&base, "b=2; Path=/deep; Secure", T);
    jar.set_from_set_cookie(&base, "c=3; Path=/; Secure", T);
    let h = jar.cookie_header(&url("https://example.com/deep/page"), T).unwrap();
    assert!(h.contains("b=2"), "{h}");
    assert!(h.contains("a=1") && h.contains("c=3"));
    // Longer cookie path sorts first.
    assert!(h.starts_with("b=2"), "{h}");
    // At root only the two "/" cookies are sent.
    assert_eq!(
        jar.cookie_header(&url("https://example.com/"), T).as_deref(),
        Some("a=1; c=3")
    );
}

#[test]
fn malformed_set_cookie_values_are_ignored() {
    let mut jar = CookieJar::new();
    let base = url("https://example.com/");
    jar.set_from_set_cookie(&base, "", T); // no name
    jar.set_from_set_cookie(&base, "=novame", T); // empty name
    jar.set_from_set_cookie(&base, "noeq", T); // no '=' pair semantics needed here
    assert!(jar.cookies().is_empty());
}

/// End-to-end persistence + isolation through ContainerStore files.
#[test]
fn jar_persists_per_container_file() {
    use ironsurf_storage::{ContainerStore, Store};

    let tmp = tempfile::tempdir().unwrap();
    let mut store = Store::open(tmp.path()).unwrap();
    let a = store.create_container("A", "#ff0000").unwrap();
    let ca = ContainerStore::open(&store, a);

    let mut jar = ca.cookie_jar();
    jar.set_from_set_cookie(&url("https://example.com/"), "keep=1; Path=/; Secure", T);
    jar.save().unwrap();

    let raw_file = ca.profile_dir().join("cookies.json");
    assert!(raw_file.exists());

    // Reload picks the cookie back up...
    let jar = ca.cookie_jar();
    assert_eq!(jar.cookie_header(&url("https://example.com/"), T).as_deref(), Some("keep=1"));

    // ...but an already-expired cookie on disk is pruned at load.
    std::fs::write(
        &raw_file,
        r#"[{"name":"dead","value":"x","domain":"example.com","path":"/","secure":false,"http_only":false,"expires_unix":1}]"#,
    )
    .unwrap();
    let mut jar = ca.cookie_jar();
    assert!(jar.cookies().is_empty());
    jar.clear();
    jar.save().unwrap();
    assert!(ca.cookie_jar().cookies().is_empty());
}
