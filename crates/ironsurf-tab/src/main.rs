//! ironsurf-tab — sandboxed per-tab Blitz host process.
//!
//! The full `ToTab`/`FromTab` IPC lifecycle on Blitz (task 7c): OS sandbox
//! at startup, one `HtmlDocument` per navigation driven on a single thread,
//! `resolve` → `vello_cpu` paint → `FromTab::Frame` at ≤30 fps, managed
//! container networking through `netmod`. Input events route through the
//! `runtime::Tab::handle_input` extension point (link clicks + full input
//! semantics land in task 7d). The standalone render spike ([`spike`]) is
//! kept behind `--spike`.

mod forms;
mod headless;
mod js;
mod logging;
mod media;
mod netmod;
mod runtime;
mod spike;

// (The stderr logger arrives under an alias: `logging` is this crate's own
// DevTools-console module — src/logging.rs.)
use ironsurf_common::logging as stderr_logging;
use ironsurf_common::error;

fn usage() -> String {
    "usage: ironsurf-tab --tab-id N --container-id N --profile-dir PATH \
     [--url URL] [--offline-fixtures DIR] [--managed-net] [--no-sandbox] [--log LEVEL|-v|-vv]\n\
     \x20      ironsurf-tab --spike --url <file:///...> --out <out.ppm> [--size WxH] [--out-rgba out.rgba]\n\
     \x20      ironsurf-tab --headless --url URL [--wait MS] [--body-out FILE] [--shot out.ppm] [--size WxH]\n\
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

fn main() {
    install_crash_hook();
    let args: Vec<String> = std::env::args().collect();
    // Logging first: every line after this goes to stderr (never stdout, which
    // is the IPC channel). `runtime::run` relabels the lines with the tab id.
    stderr_logging::init("tab", stderr_logging::level_from_args(&args));
    let code = if args.iter().any(|a| a == "--headless") {
        // One-shot ACID-test harness: navigate, wait, dump body HTML + PPM,
        // summary on stderr, stdout unused (see `crate::headless`).
        match headless::run(&args) {
            Ok(()) => 0,
            Err(err) => {
                error!("headless run failed: {err}");
                1
            }
        }
    } else if args.iter().any(|a| a == "--spike") {
        match spike::run(&args) {
            Ok(()) => 0,
            Err(err) => {
                error!("spike failed: {err}");
                1
            }
        }
    } else {
        match runtime::TabArgs::parse(&args) {
            Ok(tab_args) => runtime::run(tab_args),
            Err(err) => {
                error!("bad arguments: {err}\n{}", usage());
                2
            }
        }
    };
    // `_exit`, not `exit`: keep the shell-visible shutdown path identical to
    // the previous runtime (no atexit handlers, no teardown races). Runtime
    // state (cookie jars, the frame BufWriter) is flushed explicitly by the
    // runtime before we get here.
    // SAFETY: nothing else needs to run; stdout has been flushed.
    unsafe { libc::_exit(code) }
}
