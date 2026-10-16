//! Dependency-free stderr logging for every IronSurf process (task 13).
//!
//! **stderr only, always.** stdout is the binary IPC channel between the shell
//! and a tab (`crate::{read_frame, write_frame}`); a stray byte there corrupts
//! the stream, so log lines never go anywhere near stdout.
//!
//! ## Level control
//!
//! Resolution order (first hit wins):
//! 1. CLI: `--log <level>`, `--log=<level>`, `--verbose`/`-v` (debug) — see
//!    [`level_from_args`], which both binaries run over `std::env::args()`;
//! 2. env `IRONSURF_LOG` (accepted spellings: `off|error|warn|info|debug|trace`);
//! 3. env `RUST_LOG` (same grammar; `targets=debug` style tokens are ignored
//!    except a `*` global entry);
//! 4. [`DEFAULT_LEVEL`] = `info`.
//!
//! Because the shell spawns tabs with the environment inherited, setting
//! `IRONSURF_LOG=debug` once turns on debug output for the whole browser.
//!
//! ## Line format
//!
//! ```text
//! [14:22:07.418] INFO tab 3 runtime: navigate https://example.com/ (typed)
//!  └ UTC time-of-day   └ level  └ ctx  └ module  └ message
//! ```
//!
//! The context is set once per process by [`set_context`] (e.g. `"tab 3"`,
//! `"shell"`); the module segment is `module_path!()` with the crate name
//! stripped, so call sites stay terse.
//!
//! ## Volume contract (keep it safe to leave debug on)
//!
//! A tab's stderr is a *pipe* to the shell, and the shell does not read it —
//! so a flood can fill the 64 KiB pipe buffer and block the tab. Frame bytes,
//! pointer moves and per-pixel noise therefore belong at [`Level::Trace`],
//! never `info`.

use std::fmt::Arguments;
use std::io::Write;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Level used when neither CLI nor env asks for anything else.
pub const DEFAULT_LEVEL: Level = Level::Info;
/// Env var consulted before `RUST_LOG`.
pub const ENV_VAR: &str = "IRONSURF_LOG";

/// Severity of a log line (ascending). [`Level::Off`] sorts **below** every
/// severity on purpose: the configured maximum is compared as
/// `requested <= max`, so `max = Off` disables every line.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub enum Level {
    Off = 0,
    Error = 1,
    Warn = 2,
    Info = 3,
    Debug = 4,
    Trace = 5,
}

impl Level {
    pub fn as_str(self) -> &'static str {
        match self {
            Level::Off => "OFF",
            Level::Error => "ERROR",
            Level::Warn => "WARN",
            Level::Info => "INFO",
            Level::Debug => "DEBUG",
            Level::Trace => "TRACE",
        }
    }

    /// Parse one spelling (`off`/`none`/`silent`, `error`/`err`,
    /// `warn`/`warning`, `info`, `debug`, `trace`; case-insensitive).
    pub fn parse(raw: &str) -> Option<Level> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "off" | "none" | "silent" | "quiet" => Some(Level::Off),
            "error" | "err" | "fatal" | "1" => Some(Level::Error),
            "warn" | "warning" | "2" => Some(Level::Warn),
            "info" | "notice" | "3" => Some(Level::Info),
            "debug" | "4" => Some(Level::Debug),
            "trace" | "verbose" | "5" => Some(Level::Trace),
            _ => None,
        }
    }

    fn from_u8(v: u8) -> Level {
        match v {
            1 => Level::Error,
            2 => Level::Warn,
            3 => Level::Info,
            4 => Level::Debug,
            5..=u8::MAX => Level::Trace,
            _ => Level::Off,
        }
    }
}

static MAX_LEVEL: AtomicU8 = AtomicU8::new(DEFAULT_LEVEL as u8);
static CONTEXT: OnceLock<Mutex<String>> = OnceLock::new();

fn context_lock() -> &'static Mutex<String> {
    CONTEXT.get_or_init(|| Mutex::new(String::new()))
}

