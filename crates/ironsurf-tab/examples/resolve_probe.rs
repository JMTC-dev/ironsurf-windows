//! Probe: how do repeated `resolve()` passes on the saved Acid3 page behave?
//! Usage: resolve_probe <path.html> [passes]
use blitz_dom::DocumentConfig;
use blitz_html::HtmlDocument;
use std::time::Instant;

fn main() {
    let path = std::env::args().nth(1).expect("path");
    let passes: usize = std::env::args()
        .nth(2)
        .and_then(|s| s.parse().ok())
        .unwrap_or(4);
    let html = std::fs::read_to_string(&path).expect("read");
    let t = Instant::now();
    let url = url::Url::from_file_path(&path).unwrap();
    let use_net = std::env::var("PROBE_NET").is_ok();
    let viewport = std::env::var("PROBE_VIEWPORT").ok().map(|v| {
        let (w, h) = v.split_once('x').unwrap();
        (w.parse::<u32>().unwrap(), h.parse::<u32>().unwrap())
    });
    let mut doc = HtmlDocument::from_html(
        &html,
        DocumentConfig {
            base_url: Some(url.as_str().to_owned()),
            viewport: viewport.map(|(w, h)| {
                blitz_traits::shell::Viewport::new(
                    w,
                    h,
                    1.0,
                    blitz_traits::shell::ColorScheme::Light,
                )
            }),
            net_provider: if use_net {
                Some(blitz_net::Provider::shared(None))
            } else {
                None
            },
            ..Default::default()
        },
    );
    println!("from_html {}ms", t.elapsed().as_millis());
    for i in 0..passes {
        let t = Instant::now();
        doc.resolve(0.);
        println!("resolve #{i} {}ms", t.elapsed().as_millis());
        let t = Instant::now();
        doc.handle_messages();
        println!("handle_messages #{i} {}ms", t.elapsed().as_millis());
    }
}
