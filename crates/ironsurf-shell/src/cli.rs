//! The shell half of the single `ironsurf` binary: CLI parsing, renderer
//! choice and the Slint boot (this is [`run`], called by `ironsurf`'s `main`).
//!
//! Usage:
//!   ironsurf [-h|--help] [-u|--url URL] [--smoke] [--managed-net]
//!            [--no-sandbox] [--fake-tab] [--tab-bin PATH]
//!            [-V|--version] [--software] [--linuxkms]
//!            [--log LEVEL|--verbose|-v] [--tab-stderr|--no-tab-stderr]
//!
//! `-h`/`--help` prints [`USAGE`] and exits 0; `-u`/`--url` picks the start
//! address instead of the default start page (`ironsurf://welcome`), with
//! `--smoke` still winning to keep the smoke contract on `about:blank`.
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

use std::path::{Path, PathBuf};
use std::time::Duration;

use ironsurf_common::logging;
use crate::app::App;
use crate::tabs::{set_tab_stderr, TabStderr};
use crate::AppWindow;
use ironsurf_common::{info, warn};
use slint::ComponentHandle;

/// The full CLI help, printed verbatim for `-h`/`--help`.
pub const USAGE: &str = "\
ironsurf — the IronSurf browser (one binary: Slint shell + Blitz/Boa tab host)

USAGE:
    ironsurf [OPTIONS]              start the shell (the GUI)
    ironsurf tab [TAB-OPTIONS]      run one tab host process — what the shell
                                    does per tab; see `ironsurf tab --help`