/// Highest level that will be written (lines above it are dropped, and their
/// `format_args!` are never evaluated).
pub fn level() -> Level {
    Level::from_u8(MAX_LEVEL.load(Ordering::Relaxed))
}

/// Set the maximum level (after [`Level::Off`], nothing is emitted).
pub fn set_level(level: Level) {
    MAX_LEVEL.store(level as u8, Ordering::Relaxed);
}

/// Prefix every line, e.g. `"tab 3"` or `"shell"`. Call once at startup.
pub fn set_context(tag: &str) {
    let mut ctx = context_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    ctx.clear();
    ctx.push_str(tag);
}

/// Current line prefix (empty until [`set_context`] is called).
pub fn context() -> String {
    context_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
}

/// Would a line at `level` be written right now?
pub fn enabled(requested: Level) -> bool {
    requested <= level()
}

/// Resolve the level from `--log <lvl>|--log=<lvl>|--verbose|--debug|-v`,
/// leaving `None` when the process was given no verbosity flag.
pub fn level_from_args(args: &[String]) -> Option<Level> {
    let mut it = args.iter();
    let mut found = None;
    while let Some(arg) = it.next() {
        if let Some(rest) = arg.strip_prefix("--log=") {
            found = Level::parse(rest);
        } else if arg == "--log" || arg == "--log-level" {
            found = it.next().and_then(|v| Level::parse(v));
        } else if matches!(arg.as_str(), "-v" | "-vv" | "--verbose" | "--debug") {
            found = Some(if arg == "-vv" {
                Level::Trace
            } else {
                Level::Debug
            });
        }
    }
    found
}

/// Read the level out of one spec (`IRONSURF_LOG`/`RUST_LOG` value). A comma
/// list is accepted; only a bare token or a `*`-keyed entry applies.
pub fn level_from_spec(spec: &str) -> Option<Level> {
    let mut found = None;
    for token in spec.split(',') {
        let token = token.trim();
        // Accepts `debug`, `*:debug`, `*=warn`, `ironsurf_tab=trace` (the
        // last is target-specific and intentionally ignored in v1).
        let (key, value) = match token.split_once(['=', ':']) {
            Some((k, v)) => (k.trim(), v.trim()),
            None => ("*", token),
        };
        if key == "*" || key.is_empty() {
            found = Level::parse(value).or(found);
        }
    }
    found
}

fn level_from_env() -> Option<Level> {
    [ENV_VAR, "RUST_LOG"]
        .iter()
        .filter_map(|key| std::env::var(key).ok())
        .find_map(|spec| level_from_spec(&spec))
}

/// One-shot startup: label the process and pick the level from `cli`
/// (`None` → env, `None` → [`DEFAULT_LEVEL`]).
pub fn init(context: &str, cli: Option<Level>) -> Level {
    set_context(context);
    let resolved = cli.or_else(level_from_env).unwrap_or(DEFAULT_LEVEL);
    set_level(resolved);
    resolved
}

/// `main()` convenience: label the process, then take the level from
/// `--log <lvl>` / `--log=<lvl>` / `-v` / `-vv` in the real argv, falling back
/// to `IRONSURF_LOG` / `RUST_LOG`, then to [`DEFAULT_LEVEL`]. Returns the level
/// actually applied (log it yourself if you want a confirmation line).
pub fn init_process(context: &str) -> Level {
    let args: Vec<String> = std::env::args().collect();
    init(context, level_from_args(&args))
}

/// Drop every log line (used by tests and by `--log off`).
pub fn silence() {
    set_level(Level::Off);
}

/// `ironsurf_tab::runtime` → `runtime`, `shell` → `shell`.
pub fn shorten_target(target: &str) -> &str {
    match target.find("::") {
        Some(i) => &target[i + 2..],
        None => target,
    }
}

