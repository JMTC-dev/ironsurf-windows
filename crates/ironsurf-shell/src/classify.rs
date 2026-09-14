//! Search-or-navigate classification for the URL bar.
//!
//! Rule (per ARCHITECTURE.md §ironsurf-shell): a typed value whose host part
//! contains a dot becomes an `https://` URL; everything else becomes a
//! DuckDuckGo search. Explicit schemes pass through untouched — matched
//! case-insensitively (per the URL spec) and including `data:`, whose payload
//! is base64 in practice (`data:text/javascript;base64,…`) and therefore never
//! looks like a host.

/// Typed text → a URL to navigate to (already absolute).
pub fn classify_input(input: &str) -> Option<String> {
    let input = input.trim();
    if input.is_empty() {
        return None;
    }

    // explicit scheme → pass through (the tab applies its own policy, running
    // `javascript:` URLs as programs in the current page, which stays visible
    // instead of becoming a search for the text after the colon).
    let scheme_haystack = input.to_ascii_lowercase();
    for scheme in [
        "http://",
        "https://",
        "about:",
        "file://",
        "data:",
        "javascript:",
        "ironsurf://",
    ] {
        if scheme_haystack.starts_with(scheme) {
            return Some(input.to_string());
        }
    }

    // any whitespace means this can only be a search
    if input.chars().any(char::is_whitespace) {
        return Some(search_url(input));
    }

    // `host` / `host:port` / `host/path` with a dot in the authority → https
    let host_end = input.find(['/', '?', '#']).unwrap_or(input.len());
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
        "https://html.duckduckgo.com/html/?q={}",
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
        assert_eq!(
            classify_input("about:blank").as_deref(),
            Some("about:blank")
        );
        assert_eq!(
            classify_input("https://example.com/a?b=1").as_deref(),
            Some("https://example.com/a?b=1")
        );
        assert_eq!(
            classify_input("file:///tmp/index.html").as_deref(),
            Some("file:///tmp/index.html")
        );
        // The built-in start page stays a URL: its host has no dot, so without
        // the scheme match it would silently become a DuckDuckGo search.
        assert_eq!(
            classify_input("ironsurf://welcome").as_deref(),
            Some("ironsurf://welcome")
        );
        assert_eq!(
            classify_input("IRONSURF://Welcome").as_deref(),
            Some("IRONSURF://Welcome")
        );
    }

    /// `data:text/javascript;base64,…` — the payload has no dot and is not a
    /// host, so the scheme match is what keeps it out of the search box.
    const JS_DATA_URL: &str =
        "data:text/javascript;base64,Y29uc29sZS5sb2coImhpIik7IGRvY3VtZW50LnRpdGxlID0gImpzIjs=";

    #[test]
    fn data_url_passes_through_instead_of_becoming_a_search() {
        assert_eq!(classify_input(JS_DATA_URL).as_deref(), Some(JS_DATA_URL));
        // The typed text is a URL the tab can actually parse (and whose media
        // type it can act on).
        let parsed = url::Url::parse(&classify_input(JS_DATA_URL).unwrap()).unwrap();
        assert_eq!(parsed.scheme(), "data");
        assert!(!parsed.as_str().contains("duckduckgo"));
    }

    #[test]
    fn scheme_match_is_case_insensitive() {
        // URL schemes are case-insensitive, and `DATA:`/`Data:` arrive from
        // pasted links just as often as lowercase.
        assert_eq!(
            classify_input("DATA:Text/HTML,<p>hi</p>").as_deref(),
            Some("DATA:Text/HTML,<p>hi</p>")
        );
        assert_eq!(
            classify_input("HTTPS://example.com/X").as_deref(),
            Some("HTTPS://example.com/X")
        );
        assert_eq!(
            classify_input("File:///tmp/index.html").as_deref(),
            Some("File:///tmp/index.html")
        );
        let payload = JS_DATA_URL.split_once(',').expect("data url").1;
        let upper = format!("DATA:TEXT/JAVASCRIPT;BASE64,{payload}");
        let got = classify_input(&upper).unwrap();
        assert_eq!(got, upper);
        assert_eq!(url::Url::parse(&got).unwrap().scheme(), "data");
    }

    #[test]
    fn javascript_urls_reach_the_tab_that_runs_them() {
        // The tab owns `javascript:` (it executes the program; the URL itself
        // never becomes the document URL); searching would never run it.
        assert_eq!(
            classify_input("javascript:void(0)").as_deref(),
            Some("javascript:void(0)")
        );
        // Words that merely start with the scheme are still searches.
        assert_eq!(
            classify_input("javascript tutorial").as_deref(),
            Some("https://html.duckduckgo.com/html/?q=javascript+tutorial")
        );
    }

    #[test]
    fn dots_only_after_authority_mean_search() {
        // the dot is in the path/query, not the host → not a host
        assert_eq!(
            classify_input("what is a dot?.com-ish thing").as_deref(),
            Some("https://html.duckduckgo.com/html/?q=what+is+a+dot%3F.com-ish+thing")
        );
        assert_eq!(
            classify_input("no dots here").as_deref(),
            Some("https://html.duckduckgo.com/html/?q=no+dots+here")
        );
        assert_eq!(
            classify_input("rust").as_deref(),
            Some("https://html.duckduckgo.com/html/?q=rust")
        );
    }

    #[test]
    fn localhost_without_dot_is_a_search() {
        assert_eq!(
            classify_input("localhost").as_deref(),
            Some("https://html.duckduckgo.com/html/?q=localhost")
        );
        assert_eq!(
            classify_input("localhost:8080").as_deref(),
            Some("https://html.duckduckgo.com/html/?q=localhost%3A8080")
        );
    }

    #[test]
    fn empty_input_is_ignored() {
        assert_eq!(classify_input("   "), None);
    }

    #[test]
    fn search_query_is_percent_encoded() {
        let got = classify_input("hello & goodbye").unwrap();
        assert_eq!(got, "https://html.duckduckgo.com/html/?q=hello+%26+goodbye");
        // the produced URL parses and round-trips the query losslessly
        let u = url::Url::parse(&got).unwrap();
        let q: Vec<(String, String)> = u.query_pairs().into_owned().collect();
        assert_eq!(q, vec![("q".to_string(), "hello & goodbye".to_string())]);
    }
}
