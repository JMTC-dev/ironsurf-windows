//! Search-or-navigate classification for the URL bar.
//!
//! Rule (per ARCHITECTURE.md §servfox-shell): a typed value whose host part
//! contains a dot becomes an `https://` URL; everything else becomes a
//! DuckDuckGo search. Explicit schemes pass through untouched.

/// Typed text → a URL to navigate to (already absolute).
pub fn classify_input(input: &str) -> Option<String> {
    let input = input.trim();
    if input.is_empty() {
        return None;
    }

    // explicit scheme → pass through (the tab applies its own policy)
    for scheme in ["http://", "https://", "about:", "file://", "data:"] {
        if input.starts_with(scheme) {
            return Some(input.to_string());
        }
    }

    // any whitespace means this can only be a search
    if input.chars().any(char::is_whitespace) {
        return Some(search_url(input));
    }

    // `host` / `host:port` / `host/path` with a dot in the authority → https
    let host_end = input
        .find(['/', '?', '#'])
        .unwrap_or(input.len());
    let host = &input[..host_end];
    if host.contains('.') {
        return Some(format!("https://{input}"));
    }

    // An already-absolute URL the schemes above did not cover (odd schemes
    // fall through to the tab); otherwise: search.
    match url::Url::parse(input) {
        Ok(u) if u.has_host() => Some(u.into()),
        _ => Some(search_url(input)),
    }
}

fn search_url(query: &str) -> String {
    format!(
        "https://duckduckgo.com/?q={}",
        url::form_urlencoded::byte_serialize(query.as_bytes()).collect::<String>()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typed_host_with_dot_upgrades_to_https() {
        assert_eq!(
            classify_input("example.com").as_deref(),
            Some("https://example.com")
        );
        assert_eq!(
            classify_input("  example.com/with/path  ").as_deref(),
            Some("https://example.com/with/path")
        );
        assert_eq!(
            classify_input("example.com:8443/x").as_deref(),
            Some("https://example.com:8443/x")
        );
    }

    #[test]
    fn explicit_schemes_pass_through() {
        assert_eq!(
            classify_input("http://example.com").as_deref(),
            Some("http://example.com")
        );
        assert_eq!(classify_input("about:blank").as_deref(), Some("about:blank"));
        assert_eq!(
            classify_input("https://example.com/a?b=1").as_deref(),
            Some("https://example.com/a?b=1")
        );
        assert_eq!(
            classify_input("file:///tmp/index.html").as_deref(),
            Some("file:///tmp/index.html")
        );
    }

    #[test]
    fn dots_only_after_authority_mean_search() {
        // the dot is in the path/query, not the host → not a host
        assert_eq!(
            classify_input("what is a dot?.com-ish thing").as_deref(),
            Some("https://duckduckgo.com/?q=what+is+a+dot%3F.com-ish+thing")
        );
        assert_eq!(
            classify_input("no dots here").as_deref(),
            Some("https://duckduckgo.com/?q=no+dots+here")
        );
        assert_eq!(
            classify_input("rust").as_deref(),
            Some("https://duckduckgo.com/?q=rust")
        );
    }

    #[test]
    fn localhost_without_dot_is_a_search() {
        assert_eq!(
            classify_input("localhost").as_deref(),
            Some("https://duckduckgo.com/?q=localhost")
        );
        assert_eq!(
            classify_input("localhost:8080").as_deref(),
            Some("https://duckduckgo.com/?q=localhost%3A8080")
        );
    }

    #[test]
    fn empty_input_is_ignored() {
        assert_eq!(classify_input("   "), None);
    }

    #[test]
    fn search_query_is_percent_encoded() {
        let got = classify_input("hello & goodbye").unwrap();
        assert_eq!(got, "https://duckduckgo.com/?q=hello+%26+goodbye");
        // the produced URL parses and round-trips the query losslessly
        let u = url::Url::parse(&got).unwrap();
        let q: Vec<(String, String)> = u.query_pairs().into_owned().collect();
        assert_eq!(q, vec![("q".to_string(), "hello & goodbye".to_string())]);
    }
}
