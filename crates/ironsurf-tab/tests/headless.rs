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