/// UTC time-of-day with millisecond resolution (`HH:MM:SS.mmm`). UTC because
/// it costs no `libc` call and never shifts under DST.
pub fn clock(now: Duration) -> String {
    let day = now.as_secs() % 86_400;
    let (h, m, s) = (day / 3_600, day % 3_600 / 60, day % 60);
    format!("[{h:02}:{m:02}:{s:02}.{:03}]", now.subsec_millis())
}

/// Render one line (no trailing newline). Kept pure so it is testable
/// without touching the process-global level or the real stderr.
pub fn format_line(now: Duration, level: Level, context: &str, target: &str, msg: &str) -> String {
    let target = shorten_target(target);
    if context.is_empty() {
        format!("{} {} {target}: {msg}", clock(now), level.as_str())
    } else {
        format!(
            "{} {} {context} {target}: {msg}",
            clock(now),
            level.as_str()
        )
    }
}

/// Write one line to stderr when `level` passes [`level()`]. Prefer the
/// [`error!`] … [`trace!`] macros: they skip `format_args!` entirely when the
/// level is filtered out.
pub fn emit(level: Level, target: &str, msg: Arguments<'_>) {
    if !enabled(level) {
        return;
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO);
    let ctx = context();
    let mut line = format_line(now, level, &ctx, target, &msg.to_string());
    line.push('\n');
    // A single write_all under a mutex keeps lines from interleaving between
    // threads (tab fetch workers, the shell reader threads).
    let mut err = None;
    {
        let stderr = std::io::stderr();
        let mut sink = stderr.lock();
        if let Err(e) = sink.write_all(line.as_bytes()) {
            err = Some(e);
        }
        let _ = sink.flush();
    }
    if let Some(e) = err {
        // Never recurse into the logger about the logger.
        let _ =
            std::io::stderr().write_all(format!("ironsurf: stderr write failed: {e}\n").as_bytes());
    }
}

#[macro_export]
#[doc(hidden)]
macro_rules! __log_at {
    ($level:expr, $($arg:tt)+) => {{
        if $crate::logging::enabled($level) {
            $crate::logging::emit($level, module_path!(), format_args!($($arg)+));
        }
    }};
}

/// Error the user will see (navigation failure, tab died, protocol error).
#[macro_export]
macro_rules! error {
    ($($arg:tt)+) => { $crate::__log_at!($crate::logging::Level::Error, $($arg)+) };
}

/// Degraded-but-working (sandbox refused, cookie rejected, JS refused).
#[macro_export]
macro_rules! warn {
    ($($arg:tt)+) => { $crate::__log_at!($crate::logging::Level::Warn, $($arg)+) };
}

/// Notable state transitions (navigation, load start/finish, tab lifecycle).
#[macro_export]
macro_rules! info {
    ($($arg:tt)+) => { $crate::__log_at!($crate::logging::Level::Info, $($arg)+) };
}

/// Per-step detail (fetch dispatch/response, script counts, IPC commands).
#[macro_export]
macro_rules! debug {
    ($($arg:tt)+) => { $crate::__log_at!($crate::logging::Level::Debug, $($arg)+) };
}

