//! Integration test for the Servo rendering spike (task 4).
//!
//! Headless (no `$DISPLAY`): launches the built `servfox-tab --spike` on a tiny
//! local `file://` fixture and asserts a correctly-sized, non-uniform PPM.
//!
//! Run with: `cargo test -p servfox-tab --test spike`

use std::fs;
use std::process::Command;

/// Minimal page: white background, a solid red box, and a blue heading —
/// guarantees a non-uniform frame if (and only if) rendering actually happened.
const FIXTURE: &str = r#"<!doctype html>
<html>
  <head><title>ServFox Spike</title></head>
  <body style="margin: 0; background: #ffffff">
    <div style="width: 120px; height: 120px; background: #ff3200"></div>
    <h1 style="color: #000064; font-family: monospace">ServFox spike OK</h1>
  </body>
</html>
"#;

fn parse_ppm(bytes: &[u8]) -> (u32, u32, Vec<u8>) {
    // "P6\n<w> <h>\n255\n" then w*h*3 RGB bytes.
    let header_end = bytes
        .windows(4)
        .position(|w| w == b"255\n")
        .expect("PPM maxval section");
    let header = std::str::from_utf8(&bytes[..header_end])
        .expect("ASCII header")
        .to_owned();
    let mut it = header.split_whitespace();
    assert_eq!(it.next(), Some("P6"), "PPM magic");
    let w: u32 = it.next().expect("width").parse().expect("numeric width");
    let h: u32 = it.next().expect("height").parse().expect("numeric height");
    let pixels = bytes[header_end + 4..].to_vec();
    assert_eq!(pixels.len(), (w * h * 3) as usize, "pixel data length");
    (w, h, pixels)
}

#[test]
fn spike_captures_local_html_page() {
    let dir = tempfile::tempdir().expect("tempdir");
    let html = dir.path().join("index.html");
    fs::write(&html, FIXTURE).expect("write fixture");
    let out = dir.path().join("spike.ppm");

    let exe = env!("CARGO_BIN_EXE_servfox-tab");
    let status = Command::new(exe)
        // Prove the software path: no display of any kind.
        .env_remove("DISPLAY")
        .env_remove("WAYLAND_DISPLAY")
        .arg("--spike")
        .arg("--url")
        .arg(
            url::Url::from_file_path(&html)
                .expect("file url")
                .as_str(),
        )
        .arg("--out")
        .arg(&out)
        .arg("--size")
        .arg("800x600")
        .status()
        .expect("spawn servfox-tab spike");
    assert!(status.success(), "spike exited with {status:?}");

    let bytes = fs::read(&out).expect("PPM exists");
    let (w, h, pixels) = parse_ppm(&bytes);
    assert_eq!((w, h), (800, 600), "PPM dimensions");

    let first = pixels[..3].to_vec();
    let uniform = pixels.chunks_exact(3).all(|px| px == first);
    assert!(
        !uniform,
        "captured frame is a single uniform color ({first:?}) — nothing was rendered"
    );
}
