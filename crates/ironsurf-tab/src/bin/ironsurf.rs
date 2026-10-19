//! ironsurf — the one binary IronSurf ships and runs.
//!
//! Shell and tab host are linked into this single executable (Slint GUI from
//! `ironsurf-shell`, the Blitz + Boa engine host from `ironsurf_tab`), so a
//! release is a single file to download and run: `./ironsurf`.
//!
//! The shell still runs every tab in its own OS process — it re-executes
//! **itself** per tab (`ironsurf --tab-id N --container-id N --profile-dir P`,
//! stdio pipes, `ironsurf-common` framing) and the child lands in
//! [`ironsurf_tab::run`]. Which half a process is follows from argv:
//!
//! ```text
//! ./ironsurf [OPTIONS]                 → shell (the GUI)
//! ./ironsurf tab [TAB OPTIONS]         → tab host (the subcommand is explicit)
//! ./ironsurf --tab-id 1 …              → tab host (how the shell spawns children)
//! ./ironsurf-tab [TAB OPTIONS]         → tab host (argv[0] under the old name)
//! ```
//!
//! i.e. [`dispatch`] answers *tab* for an `ironsurf-tab` `argv[0]` (busybox
//! style: `ln -s ironsurf ironsurf-tab` works), for the `tab` subcommand, and
//! for a command line carrying a tab-only flag; everything else is the shell.

use std::path::Path;
use std::process::ExitCode;

use ironsurf_common::error;

/// Flags only the tab host accepts: seeing one anywhere means this process was
/// launched as a tab (which is exactly how the shell spawns its children, so
/// no `tab` subcommand has to be injected into the child's argv).
const TAB_MARKERS: &[&str] = &["--tab-id", "--headless", "--spike"];

/// Which half of the binary this process is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// The Slint shell — anything that is not explicitly a tab: no marker flag
    /// and no `tab` subcommand (unknown flags are the shell's business, and it
    /// ignores them exactly as it always did).
    Shell,
    /// One sandboxed tab host process.
    Tab,
}

impl Mode {
    pub fn is_tab(self) -> bool {
        self == Mode::Tab
    }
}

/// Decide the mode from `argv` and return it with the `argv` to hand that half
/// (`argv[0]` stays the program name, so both halves parse it the usual way).
///
/// An explicit `tab`/`shell` subcommand wins over sniffing and is consumed;
/// otherwise an `ironsurf-tab` program name or any [`TAB_MARKERS`] flag means
/// tab.
pub fn dispatch(argv: &[String]) -> (Mode, Vec<String>) {
    if argv.is_empty() {
        return (Mode::Shell, Vec::new());
    }
    let (program, rest) = argv.split_first().unwrap();
    // A copy or symlink named `ironsurf-tab` (the pre-merge name) is the tab
    // host, whatever it is asked for.
    if is_tab_program(program) {
        return (Mode::Tab, argv.to_vec());
    }
    // An explicit subcommand wins over sniffing, and is consumed on the way in.
    let subcommand = |mode: Mode| -> (Mode, Vec<String>) {
        let mut args = vec![program.clone()];
        args.extend_from_slice(&rest[1..]);
        (mode, args)
    };
    match rest.first().map(String::as_str) {
        Some("tab") => subcommand(Mode::Tab),
        Some("shell") => subcommand(Mode::Shell),
        _ if argv.iter().any(|a| TAB_MARKERS.contains(&a.as_str())) => (Mode::Tab, argv.to_vec()),
        _ => (Mode::Shell, argv.to_vec()),
    }
}

