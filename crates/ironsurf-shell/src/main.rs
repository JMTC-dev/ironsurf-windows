//! ironsurf-shell — Slint GUI for IronSurf.
//!
//! Usage:
//!   ironsurf-shell [--smoke] [--managed-net] [--no-sandbox]
//!                 [--fake-tab] [--tab-bin PATH] [--software] [--linuxkms]
//!                 [--log LEVEL|--verbose|-v] [--tab-stderr|--no-tab-stderr]
//!
//! Logging: every line the shell (and, when inherited, every tab) writes goes
//! to **stderr** — stdout stays untouched. `--log off|error|warn|info|debug|
//! trace` (or `IRONSURF_LOG`) picks the level; the level is forwarded to tab
//! children, and `--tab-stderr` lets their lines reach this terminal.
//!
//! Renderer: **GPU first, software last**. The crate is built with Slint's
//! LinuxKMS backend (`backend-linuxkms-noseat`), so on a bare virtual console
//! the shell renders fullscreen through DRM/KMS with EGL/GBM-accelerated
//! FemtoVG; on an X11/Wayland session winit uses FemtoVG/GL whenever a GPU
//! stack answers. Where no GL exists (this fleet's headless boxes) Slint
//! falls back to its software renderer automatically — `--software` pins
//! that path, `--linuxkms` pins the KMS backend, and an explicit
//! `SLINT_BACKEND` (e.g. `linuxkms/femtovg`, `winit/software`) always wins.

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use ironsurf_common::logging;
use ironsurf_shell::app::App;
use ironsurf_shell::tabs::{set_tab_stderr, TabStderr};
use ironsurf_shell::AppWindow;
use ironsurf_common::{error, info, warn};
use slint::ComponentHandle;

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            // `run` initialises logging before it can fail, so this lands as a
            // proper ERROR line (stderr) rather than a bare write.
            error!("fatal: {err}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    let flag = |f: &str| args.iter().any(|a| a == f);
    let smoke = flag("--smoke");

    // Level from --log/-v, then IRONSURF_LOG/RUST_LOG, then `info`.
    let level = logging::level_from_args(&args)
        .or_else(|| {
            [logging::ENV_VAR, "RUST_LOG"]
                .iter()
                .filter_map(|k| std::env::var(*k).ok())
                .find_map(|spec| logging::level_from_spec(&spec))
        })
        .unwrap_or(logging::DEFAULT_LEVEL);
    logging::init("shell", Some(level));
    info!(
        "ironsurf-shell starting: smoke={} managed-net={} no-sandbox={} fake-tab={} log {}",
        smoke,
        flag("--managed-net"),
        flag("--no-sandbox"),
        flag("--fake-tab"),
        level.as_str(),
    );

    // Tab children inherit our environment; the explicit flag also relays the
    // level so `ironsurf-shell --log debug` traces the whole browser.
    let mut extra_tab_args: Vec<String> = Vec::new();
    if flag("--managed-net") {
        extra_tab_args.push("--managed-net".into());
    }
    if flag("--no-sandbox") {
        extra_tab_args.push("--no-sandbox".into());
    }
    if logging::level_from_args(&args).is_some() {
        extra_tab_args.push("--log".into());
        extra_tab_args.push(level.as_str().to_ascii_lowercase());
    }
    if flag("--tab-stderr") {
        set_tab_stderr(TabStderr::Inherit);
    } else if flag("--no-tab-stderr") {
        set_tab_stderr(TabStderr::Null);
    }

    let tab_bin = match args.iter().position(|a| a == "--tab-bin") {
        Some(i) => args
            .get(i + 1)
            .cloned()
            .map(PathBuf::from)
            .ok_or("--tab-bin requires a path")?,
        None => resolve_tab_bin(flag("--fake-tab"))?,
    };
    info!("tab binary: {}", tab_bin.display());

    // GPU-first renderer policy, decided before the first window exists; an
    // explicit SLINT_BACKEND (e.g. "linuxkms", "winit/software") always wins.
    match std::env::var("SLINT_BACKEND") {
        Ok(spec) => info!("renderer: SLINT_BACKEND={spec} (explicit)"),
        Err(_) => match backend_choice(flag("--software"), flag("--linuxkms")) {
            // Slint's default chain: winit → FemtoVG/GL (auto-falls back to
            // the software renderer on GL-less displays); if no display
            // server exists, LinuxKMS → EGL/GBM FemtoVG (software fallback).
            None => info!("renderer: auto (GPU first: FemtoVG on winit/LinuxKMS, software fallback)"),
            Some(choice) => {
                let mut selector = slint::BackendSelector::new();
                if let Some(backend) = choice.backend {
                    selector = selector.backend_name(backend.into());
                }
                if let Some(renderer) = choice.renderer {
                    selector = selector.renderer_name(renderer.into());
                }
                match selector.select() {
                    Ok(()) => info!(
                        "renderer: pinned {}", choice.describe()
                    ),
                    // Selector refused the combination; the default chain has
                    // not been pinned and still runs (software catches us).
                    // A pinned backend that only fails LATER (at window
                    // creation, e.g. no DRM device) is an honest fatal error —
                    // the user asked for that backend by name.
                    Err(err) => warn!("renderer: request {} ignored: {err}", choice.describe()),
                }
            }
        },
    }

    // Slint 1.17 exposes no "which renderer won" query, so the policy above is
    // everything we can honestly log before the window exists.
    let ui = AppWindow::new()?;
    let app = App::new(&ui, tab_bin, extra_tab_args)?;

    app.start(if smoke { Some("about:blank") } else { None });

    ui.window().set_size(slint::LogicalSize::new(1024.0, 768.0));
    ui.window().show()?;

    if smoke {
        // Smoke contract: window is up, about:blank loaded, exit 0 after 3s
        // regardless of whether a frame arrived (CI boxes can lack engine
        // runtime deps; spawn failure is reported in the status bar, not fatal).
        let timer = slint::Timer::default();
        timer.start(slint::TimerMode::SingleShot, Duration::from_millis(3000), move || {
            let _ = slint::quit_event_loop();
        });
    }

    slint::run_event_loop()?;
    app.shutdown();
    Ok(())
}