/// Everything: frames, pointer moves, repaint decisions. Off unless asked
/// for — see the volume contract in the module docs.
#[macro_export]
macro_rules! trace {
    ($($arg:tt)+) => { $crate::__log_at!($crate::logging::Level::Trace, $($arg)+) };
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Level and context are process-global; serialize the tests that touch them.
    static SERIALIZED: Mutex<()> = Mutex::new(());

    fn dur(h: u64, m: u64, s: u64, ms: u64) -> Duration {
        Duration::from_secs(h * 3_600 + m * 60 + s) + Duration::from_millis(ms)
    }

    #[test]
    fn parses_level_spellings() {
        assert_eq!(Level::parse("OFF"), Some(Level::Off));
        assert_eq!(Level::parse(" error "), Some(Level::Error));
        assert_eq!(Level::parse("warning"), Some(Level::Warn));
        assert_eq!(Level::parse("Trace"), Some(Level::Trace));
        assert_eq!(Level::parse("nonsense"), None);
    }

    #[test]
    fn ordering_off_is_above_every_real_level() {
        assert!(Level::Error < Level::Warn && Level::Warn < Level::Info);
        assert!(Level::Info < Level::Debug && Level::Debug < Level::Trace);
        // Off is the zero cap: `requested <= Off` is false for every severity.
        assert!(Level::Off < Level::Error);
        assert!(!(Level::Error <= Level::Off));
    }

    #[test]
    fn clock_formats_utc_with_millis() {
        assert_eq!(clock(dur(14, 22, 7, 418)), "[14:22:07.418]");
        assert_eq!(clock(Duration::ZERO), "[00:00:00.000]");
        // Rolls over inside a day, never shows >23.
        assert_eq!(clock(Duration::from_secs(86_399)), "[23:59:59.000]");
    }

    #[test]
    fn format_line_with_and_without_context() {
        let line = format_line(
            dur(1, 2, 3, 4),
            Level::Info,
            "tab 3",
            "ironsurf_tab::runtime",
            "hello",
        );
        assert_eq!(line, "[01:02:03.004] INFO tab 3 runtime: hello");
        let no_ctx = format_line(
            dur(1, 2, 3, 4),
            Level::Warn,
            "",
            "ironsurf_shell::app",
            "gone",
        );
        // An empty context drops that field entirely (the shell may not have one yet).
        assert_eq!(no_ctx, "[01:02:03.004] WARN app: gone");
    }

    #[test]
    fn target_is_shortened_to_the_module() {
        assert_eq!(shorten_target("ironsurf_tab::runtime"), "runtime");
        assert_eq!(shorten_target("ironsurf_tab::netmod"), "netmod");
        assert_eq!(shorten_target("shell"), "shell");
    }

    #[test]
    fn env_spec_accepts_bare_star_and_ignores_targeted_noise() {
        assert_eq!(level_from_spec("debug"), Some(Level::Debug));
        assert_eq!(level_from_spec(" off "), Some(Level::Off));
        assert_eq!(level_from_spec("*:debug"), Some(Level::Debug));
        assert_eq!(level_from_spec("*=warn"), Some(Level::Warn));
        assert_eq!(level_from_spec("ironsurf_tab=trace"), None);
        assert_eq!(
            level_from_spec("ironsurf_tab=trace,warn"),
            Some(Level::Warn)
        );
    }

    #[test]
    fn cli_flags_win_over_default() {
        let args = |v: &[&str]| -> Vec<String> { v.iter().map(|s| s.to_string()).collect() };
        assert_eq!(
            level_from_args(&args(&["ironsurf-tab", "--log", "trace"])),
            Some(Level::Trace)
        );
        assert_eq!(
            level_from_args(&args(&["x", "--log=warn"])),
            Some(Level::Warn)
        );
        assert_eq!(level_from_args(&args(&["x", "-v"])), Some(Level::Debug));
        assert_eq!(level_from_args(&args(&["x", "-vv"])), Some(Level::Trace));
        assert_eq!(level_from_args(&args(&["x", "--url", "y"])), None);
    }

    #[test]
    fn init_sets_context_and_level() {
        let _guard = SERIALIZED.lock().unwrap_or_else(|p| p.into_inner());
        let resolved = init("tab 7", Some(Level::Debug));
        assert_eq!(resolved, Level::Debug);
        assert_eq!(level(), Level::Debug);
        assert_eq!(context(), "tab 7");
        assert!(enabled(Level::Debug));
        assert!(!enabled(Level::Trace));
        silence();
        assert!(!enabled(Level::Error));
        // Leave a sane level for the rest of the test binary.
        set_level(Level::Error);
    }

    #[test]
    fn emit_is_a_noop_when_filtered() {
        let _guard = SERIALIZED.lock().unwrap_or_else(|p| p.into_inner());
        set_level(Level::Error);
        // Nothing to assert beyond "does not panic / does not write"; a debug
        // line under an Error cap must return before formatting.
        assert!(!enabled(Level::Debug));
        emit(Level::Debug, "x", format_args!("dropped {0}", 1));
    }
}
