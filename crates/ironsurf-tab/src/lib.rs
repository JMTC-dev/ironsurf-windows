//! ironsurf-tab — the sandboxed per-tab Blitz + Boa engine host.
//!
//! This crate is the tab half of the single `ironsurf` binary (see
//! [`src/bin/ironsurf.rs`], whose `main` calls [`run`]): the same executable
//! that shows the Slint shell re-executes *itself* once per tab, and the copy
//! that gets `--tab-id …` comes through here as the tab host process.
//!
//! The full `ToTab`/`FromTab` IPC lifecycle on Blitz (task 7c): OS sandbox
//! at startup, one `HtmlDocument` per navigation driven on a single thread,
//! `resolve` → `vello_cpu` paint → `FromTab::Frame` at ≤30 fps, managed
//! container networking through `netmod`. Input events route through the
//! `runtime::Tab::handle_input` extension point (link clicks + full input
//! semantics land in task 7d). The standalone render spike ([`spike`]) is
//! kept behind `--spike`.

mod archive;
mod downloads;
mod find;
mod forms;
mod headless;
mod inspector;
mod js;
mod logging;
mod media;
mod netmod;
mod popup;
mod private_addr;
mod reader;
mod runtime;
mod select;
mod sha256;
mod spike;
mod worker;

// (The stderr logger arrives under an alias: `logging` is this crate's own
// DevTools-console module — src/logging.rs.)
use ironsurf_common::logging as stderr_logging;
use ironsurf_common::error;

/// The tab host's CLI help (also what `ironsurf tab --help` prints, with the
/// `ironsurf tab` invocation spelled out by the caller).
pub fn usage() -> String {
    "usage: ironsurf tab --tab-id N --container-id N --profile-dir PATH \
     [--url URL] [--offline-fixtures DIR] [--user-agent UA] [--archive-mode] [--managed-net] [--no-sandbox] [--log LEVEL|-v|-vv]\n\
     \x20      ironsurf tab --spike --url <file:///...> --out <out.ppm> [--size WxH] [--out-rgba out.rgba]\n\
     \x20      ironsurf tab --headless --url URL [--wait MS] [--body-out FILE] [--shot out.ppm] [--size WxH] [--find QUERY] [--reader]\n\
     \x20      log LEVEL: off|error|warn|info|debug|trace (default info; also IRONSURF_LOG)"
        .to_owned()
}

/// Crash loudly: the default hook only prints a “run with RUST_BACKTRACE=1”
/// hint when the env is unset, and the shell captures (rather than shows) our
/// stderr — so without this a panicking tab would look like it died silently.
/// The backtrace lands on stderr, which the shell keeps as the crash report.
fn install_crash_hook() {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        default(info);
        eprintln!("tab process {} crashing: {info}", std::process::id());
        eprintln!(
            "stack backtrace:\n{}",
            std::backtrace::Backtrace::force_capture()
        );
    }));
}

/// Run one tab host process: `argv[0]` is the program name, the rest the tab
/// CLI (see [`usage`]). Returns the exit code the caller must pass to
/// `libc::_exit` — never `std::process::exit`, see the binary's `main`.
///
/// Spelled as a function (rather than `main`) so the single `ironsurf` binary
/// can host both halves: the shell re-executes itself per tab and the child
/// lands here.
pub fn run(args: &[String]) -> i32 {
    install_crash_hook();
    // Logging first: every line after this goes to stderr (never stdout, which
    // is the IPC channel). `runtime::run` relabels the lines with the tab id.
    stderr_logging::init("tab", stderr_logging::level_from_args(args));
    if args.iter().any(|a| a == "--headless") {
        // One-shot ACID-test harness: navigate, wait, dump body HTML + PPM,
        // summary on stderr, stdout unused (see `crate::headless`).
        match headless::run(args) {
            Ok(()) => 0,
            Err(err) => {
                error!("headless run failed: {err}");
                1
            }
        }
    } else if args.iter().any(|a| a == "--spike") {
        match spike::run(args) {
            Ok(()) => 0,
            Err(err) => {
                error!("spike failed: {err}");
                1
            }
        }
    } else {
        match runtime::TabArgs::parse(args) {
            Ok(tab_args) => runtime::run(tab_args),
            Err(err) => {
                error!("bad arguments: {err}\n{}", usage());
                2
            }
        }
    }
}
