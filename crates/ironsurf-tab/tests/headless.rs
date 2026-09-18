//! Headless ACID-test harness mode (`ironsurf-tab --headless`).
//!
//! Runs the built binary against a local `file://` fixture whose script uses
//! only the JS surface `js.rs` supports today (`document.title = …`), and
//! checks the three harness outputs: the `HEADLESS …` stderr summary, the
//! `--body-out` DOM dump, and the `--shot` P6 PPM (header + exact size).
//! Fully offline; no `$DISPLAY`, no network.

use std::fs;
use std::path::Path;
use std::process::Command;

/// Fixture: static body text, a `<title>`, and a script that re-titles the
/// page through the only JS binding the runtime executes (title setter).
const FIXTURE: &str = r#"<!doctype html>
<html>
  <head><title>Before JS</title></head>
  <body>
    <h1>Headless harness body</h1>
    <p>score: <b>42</b> &amp; counting</p>
    <script>document.title = "After JS";</script>
  </body>
</html>
"#;

fn run_headless(dir: &Path, extra: &[&str]) -> std::process::Output {
    let fixture = dir.join("fixture.html");
    fs::write(&fixture, FIXTURE).expect("write fixture");
    let output = Command::new(env!("CARGO_BIN_EXE_ironsurf-tab"))
        .arg("--headless")
        .arg("--url")
        .arg(
            url::Url::from_file_path(&fixture)
                .expect("file url")
                .as_str(),
        )
        .args(extra)
        .output()
        .expect("spawn ironsurf-tab --headless");
    assert!(
        output.status.success(),
        "exit {:?}\nstderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

#[test]
fn headless_mode_dumps_body_shot_and_summary() {
    let dir = tempfile::tempdir().expect("tempdir");
    let body_out = dir.path().join("nested/deeper/body.html"); // parent must be created
    let shot = dir.path().join("out.ppm");

    let output = run_headless(
        dir.path(),
        &[
            "--wait",
            "100",
            "--body-out",
            body_out.to_str().unwrap(),
            "--shot",
            shot.to_str().unwrap(),
            "--size",
            "320x240",
        ],
    );

    // stdout stays unused in headless mode.
    assert!(
        output.stdout.is_empty(),
        "headless stdout must be empty, got {} bytes",
        output.stdout.len()
    );
    // stderr summary line with the JS-assigned title.
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("HEADLESS title=After JS"),
        "summary missing from stderr:\n{stderr}"
    );

    // Body dump: the serialized <body> innerHTML, including the JS-era text,
    // the raw <script> source (raw-text element), and escaped entities.
    let body = fs::read_to_string(&body_out).expect("body-out written (with parent dirs)");
    assert!(body.contains("Headless harness body"), "{body}");
    assert!(body.contains("score: <b>42</b>"), "child tags: {body}");
    assert!(body.contains("&amp;"), "escaped entity: {body}");
    assert!(
        body.contains("document.title"),
        "script source kept as raw text: {body}"
    );

    // Screenshot: exact P6 PPM at the requested viewport size.
    let ppm = fs::read(&shot).expect("shot written");
    let header = format!("P6\n320 240\n255\n");
    assert!(
        ppm.starts_with(header.as_bytes()),
        "bad PPM header: {:?}",
        &ppm[..header.len().min(ppm.len())]
    );
    assert_eq!(
        ppm.len(),
        header.len() + 320 * 240 * 3,
        "PPM must hold exactly w*h*3 RGB bytes after the header"
    );
}

#[test]
fn headless_requires_url_and_fails_cleanly_without_it() {
    let output = Command::new(env!("CARGO_BIN_EXE_ironsurf-tab"))
        .arg("--headless")
        .arg("--wait")
        .arg("10")
        .output()
        .expect("spawn");
    assert!(!output.status.success(), "missing --url must fail");
    assert!(output.stdout.is_empty(), "stdout stays unused in headless");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("--headless requires --url"), "{stderr}");
}