OPTIONS:
    -u, --url URL         open URL at startup instead of the default start page
                          (ironsurf://welcome); --smoke overrides it
    -h, --help            print this help and exit
        --smoke           smoke-test boot: load about:blank, exit 0 after ~3 s
        --managed-net     tabs fetch through the shell-managed network module
        --no-sandbox      start tabs without the sandbox (dev/debug escape hatch)
        --fake-tab        drive the scripted fake tab instead of the real engine
                          (dev harness; the IPC contract exercised unchanged)
        --tab-bin PATH    use PATH as the tab host binary (default: this same
                          executable — every tab is a copy of `ironsurf`)
        -V, --version     print the version and exit
        --software        pin Slint's software renderer (no GPU required)
        --linuxkms        pin the DRM/KMS backend (bare virtual console)
        --log LEVEL       logging level: off|error|warn|info|debug|trace
                          (default info; also IRONSURF_LOG/RUST_LOG)
        --verbose, -v     shortcut for --log debug
        --tab-stderr      let tab stderr lines reach this terminal
        --no-tab-stderr   suppress them (default when a level is set)

ENVIRONMENT:
    IRONSURF_DATA_DIR     profile/container data root (default ~/.local/share/ironsurf)
    SLINT_BACKEND         explicit Slint backend/renderer, wins over the flags
                          (e.g. linuxkms/femtovg, winit/software)
";

/// `-h`/`--help` anywhere in the arguments (first match, like every flag here).
pub fn wants_help(args: &[String]) -> bool {
    args.iter().any(|a| a == "-h" || a == "--help")
}

/// Boot the shell. `args` is the full argv (`args[0]` = program name).
///
/// Errors are fatal start-up errors (bad flags, no store, no window); the
/// binary's `main` logs them and exits non-zero.
pub fn run(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    // Help is pure output: no logging init, no window, no tab.
    if wants_help(args) {
        print!("{USAGE}");
        return Ok(());
    }
    let flag = |f: &str| args.iter().any(|a| a == f);
    let smoke = flag("--smoke");
    // `-u`/`--url` picks the start page; validated up front so a bare
    // `ironsurf -u` is a usage error, not a silently opened welcome.
    let url = start_url_from(args)?;

    // Level from --log/-v, then IRONSURF_LOG/RUST_LOG, then `info`.
    let level = logging::level_from_args(args)
        .or_else(|| {
            [logging::ENV_VAR, "RUST_LOG"]
                .iter()
                .filter_map(|k| std::env::var(*k).ok())
                .find_map(|spec| logging::level_from_spec(&spec))
        })
        .unwrap_or(logging::DEFAULT_LEVEL);
    logging::init("shell", Some(level));
    info!(
        "ironsurf shell starting: smoke={} url={:?} managed-net={} no-sandbox={} fake-tab={} log {}",
        smoke,
        url.as_deref(),
        flag("--managed-net"),
        flag("--no-sandbox"),
        flag("--fake-tab"),
        level.as_str(),
    );

    // Tab children inherit our environment; the explicit flag also relays the
    // level so `ironsurf --log debug` traces the whole browser.
    let mut extra_tab_args: Vec<String> = Vec::new();
    if flag("--managed-net") {
        extra_tab_args.push("--managed-net".into());
    }
    if flag("--no-sandbox") {
        extra_tab_args.push("--no-sandbox".into());
    }
    if logging::level_from_args(args).is_some() {
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

    // The smoke contract is an about:blank boot no matter what URL was given.
    app.start(if smoke { Some("about:blank") } else { url.as_deref() });

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

/// The start URL from `-u URL` / `--url URL` (first match wins).
/// `Ok(None)` = no flag, open the default start page; `Err` = the flag came
/// without a value (or with what is clearly the *next* flag), a usage error.
fn start_url_from(args: &[String]) -> Result<Option<String>, &'static str> {
    for (i, arg) in args.iter().enumerate() {
        if arg == "-u" || arg == "--url" {
            return match args.get(i + 1) {
                Some(value) if !value.is_empty() && !value.starts_with('-') => {
                    Ok(Some(value.clone()))
                }
                _ => Err("-u/--url requires a URL"),
            };
        }
    }
    Ok(None)
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

/// Locate the tab host binary.
///
/// The single `ironsurf` binary hosts both halves of the browser, so the
/// default is **our own executable**: each tab is another copy of the same
/// file, launched with `--tab-id …` (see the `ironsurf` binary's `dispatch`).
/// That is what keeps a release down to one downloaded file — no sibling
/// `ironsurf-tab` to fetch and no way for the two halves to drift.
///
/// `prefer_fake` (the `--fake-tab` dev escape hatch) instead drives the
/// scripted harness; `--tab-bin PATH` (handled by the caller) overrides both
/// and may name this very binary again.
fn resolve_tab_bin(prefer_fake: bool) -> Result<PathBuf, Box<dyn std::error::Error>> {
    if prefer_fake {
        // Not found next to us → the bare name, so the spawn fails loudly
        // (`cannot spawn fake_tab`) instead of quietly running the real engine.
        return Ok(find_sibling("fake_tab"));
    }
    Ok(std::env::current_exe()?)
}

/// `name` next to our own executable, then a couple of levels up (integration
/// tests run from `target/<profile>/deps/` while the workspace binaries sit in
/// `target/<profile>`), else the bare name for a `PATH` lookup.
fn find_sibling(name: &str) -> PathBuf {
    let name = format!("{name}{}", std::env::consts::EXE_SUFFIX);
    if let Ok(exe) = std::env::current_exe() {
        let mut dir = exe.parent().map(Path::to_path_buf);
        // Bounded walk: sibling, parent, grandparent — never the whole route.
        for _ in 0..3 {
            let Some(d) = dir else { break };
            let candidate = d.join(&name);
            if candidate.is_file() {
                return candidate;
            }
            dir = d.parent().map(Path::to_path_buf);
        }
    }
    PathBuf::from(name)
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
    fn help_is_answered_for_both_spellings() {
        let argv = |argv: &[&str]| -> Vec<String> {
            argv.iter().map(|a| a.to_string()).collect()
        };
        assert!(wants_help(&argv(&["ironsurf", "-h"])));
        assert!(wants_help(&argv(&["ironsurf", "--help"])));
        assert!(wants_help(&argv(&["ironsurf", "--smoke", "-h"])));
        assert!(!wants_help(&argv(&["ironsurf", "-u", "https://example.com"])));
        // The usage text stays in sync with the flags this binary accepts —
        // every long flag named in it must exist as a string above too.
        assert!(USAGE.contains("-u, --url URL"));
        assert!(USAGE.contains("-h, --help"));
        for flag in [
            "--smoke",
            "--managed-net",
            "--no-sandbox",
            "--fake-tab",
            "--tab-bin",
            "--software",
            "--linuxkms",
            "--log",
            "--tab-stderr",
            "--version",
        ] {
            assert!(USAGE.contains(flag), "help must document {flag}");
        }
        // …and the tab half of the one binary is advertised too.
        assert!(USAGE.contains("ironsurf tab [TAB-OPTIONS]"));
    }

    #[test]
    fn start_url_reads_u_and_long_url_flags() {
        let parse = |argv: &[&str]| {
            let args: Vec<String> = argv.iter().map(|a| a.to_string()).collect();
            start_url_from(&args).map(|url| url.as_deref().map(str::to_owned))
        };
        // No flag → the default start page.
        assert_eq!(parse(&["ironsurf", "--software"]), Ok(None));
        // Both spellings, and the value is taken verbatim (any scheme the
        // shell's URL contract accepts: https, about, file, data…).
        assert_eq!(
            parse(&["ironsurf", "--url", "https://example.com/x"]),
            Ok(Some("https://example.com/x".to_string()))
        );
        assert_eq!(
            parse(&["ironsurf", "-u", "about:blank"]),
            Ok(Some("about:blank".to_string()))
        );
        // The first occurrence wins over later ones.
        assert_eq!(
            parse(&["ironsurf", "-u", "https://a.test", "--url", "https://b.test"]),
            Ok(Some("https://a.test".to_string()))
        );
        // A flag without a value — missing or followed by another flag — is a
        // usage error, never a silently ignored welcome page.
        assert!(parse(&["ironsurf", "-u"]).is_err());
        assert!(parse(&["ironsurf", "--url", "--smoke"]).is_err());
        assert!(parse(&["ironsurf", "--url", ""]).is_err());
    }

    #[test]
    fn tab_host_defaults_to_this_very_executable() {
        // The single-binary contract: tabs are copies of ourselves, so the
        // default tab binary is our own path — no sibling to install.
        let p = resolve_tab_bin(false).unwrap();
        assert_eq!(p, std::env::current_exe().unwrap());
    }

    #[test]
    fn fake_tab_is_looked_for_next_to_us_then_on_path() {
        // The dev harness sits in target/<profile>/ while this test binary runs
        // from target/<profile>/deps/, so the upward walk finds it when built;
        // if it is not built the answer is the bare name (a loud spawn error).
        let p = resolve_tab_bin(true).unwrap();
        let name = p.file_name().unwrap().to_string_lossy().to_string();
        assert!(name == "fake_tab" || name == format!("fake_tab{}", std::env::consts::EXE_SUFFIX));
        if p != PathBuf::from(name.clone()) {
            assert!(p.is_file(), "{} exists or was not chosen", p.display());
        }
    }
}
