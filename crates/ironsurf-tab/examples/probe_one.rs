use blitz_dom::DocumentConfig;
use blitz_html::HtmlDocument;
use blitz_traits::shell::{ColorScheme, Viewport};
use std::time::Instant;
fn main() {
    let f = std::env::args().nth(1).unwrap();
    let html = std::fs::read_to_string(&f).unwrap();
    let vp = std::env::var("VP").ok().map(|v| {
        let (w, h) = v.split_once('x').unwrap();
        Viewport::new(w.parse().unwrap(), h.parse().unwrap(), 1.0, ColorScheme::Light)
    });
    let t = Instant::now();
    let mut d = HtmlDocument::from_html(
        &html,
        DocumentConfig {
            base_url: Some("file:///tmp/acid3dir/index.html".into()),
            viewport: vp,
            ..Default::default()
        },
    );
    println!("parse {}ms", t.elapsed().as_millis());
    for i in 0..3 {
        let t = Instant::now();
        d.resolve(0.);
        println!("resolve #{i} {}ms", t.elapsed().as_millis());
    }
}
