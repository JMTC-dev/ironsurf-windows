//! Integration test for the Blitz rendering spike (task 6 — engine pivot).
//!
//! Headless (no `$DISPLAY`, no GPU): launches the built `ironsurf-tab --spike`
//! on a tiny local `file://` fixture with pure inline CSS and asserts the
//! VelloCpu RGBA8 buffer is exactly `w*h*4` bytes with non-uniform pixels,
//! plus a valid non-uniform PPM for eyeballing.
//!
//! Run with: `cargo test -p ironsurf-tab --test spike`

use std::fs;
use std::process::Command;

/// Minimal page: white background, solid colour blocks, and a heading —
/// guarantees a non-uniform frame if (and only if) Blitz parsed, resolved,
/// and painted anything. Pure inline CSS: no external assets, no network.
const FIXTURE: &str = r#"<!doctype html>
<html>
  <head><title>IronSurf Spike</title></head>
  <body style="margin: 0; background: #ffffff">
    <div style="width: 120px; height: 120px; background: #ff3200"></div>
    <div style="width: 60px; height: 60px; background: #006400"></div>
    <h1 style="color: #000064; font-family: monospace">IronSurf spike OK</h1>
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
    let out_rgba = dir.path().join("spike.rgba");

    let exe = env!("CARGO_BIN_EXE_ironsurf");
    let status = Command::new(exe)
        // Prove the pure-CPU path: no display of any kind.
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
        .arg("--out-rgba")
        .arg(&out_rgba)
        .arg("--size")
        .arg("800x600")
        .status()
        .expect("spawn ironsurf-tab spike");
    assert!(status.success(), "spike exited with {status:?}");

    // The raw VelloCpuImageRenderer buffer: exactly w*h*4 RGBA8 bytes.
    let rgba = fs::read(&out_rgba).expect("raw RGBA buffer exists");
    let (w, h) = (800u32, 600u32);
    assert_eq!(
        rgba.len(),
        (w as usize) * (h as usize) * 4,
        "render buffer must be w*h*4 bytes"
    );
    let first_pixel = rgba[..4].to_vec();
    let uniform = rgba.chunks_exact(4).all(|px| px == first_pixel);
    assert!(
        !uniform,
        "render buffer is a single uniform color ({first_pixel:?}) — nothing was painted"
    );

    // PPM sidecar for eyeballing (P6 RGB).
    let bytes = fs::read(&out).expect("PPM exists");
    let (pw, ph, pixels) = parse_ppm(&bytes);
    assert_eq!((pw, ph), (w, h), "PPM dimensions");
    let first = pixels[..3].to_vec();
    let uniform = pixels.chunks_exact(3).all(|px| px == first);
    assert!(
        !uniform,
        "PPM is a single uniform color ({first:?}) — nothing was painted"
    );

    // Sanity: the red block actually made it into the top-left corner.
    let idx = (40 * w + 40) as usize * 4; // pixel (40,40) inside the 120x120 red div
    let (r, g, b, a) = (rgba[idx], rgba[idx + 1], rgba[idx + 2], rgba[idx + 3]);
    assert!(
        r > 128 && g < 128 && b < 128 && a == 255,
        "expected reddish opaque pixel at (40,40), got {r},{g},{b},{a}"
    );
}