#[test]
fn headless_wait_window_runs_js_timers() {
    // The task: real setTimeout — the `--wait` window is the poll loop that
    // pumps the JS timer queue, so a callback armed for 10 ms has fired
    // (title re-assigned) by capture time. `--wait 400` with a still-armed
    // 20 ms interval also proves timers do NOT hold the run open: exit
    // happens on the requested deadline, not when the page "finishes".
    let dir = tempfile::tempdir().expect("tempdir");
    let page = dir.path().join("timers.html");
    fs::write(
        &page,
        r#"<!doctype html>
<html>
  <head><title>before</title></head>
  <body>
    <script>
      setTimeout(function () { document.title = "timer-fired"; }, 10);
      var n = 0;
      setInterval(function () { n += 1; }, 20);
    </script>
  </body>
</html>
"#,
    )
    .expect("write timers page");
    let output = Command::new(env!("CARGO_BIN_EXE_ironsurf-tab"))
        .arg("--headless")
        .arg("--url")
        .arg(
            url::Url::from_file_path(&page)
                .expect("file url")
                .as_str(),
        )
        .args(["--wait", "400"])
        .output()
        .expect("spawn ironsurf-tab --headless");
    assert!(
        output.status.success(),
        "exit {:?}\nstderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stdout.is_empty(), "stdout stays unused");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("HEADLESS title=timer-fired"),
        "the setTimeout callback must have retitled the page within --wait:\n{stderr}"
    );
}

// ---------------------------------------------------------------------------
// DOM bridge smoke test: page JS builds the DOM through the real blitz
// document (`document.createElement`/`appendChild` land in the parsed tree),
// the built node is *painted* (red pixels in the --shot PPM) and shows up in
// the --body-out serialization.

/// The box the script creates is 200x120 solid red; the viewport is bigger.
const DOM_FIXTURE: &str = r#"<!doctype html>
<html>
<head><title>Before</title></head>
<body>
  <script>
    var box = document.createElement("div");
    box.id = "box";
    box.setAttribute("style", "width: 200px; height: 120px; background: red");
    document.body.appendChild(box);
    box.textContent = "built:" + (document.getElementById("box") === box) +
                      ":" + box.parentNode.nodeName +
                      ":" + (document.getElementById("box") === box);
    document.title = "DomBridge " + (document.body.lastElementChild === box ? "ok" : "gone");
  </script>
</body>
</html>
"#;

#[test]
fn dom_bridge_paints_red_box_and_serializes_it() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fixture = dir.path().join("dom.html");
    fs::write(&fixture, DOM_FIXTURE).expect("write fixture");
    let body_out = dir.path().join("body.html");
    let shot = dir.path().join("shot.ppm");

    let output = Command::new(env!("CARGO_BIN_EXE_ironsurf-tab"))
        .arg("--headless")
        .arg("--url")
        .arg(url::Url::from_file_path(&fixture).expect("file url").as_str())
        .args([
            "--wait",
            "150",
            "--body-out",
            body_out.to_str().unwrap(),
            "--shot",
            shot.to_str().unwrap(),
            "--size",
            "400x300",
        ])
        .output()
        .expect("spawn ironsurf-tab --headless");
    assert!(
        output.status.success(),
        "exit {:?}\nstderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );

    // 1. The identity assertions ran (title reflects the DOM the script built).
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("HEADLESS title=DomBridge ok"),
        "title assertion failed:\n{stderr}"
    );

    // 2. --body-out serialized the script-built node out of the real tree.
    let body = fs::read_to_string(&body_out).expect("body out");
    assert!(body.contains("<div"), "div missing in:\n{body}");
    assert!(body.contains("id=\"box\""), "id attribute missing in:\n{body}");
    assert!(
        body.contains("background: red"),
        "style attribute missing in:\n{body}"
    );
    assert!(
        body.contains("built:true:BODY"),
        "identity/parent assertions failed in:\n{body}"
    );
    assert!(body.contains("</div>"), "end tag missing in:\n{body}");

    // 3. The new node is actually painted: thousands of pure-red pixels.
    let ppm = fs::read(&shot).expect("shot");
    let header_end = ppm
        .windows(4)
        .position(|w| w == b"255\n")
        .expect("ppm maxbyte header")
        + 4;
    let pixels = &ppm[header_end..];
    let red_pixels = pixels.chunks_exact(3).filter(|rgb| rgb == b"\xff\x00\x00").count();
    assert!(
        red_pixels > 5_000,
        "expected a painted red box (200x120), found {red_pixels} red pixels"
    );
}

