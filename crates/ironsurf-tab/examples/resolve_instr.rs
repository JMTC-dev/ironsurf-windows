//! Probe: narrow the spinning CSS for Acid3's `#instructions` paragraph.
//! Usage: resolve_instr <variant-name>
use blitz_dom::DocumentConfig;
use blitz_html::HtmlDocument;
use std::time::Instant;

fn run(label: &str, html: &str) {
    let t = Instant::now();
    let mut doc = HtmlDocument::from_html(
        html,
        DocumentConfig {
            base_url: Some("file:///tmp/acid3dir/index.html".to_owned()),
            ..Default::default()
        },
    );
    doc.resolve(0.);
    println!("[{label}] ok ({}ms)", t.elapsed().as_millis());
}

fn main() {
    let variant = std::env::args().nth(1).expect("variant");
    let text = "To pass the test,<span></span> a browser must use its default settings, the animation has to be smooth, the score has to end on 100/100, and the final page has to look exactly, pixel for pixel, like <a href=\"reference.html\">this reference rendering</a>.";
    let base_css = r#"
  * { margin: 0; border: 1px blue; padding: 0; border-spacing: 0; font: inherit; line-height: 1.2; color: inherit; background: transparent; }
  html { font: 20px Arial, sans-serif; border: 2cm solid gray; width: 32em; margin: 1em; }
  :root { background: silver; color: black; border-width: 0 0.2em 0.2em 0; }
  body { padding: 2em 2em 0; border: solid 1px black; margin: -0.2em 0 0 -0.2em; }
  #instructions { font-family: Helvetica, Arial, sans-serif; font-size: medium; line-height: 1.3125em; margin-top: 1.42857em; }
  #instructions span { float: right; width: 20px; margin-right: -20px; background: white; height: 20px; }
"#;
    let css_no_span = base_css.replace("#instructions span { float: right; width: 20px; margin-right: -20px; background: white; height: 20px; }", "");
    let css_acid = base_css.replace("font-family: Helvetica", "font-family: \"AcidAhemTest\", Helvetica").to_owned()
        + "\n  @font-face { font-family: \"AcidAhemTest\"; src: url(font.ttf); }\n";
    let html = |css: &str| format!("<!doctype html><html><head>{css}</head><body><p id=\"instructions\">{text}</p></body></html>");
    let html2 = |css: &str| format!("<!doctype html><html><head>{css}</head><body>{text}</body></html>");
    match variant.as_str() {
        "full" => run("full", &html(base_css)),
        "no-float-span" => run("no-float-span", &html(&css_no_span)),
        "no-id" => run("no-id", &html2(base_css)),
        "acid-font" => run("acid-font", &html(&css_acid)),
        _ => panic!("unknown variant"),
    }
}
