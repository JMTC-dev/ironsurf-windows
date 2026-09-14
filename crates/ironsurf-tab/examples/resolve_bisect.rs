//! Probe: which part of the Acid3 page makes `resolve()` spin?
//! Usage: resolve_probe [passes]  — bisects the page saved at /tmp/acid3-raw.html
use blitz_dom::DocumentConfig;
use blitz_html::HtmlDocument;
use std::time::Instant;

fn probe(label: &str, html: &str, passes: usize) {
    let t = Instant::now();
    let mut doc = HtmlDocument::from_html(
        html,
        DocumentConfig {
            base_url: Some("file:///tmp/acid3dir/index.html".to_owned()),
            ..Default::default()
        },
    );
    let parsed = t.elapsed();
    for i in 0..passes {
        let t = Instant::now();
        doc.resolve(0.);
        let ms = t.elapsed().as_millis();
        println!("[{label}] resolve #{i} {ms}ms");
        let t = Instant::now();
        doc.handle_messages();
        println!("[{label}] msgs #{i} {}ms", t.elapsed().as_millis());
    }
    println!("[{label}] done (parse {}ms)", parsed.as_millis());
}

fn main() {
    let passes: usize = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(2);
    let only = std::env::var("PROBE_ONLY").ok();
    let raw = std::fs::read_to_string("/tmp/acid3-raw.html").expect("read /tmp/acid3-raw.html");

    let cases: Vec<(String, String)> = vec![
        ("full".into(), raw.clone()),
        ("head-only".into(), {
            let i = raw.find("<body").unwrap_or(raw.len());
            raw[..i].to_string()
        }),
        ("body-only".into(), {
            let i = raw.rfind("<body").unwrap();
            raw[i..].to_string()
        }),
        ("no-styles".into(), {
            let mut s = raw.clone();
            while let Some(a) = s.find("<style") {
                let b = s.find("</style>").map(|b| b + 8).unwrap_or(s.len());
                s.replace_range(a..b, "");
            }
            s
        }),
        ("no-comments".into(), {
            let mut s = raw.clone();
            while let Some(a) = s.find("<!--") {
                let b = s.find("-->").map(|b| b + 3).unwrap_or(s.len());
                s.replace_range(a..b, "");
            }
            s
        }),
        ("style-block-1".into(), {
            let a = raw.find("<style").unwrap();
            let b = raw.find("</style>").unwrap() + 8;
            raw[a..b].to_string()
        }),
        ("style-block-2".into(), {
            let a = raw[raw.find("</style>").unwrap() + 8..]
                .find("<style")
                .map(|o| raw.find("</style>").unwrap() + 8 + o)
                .unwrap();
            let b = raw[a..].find("</style>").map(|o| a + o + 8).unwrap_or(raw.len());
            raw[a..b].to_string()
        }),
    ];

    for (label, html) in cases {
        if let Some(want) = &only {
            if &label != want {
                continue;
            }
        }
        println!("== {label} ({} B) ==", html.len());
        probe(&label, &html, passes);
    }
}
