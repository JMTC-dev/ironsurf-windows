//! Probe: which styled fragment of Acid3 makes `resolve()` spin?
//! `resolve_spin --list` prints case ids; `resolve_spin <n>` runs one case
//! (a driver script runs each under `timeout` to find the hanging one).
use blitz_dom::DocumentConfig;
use blitz_html::HtmlDocument;
use std::time::Instant;

fn style_block(raw: &str) -> String {
    let a = raw.find("<style").unwrap();
    let b = raw.find("</style>").unwrap() + 8;
    raw[a..b].to_string()
}

fn body_inner(raw: &str) -> String {
    let a = raw.rfind("<body").unwrap();
    let b = raw[a..].find("</body>").map(|o| a + o).unwrap_or(raw.len());
    let s = &raw[a..b];
    let s = &s[s.find('>').unwrap() + 1..];
    s.to_string()
}

/// One case per line-ish top-level unit of the body.
fn cases(raw: &str) -> Vec<(String, String)> {
    let style = style_block(raw);
    let body = body_inner(raw);
    let mut out = vec![("full".to_string(), format!("<!doctype html><html><head><title>t</title>{style}</head><body>{body}</body></html>"))];
    for (i, line) in body.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        out.push((format!("line{i}:{line}"), format!("<!doctype html><html><head>{style}</head><body>{line}</body></html>")));
    }
    out
}

fn main() {
    let raw = std::fs::read_to_string("/tmp/acid3-raw.html").expect("read");
    let all = cases(&raw);
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(|s| s.as_str()) == Some("--list") {
        for (i, (label, _)) in all.iter().enumerate() {
            println!("{i}\t{label}");
        }
        return;
    }
    let n: usize = args.get(1).and_then(|s| s.parse().ok()).expect("case index");
    let (label, html) = &all[n];
    let t = Instant::now();
    let mut doc = HtmlDocument::from_html(
        html,
        DocumentConfig {
            base_url: Some("file:///tmp/acid3dir/index.html".to_owned()),
            ..Default::default()
        },
    );
    doc.resolve(0.);
    doc.handle_messages();
    doc.resolve(0.);
    println!("[{label}] ok ({}ms)", t.elapsed().as_millis());
}