/// A pinned backend/renderer combination for [`BackendSelector`]; `None` in a
/// field means "let Slint choose".
#[derive(Debug, PartialEq, Eq)]
struct BackendChoice {
    backend: Option<&'static str>,
    renderer: Option<&'static str>,
}

impl BackendChoice {
    fn describe(&self) -> String {
        format!(
            "{}/{}",
            self.backend.unwrap_or("auto-backend"),
            self.renderer.unwrap_or("auto-renderer")
        )
    }
}

/// Pure renderer policy from CLI flags. `None` = defer to Slint's default
/// chain (GPU first — FemtoVG on winit or LinuxKMS — software last); `Some`
/// = pin via `BackendSelector` (`--software` → software renderer, the old
/// forced path; `--linuxkms` → the DRM/KMS GPU backend).
fn backend_choice(software: bool, linuxkms: bool) -> Option<BackendChoice> {
    match (software, linuxkms) {
        (false, false) => None,
        (true, false) => Some(BackendChoice { backend: None, renderer: Some("software") }),
        (false, true) => Some(BackendChoice { backend: Some("linuxkms"), renderer: None }),
        (true, true) => Some(BackendChoice {
            backend: Some("linuxkms"),
            renderer: Some("software"),
        }),
    }
}

/// Locate the tab host binary: next to our own executable first (workspace
/// `target/<profile>/` layout puts both side by side), then `PATH`.
/// `prefer_fake` (dev escape hatch) tries the scripted `fake_tab` first.
fn resolve_tab_bin(prefer_fake: bool) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let real = std::env::consts::EXE_SUFFIX;
    let names: Vec<String> = {
        let mut v = vec![format!("ironsurf-tab{real}"), format!("fake_tab{real}")];
        if prefer_fake {
            v.reverse();
        }
        v
    };
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            for name in &names {
                let candidate = dir.join(name);
                if candidate.is_file() {
                    return Ok(candidate);
                }
            }
        }
    }
    // Fall back to a PATH lookup by bare name (Command::new does this).
    Ok(PathBuf::from(&names[0]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renderer_policy_defaults_to_gpu_first_auto() {
        // No flags → Slint's default chain (FemtoVG GPU where available,
        // software only as automatic fallback); never an empty pin.
        assert_eq!(backend_choice(false, false), None);
        assert_eq!(
            backend_choice(true, false).unwrap().renderer,
            Some("software")
        );
        assert_eq!(
            backend_choice(false, true).unwrap().backend,
            Some("linuxkms")
        );
        let both = backend_choice(true, true).unwrap();
        assert_eq!(both.backend, Some("linuxkms"));
        assert_eq!(both.renderer, Some("software"));
    }

    #[test]
    fn tab_bin_resolution_prefers_sibling_real_tab() {
        // This test binary lives in target/debug alongside ironsurf-tab once
        // built; but it must not require ironsurf-tab, so just assert the
        // fallback is a bare resolvable name.
        let p = resolve_tab_bin(false).unwrap();
        let name = p.file_name().unwrap().to_string_lossy().to_string();
        assert!(name == "ironsurf-tab" || name.starts_with("ironsurf-tab"));
    }
}