// ---------------------------------------------------------------------------
// startpage.com search bar regression (task): its first inline script calls
// `window.matchMedia("(prefers-color-scheme: dark)")` and reads
// `#site_theme`.dataset.siteTheme before running `revealRoot()`, which lifts
// the `#root { opacity: 0 }` the page ships (in a `.theme-flash-mitigation`
// <style>) to prevent a theme flash. Before `matchMedia`/`dataset` existed,
// the script died with "TypeError: not a callable function" at :2:51, the
// reveal never ran, and the ENTIRE page — search bar included — stayed at
// opacity 0: invisible and untypeable. The fixture reproduces the reveal
// verbatim over a painted box; the control ships the same hidden CSS with no
// script, proving the pixels below come from the script's reveal, not luck.

/// startpage.com's theme-reveal script (verbatim), gating a 200x120 box.
const REVEAL_FIXTURE: &str = r##"<!doctype html>
<html>
<head>
<style class="theme-flash-mitigation">
@media (prefers-color-scheme: dark) { html { background: #171B25; } }
#root { opacity: 0; }
</style>
<meta id="site_theme" name="site_theme" data-site-theme="device"/>
</head>
<body>
<div id="root"><div style="width: 200px; height: 120px; background: #ff0000;"></div></div>
<script>
      const osPrefersDarkTheme = window.matchMedia("(prefers-color-scheme: dark)");
      const themeFlashStyleTag = document.querySelector(".theme-flash-mitigation");
      const siteTheme = document.querySelector("#site_theme").dataset.siteTheme;
      const isDarkOsThemeOnly = osPrefersDarkTheme.matches && siteTheme === "device";
      const isSiteThemeDark = ["dark", "night"].includes(siteTheme);
      const isSiteThemeLight = ["light", "air"].includes(siteTheme);
      function revealRoot() {
        themeFlashStyleTag.innerHTML += `
          #root {
            opacity: 1;
          }
        `;
      }
      if (isDarkOsThemeOnly) {
        window.prefersDark = true;
        window.addEventListener('darkThemeLoaded', function() {
          revealRoot();
        });
      } else {
        revealRoot();
      }
</script>
</body>
</html>
"##;

/// The same hidden document with the script removed: the control proving
/// `#root { opacity: 0 }` genuinely suppresses the box's pixels.
const HIDDEN_CONTROL: &str = r#"<!doctype html>
<html>
<head>
<style class="theme-flash-mitigation">
#root { opacity: 0; }
</style>
</head>
<body>
<div id="root"><div style="width: 200px; height: 120px; background: #ff0000;"></div></div>
</body>
</html>
"#;

/// Non-white RGB pixels in a --shot PPM (content the paper does not have).
fn non_white_pixels(ppm: &[u8]) -> usize {
    let header_end = ppm
        .windows(4)
        .position(|w| w == b"255\n")
        .expect("ppm maxbyte header")
        + 4;
    ppm[header_end..]
        .chunks_exact(3)
        .filter(|rgb| rgb.iter().any(|&c| c < 240))
        .count()
}

fn shoot(dir: &std::path::Path, name: &str, html: &str) -> (Vec<u8>, String) {
    let fixture = dir.join(name);
    fs::write(&fixture, html).expect("write fixture");
    let shot = dir.join(format!("{name}.ppm"));
    let output = Command::new(env!("CARGO_BIN_EXE_ironsurf-tab"))
        .arg("--headless")
        .arg("--url")
        .arg(url::Url::from_file_path(&fixture).expect("file url").as_str())
        .args(["--wait", "200", "--shot", shot.to_str().unwrap(), "--size", "400x240"])
        .output()
        .expect("spawn ironsurf-tab --headless");
    assert!(
        output.status.success(),
        "exit {:?}\nstderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );
    (fs::read(&shot).expect("shot"), String::from_utf8_lossy(&output.stderr).into_owned())
}

// ---------------------------------------------------------------------------
// CSS float regression (DDG-lite blank page): blitz-dom registers every
// `float:` element as a parley `CustomOutOfFlow` inline box, and ONLY the
// `floats`-feature layout driver consumes those `InlineBoxBreak` yields. With
// blitz-dom's `floats` Cargo feature off, `break_all_lines` loops on
// `break_next().is_some()` forever: the document fetches, "loading finished"
// arrives, and layout NEVER returns — a permanently blank tab. DuckDuckGo's
// html-lite anomaly modal hit exactly this (`.box:before{float:left}`). The
// process must COMPLETE (the hang is the regression — this test would simply
// never finish on the broken feature set) and the float must really float:
// the red square paints AND the text flows to its right, on its own lines.
const FLOAT_FIXTURE: &str = r#"<!doctype html>
<html>
<head>
<style>
  .tile { position: relative; width: 100px; height: 100px; overflow: hidden; padding: 1px }
  .tile:before { content: ""; display: block; padding-top: 100%; float: left }
  .box { position: relative; width: 100px; height: 100px; float: left; background: #ff0000 }
  p { margin: 0; font-size: 16px; color: #000000 }
</style>
</head>
<body>
  <div class="box"></div>
  <p>the quick brown fox jumps over the lazy dog and keeps on running past the float</p>
  <div class="tile"></div>
</body>
</html>
"#;

/// Non-white RGB pixels in a window of a --shot PPM: `x in x0..x1, y in y0..y1`.
fn non_white_in(ppm: &[u8], width: usize, x0: usize, x1: usize, y0: usize, y1: usize) -> usize {
    let header_end = ppm.windows(4).position(|w| w == b"255\n").expect("ppm maxbyte header") + 4;
    let px = &ppm[header_end..];
    (y0..y1)
        .flat_map(|y| (x0..x1).map(move |x| (y * width + x) * 3))
        .filter(|&i| px[i] < 240 || px[i + 1] < 240 || px[i + 2] < 240)
        .count()
}

#[test]
fn css_float_does_not_hang_layout_and_wraps_text() {
    let dir = tempfile::tempdir().expect("tempdir");
    // 400x240 keeps each prose line inside the first ~5 text rows.
    let (shot, _stderr) = shoot(dir.path(), "float.html", FLOAT_FIXTURE);

    // The floated red square paints (top-left, 100x100).
    let red = shot
        .windows(3)
        .filter(|rgb| rgb == b"\xff\x00\x00")
        .count();
    assert!(red > 9_000, "floated 100x100 red box must paint ~10 000 px, found {red}");

    // With the float honoured, the prose flows to its RIGHT (x>110) on the
    // first text lines (y<60). Without floating the text starts at x=0 and
    // the strip right of the box stays paper for those lines.
    let right_of_float = non_white_in(&shot, 400, 115, 400, 0, 60);
    assert!(
        right_of_float > 100,
        "text must wrap beside the float (ink right of x=115 in the top 60 rows), saw {right_of_float}"
    );
}

#[test]
fn matchmedia_reveal_script_unhides_the_page_it_gates() {
    let dir = tempfile::tempdir().expect("tempdir");

    // Control: with no reveal script the gated content is NOT painted.
    let (hidden, _) = shoot(dir.path(), "hidden.html", HIDDEN_CONTROL);
    assert!(
        non_white_pixels(&hidden) < 100,
        "opacity: 0 must hide the box, saw {} content pixels",
        non_white_pixels(&hidden)
    );

    // The startpage reveal: matchMedia + dataset + innerHTML += all survive,
    // raise the opacity, and the box paints. No TypeError anywhere.
    let (revealed, stderr) = shoot(dir.path(), "reveal.html", REVEAL_FIXTURE);
    assert!(
        !stderr.contains("TypeError"),
        "the reveal script must run clean:\n{stderr}"
    );
    let content = non_white_pixels(&revealed);
    assert!(
        content > 20_000,
        "the reveal must paint the 200x120 box (~24 000 px), found {content}"
    );
}
