//! Blitz integration spike (task 6 — engine pivot from Servo).
//!
//! `servfox-tab --spike --url file:///… --out /tmp/spike.ppm [--size 1024x768] [--out-rgba out.rgba]`
//!
//! Canonical headless parse→resolve→paint flow (reference: `DioxusLabs/blitz`
//! `examples/screenshot.rs`, vendored under `~/.cargo/registry/src/*/blitz-*-0.3.0-beta.2/`):
//!
//! 1. fetch the HTML (local `file://` read; `http(s)://` via blocking reqwest/rustls);
//! 2. [`HtmlDocument::from_html`] with a [`DocumentConfig`] carrying `base_url`,
//!    a physical [`Viewport`] (`w*scale`, `h*scale`, `scale`, `ColorScheme::Light`)
//!    and a [`blitz_net`] provider for sub-resources;
//! 3. `resolve(0.)` (Stylo styles + taffy layout);
//! 4. [`anyrender::render_to_buffer`] with [`VelloCpuImageRenderer`] (pure CPU
//!    rasterizer — no X, no GPU, no wgpu): white background fill, then
//!    [`blitz_paint::paint_scene`], producing an RGBA8 buffer of `w*h*4` bytes.
//!
//! The buffer is written as a P6 PPM for eyeballing (and optionally as raw RGBA
//! so tests can assert the exact buffer length).

use std::error::Error;
use std::fs::File;
use std::io::Write;

use anyrender::{render_to_buffer, PaintScene};
use anyrender_vello_cpu::VelloCpuImageRenderer;
use blitz_dom::DocumentConfig;
use blitz_html::HtmlDocument;
use blitz_paint::paint_scene;
use blitz_traits::shell::{ColorScheme, Viewport};
use kurbo::{Affine, Rect, Size};
use peniko::{Color, Fill};

// ---------------------------------------------------------------------------
// CLI

pub struct SpikeArgs {
    pub url: String,
    pub out: String,
    pub out_rgba: Option<String>,
    pub width: u32,
    pub height: u32,
}

impl SpikeArgs {
    fn parse(args: &[String]) -> Result<Self, Box<dyn Error>> {
        let mut url = None;
        let mut out = None;
        let mut out_rgba = None;
        let mut size = (1024u32, 768u32);
        let mut it = args.iter().skip(1);
        while let Some(arg) = it.next() {
            let mut value = |what: &str| -> Result<String, Box<dyn Error>> {
                it.next()
                    .cloned()
                    .ok_or_else(|| format!("--{what} requires a value").into())
            };
            match arg.as_str() {
                "--spike" => {} // selected by main()
                "--url" => url = Some(value("url")?),
                "--out" => out = Some(value("out")?),
                "--out-rgba" => out_rgba = Some(value("out-rgba")?),
                "--size" => {
                    let v = value("size")?;
                    let (w, h) = v
                        .split_once('x')
                        .ok_or_else(|| format!("--size must be WxH, got {v}"))?;
                    size = (
                        w.parse()
                            .map_err(|_| format!("bad width in --size {v:?}"))?,
                        h.parse()
                            .map_err(|_| format!("bad height in --size {v:?}"))?,
                    );
                }
                other => return Err(format!("unexpected argument {other:?} in spike mode").into()),
            }
        }
        Ok(SpikeArgs {
            url: url.ok_or("--url is required")?,
            out: out.ok_or("--out is required")?,
            out_rgba,
            width: size.0,
            height: size.1,
        })
    }
}

// ---------------------------------------------------------------------------
// Page fetch (spike-local; the managed container net layer is task 7)

fn fetch_html(raw_url: &str) -> Result<(url::Url, String), Box<dyn Error>> {
    let url = url::Url::parse(raw_url)?;
    match url.scheme() {
        "file" => {
            let path = url
                .to_file_path()
                .map_err(|_| format!("not a usable file path: {url}"))?;
            let html = std::fs::read_to_string(path)?;
            Ok((url, html))
        }
        "http" | "https" => {
            let body = reqwest::blocking::get(url.clone())?
                .error_for_status()?
                .text()?;
            Ok((url, body))
        }
        other => Err(format!("spike does not fetch {other}: URLs").into()),
    }
}

// ---------------------------------------------------------------------------
// Main spike

pub fn run(args: &[String]) -> Result<(), Box<dyn Error>> {
    let args = SpikeArgs::parse(args)?;
    eprintln!(
        "servfox-tab spike: url={} out={} size={}",
        args.url,
        args.out,
        format_args!("{}x{}", args.width, args.height)
    );

    let (url, html) = fetch_html(&args.url)?;

    let scale = 1.0f32;
    let width = args.width;
    let height = args.height;

    let config = DocumentConfig {
        base_url: Some(url.as_str().to_owned()),
        viewport: Some(Viewport::new(
            (width as f32 * scale) as u32,
            (height as f32 * scale) as u32,
            scale,
            ColorScheme::Light,
        )),
        net_provider: Some(blitz_net::Provider::shared(None)),
        ..Default::default()
    };

    let mut document = HtmlDocument::from_html(&html, config);
    document.resolve(0.);
    // Give any (inline/no-network) document messages a chance to settle, then
    // re-resolve so late style/layout invalidations are painted.
    document.handle_messages();
    document.resolve(0.);

    let buffer = render_to_buffer::<VelloCpuImageRenderer, _>(
        |scene| {
            scene.fill(
                Fill::NonZero,
                Affine::IDENTITY,
                Color::from_rgb8(255, 255, 255),
                None,
                &Rect::from_origin_size((0.0, 0.0), Size::new(f64::from(width), f64::from(height))),
            );
            paint_scene(scene, &mut document, scale as f64, width, height, 0, 0);
        },
        width,
        height,
    );
    assert_eq!(
        buffer.len(),
        (width as usize) * (height as usize) * 4,
        "VelloCpuImageRenderer buffer must be w*h*4 RGBA8 bytes"
    );

    write_ppm(&args.out, width, height, &buffer)?;
    if let Some(path) = &args.out_rgba {
        let mut file = File::create(path)?;
        file.write_all(&buffer)?;
        file.flush()?;
        eprintln!("servfox-tab spike: wrote {path} ({} bytes raw RGBA8)", buffer.len());
    }
    eprintln!(
        "servfox-tab spike: wrote {} ({}x{}, {} bytes RGBA8)",
        args.out,
        width,
        height,
        buffer.len()
    );
    Ok(())
}

/// Write the RGBA8 buffer as a P6 (binary RGB) PPM.
fn write_ppm(path: &str, width: u32, height: u32, rgba: &[u8]) -> Result<(), Box<dyn Error>> {
    let mut file = File::create(path)?;
    write!(file, "P6\n{width} {height}\n255\n")?;
    let mut rgb = Vec::with_capacity((width * height * 3) as usize);
    for chunk in rgba.chunks_exact(4) {
        rgb.extend_from_slice(&chunk[..3]);
    }
    file.write_all(&rgb)?;
    file.flush()?;
    Ok(())
}