/// `argv[0]` names the tab host when it is `ironsurf-tab` (or a
/// `ironsurf-tab-<something>` build of it).
fn is_tab_program(program: &str) -> bool {
    let stem = Path::new(program)
        .file_stem()
        .map(|s| s.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    stem == "ironsurf-tab" || stem.starts_with("ironsurf-tab-")
}

/// `-V`/`--version` before dispatch, in either mode: a single binary deserves a
/// single version string (from the workspace crate that builds it), whichever
/// half you ask. `rest` is argv minus the program name.
fn wants_version(rest: &[String]) -> bool {
    rest.iter().any(|a| a == "-V" || a == "--version")
}

fn version() -> String {
    format!(
        "ironsurf {} (one binary: Slint shell + Blitz/Boa tab host)\n",
        env!("CARGO_PKG_VERSION")
    )
}

fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args().collect();
    let (mode, args) = dispatch(&argv);

    if args.len() > 1 && wants_version(&args[1..]) {
        print!("{}", version());
        return ExitCode::SUCCESS;
    }

    if mode.is_tab() {
        if args.iter().any(|a| a == "-h" || a == "--help") {
            print!("{}", ironsurf_tab::usage());
            return ExitCode::SUCCESS;
        }
        let code = ironsurf_tab::run(&args);
        // `_exit`, not `exit`: keep the shell-visible shutdown path identical
        // to the previous runtime (no atexit handlers, no teardown races).
        // Runtime state (cookie jars, the frame BufWriter) is flushed
        // explicitly by the runtime before we get here.
        // SAFETY: nothing else needs to run; stdout has been flushed.
        unsafe { libc::_exit(code) }
    }

    match ironsurf_shell::cli::run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            // `run` initialises logging before it can fail, so this lands as a
            // proper ERROR line (stderr) rather than a bare write.
            error!("fatal: {err}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(argv: &[&str]) -> Vec<String> {
        argv.iter().map(|a| a.to_string()).collect()
    }

    #[test]
    fn bare_invocation_and_shell_flags_are_the_shell() {
        assert_eq!(dispatch(&argv(&["ironsurf"])), (Mode::Shell, argv(&["ironsurf"])));
        // A synthetic argv (no `tab` token): everything the shell itself takes
        // stays shell.
        for rest in [
            vec!["--smoke"],
            vec!["-u", "https://example.com"],
            vec!["--software", "--log", "debug"],
            vec!["--tab-bin", "/tmp/fake_tab"],
            vec!["--help"],
        ] {
            let mut full = argv(&["ironsurf"]);
            full.extend(rest.iter().map(|s| s.to_string()));
            assert_eq!(dispatch(&full).0, Mode::Shell, "{rest:?}");
        }
    }

    #[test]
    fn the_tab_subcommand_selects_the_tab_host_and_is_consumed() {
        // The subcommand token never reaches the tab CLI (which rejects
        // unknown arguments) — the program name is the only leading token.
        let (mode, args) = dispatch(&argv(&["ironsurf", "tab", "--tab-id", "7"]));
        assert_eq!(mode, Mode::Tab);
        assert_eq!(args, argv(&["ironsurf", "--tab-id", "7"]));
        // …and `tab` alone is a usage error of the tab CLI, not a shell boot.
        assert_eq!(dispatch(&argv(&["ironsurf", "tab"])).0, Mode::Tab);
    }

    #[test]
    fn the_shell_subcommand_is_consumed_too() {
        let (mode, args) = dispatch(&argv(&["ironsurf", "shell", "--smoke"]));
        assert_eq!(mode, Mode::Shell);
        assert_eq!(args, argv(&["ironsurf", "--smoke"]));
    }

    #[test]
    fn tab_flags_alone_select_the_tab_host() {
        // How the shell actually spawns a child: no subcommand at all.
        let (mode, args) = dispatch(&argv(&[
            "ironsurf",
            "--tab-id",
            "3",
            "--container-id",
            "1",
            "--profile-dir",
            "/tmp/p",
        ]));
        assert!(mode.is_tab());
        assert_eq!(args.len(), 7);
        for flag in ["--headless", "--spike"] {
            assert_eq!(dispatch(&argv(&["ironsurf", flag, "--url", "x"])).0, Mode::Tab);
        }
    }

    #[test]
    fn an_ironsurf_tab_program_name_is_the_tab_host() {
        // Busybox style: `ln -s ironsurf ironsurf-tab && ./ironsurf-tab …`.
        let (mode, args) = dispatch(&argv(&["./ironsurf-tab", "--headless", "--url", "x"]));
        assert!(mode.is_tab());
        assert_eq!(args.len(), 4);
        // …and even with no arguments at all (the tab CLI reports its usage).
        assert!(dispatch(&argv(&["/usr/bin/ironsurf-tab"])).0.is_tab());
    }

    #[test]
    fn version_is_answered_before_the_shell_boots() {
        let a = argv(&["ironsurf", "--version"]);
        let (_, args) = dispatch(&a);
        assert!(wants_version(&args[1..]));
        assert!(version().starts_with("ironsurf "));
        assert!(!wants_version(&argv(&["--smoke"])));
    }
}
