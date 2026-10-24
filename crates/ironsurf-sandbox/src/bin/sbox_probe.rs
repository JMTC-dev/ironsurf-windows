//! `sbox-probe` — sandbox acceptance probe (test-support bin per ARCHITECTURE.md).
//!
//! Applies a real sandbox *in this process*, then checks the filesystem
//! expectations and exits `42` iff **all** of them hold:
//!
//! * every `--forbidden FILE` becomes unreadable (must exist *before*
//!   sandboxing, and be readable pre-sandbox — otherwise the probe is vacuous);
//! * every `--read FILE` under a `--ro` tree stays readable;
//! * creating a new file inside every `--writable DIR` succeeds.
//!
//! Exit codes:
//! * `42`  — all expectations hold (sandbox actively enforced);
//! * `40`  — Landlock was **not** enforced (absent kernel/container-blocked
//!   LSM): graceful-degradation path taken, expectations not testable;
//!   the probe prints `NOT-ENFORCED: <notes…>` lines so the caller can
//!   assert on them;
//! * `1`   — a setup/pre-flight error (bad args, forbidden file missing);
//! * `10`  — Landlock was enforced but at least one expectation FAILED
//!   (printed on stderr).
//!
//! On Windows "enforced" means the probe reached Low integrity, and the
//! forbidden-file expectation becomes *unwritable* (Low integrity is
//! no-write-up only: reads are confined by an AppContainer, stage 2 — the
//! probe reports them as a note). Windows also checks that the probe can
//! neither start a child process nor allocate executable memory.

use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;

const EXIT_OK: u8 = 42;
const EXIT_NOT_ENFORCED: u8 = 40;
const EXIT_SETUP: u8 = 1;
const EXIT_VIOLATION: u8 = 10;

#[derive(Default)]
struct Args {
    writable: Vec<PathBuf>,
    forbidden: Vec<PathBuf>,
    ro: Vec<PathBuf>,
    read: Vec<PathBuf>,
    /// Files that exist before sandboxing inside a writable dir and must stay
    /// writable (a profile's `cookies.json` from an earlier run).
    rewrite: Vec<PathBuf>,
    no_landlock: bool,
}

fn parse_args() -> Result<Args, String> {
    let mut a = Args::default();
    let mut it = std::env::args_os().skip(1);
    while let Some(arg) = it.next() {
        let flag = arg.to_string_lossy().into_owned();
        let mut next = |what: &str| -> Result<PathBuf, String> {
            it.next()
                .map(PathBuf::from)
                .ok_or_else(|| format!("{flag} requires {what}"))
        };
        match flag.as_str() {
            "--writable" => a.writable.push(next("DIR")?),
            "--forbidden" => a.forbidden.push(next("FILE")?),
            "--ro" => a.ro.push(next("DIR")?),
            "--read" => a.read.push(next("FILE")?),
            "--rewrite" => a.rewrite.push(next("FILE")?),
            "--no-landlock" => a.no_landlock = true,
            "--help" | "-h" => {
                println!(
                    "usage: sbox-probe --writable DIR --forbidden FILE [--ro DIR] [--read FILE] [--no-landlock]\n\
                     applies a sandbox in-process; exits {EXIT_OK} iff all expectations hold,\n\
                     {EXIT_NOT_ENFORCED} if Landlock was not enforced (graceful degradation)."
                );
                std::process::exit(0);
            }
            other => return Err(format!("unknown argument: {other}")),
        }
    }
    if a.writable.is_empty() || a.forbidden.is_empty() {
        return Err("at least one --writable DIR and one --forbidden FILE are required".into());
    }
    Ok(a)
}

fn main() -> ExitCode {
    // Re-exec under a *fresh* sandbox: if a test harness sandboxes the probe
    // itself, nested enforcement would confuse expectations. Not an error.
    let already = std::env::var("IRONSURF_SANDBOXED").is_ok();
    if already {
        eprintln!("sbox-probe: refusing to run nested-sandboxed; unset IRONSURF_SANDBOXED");
        return ExitCode::from(EXIT_SETUP);
    }

    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("sbox-probe: {e}");
            return ExitCode::from(EXIT_SETUP);
        }
    };

    // Pre-flight: every forbidden file must exist and be readable *before*
    // sandboxing, otherwise "reading it fails later" proves nothing.
    for f in &args.forbidden {
        match fs::read(f) {
            Ok(_) => {}
            Err(e) => {
                eprintln!("sbox-probe: pre-flight: forbidden file {} unreadable now ({e})", f.display());
                return ExitCode::from(EXIT_SETUP);
            }
        }
    }

    // Build the policy: default system read-only paths + caller's --ro dirs.
    let mut policy = ironsurf_sandbox::SandboxPolicy::default();
    policy.readable_ro.extend(args.ro.iter().cloned());
    if args.no_landlock {
        // Degradation-mode probe: request zero Landlock rules so `apply`
        // records the "left unenforced"/absent-kernel note and skips
        // enforcement, regardless of kernel support. rlimits + no_new_privs
        // still apply, which is exactly the fallback we want to verify.
        policy.readable_ro.clear();
        policy.writable_rw.clear();
    } else {
        policy.writable_rw = args.writable.clone();
        // The probe binary itself as an exec_ro entry exercises the exec
        // rule (a real tab passes its own executable here).
        if let Ok(exe) = std::env::current_exe() {
            policy.exec_ro.push(exe);
        }
    }

    let status = match ironsurf_sandbox::apply(&policy) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("sbox-probe: sandbox apply failed: {e}");
            return ExitCode::from(EXIT_SETUP);
        }
    };
    eprintln!("sbox-probe: apply -> {status:?}");

    if args.no_landlock {
        // Degradation expectations: no Landlock, but rlimits still applied.
        println!("NOT-ENFORCED: landlock_active={}", status.landlock_active);
        for n in &status.notes {
            println!("NOT-ENFORCED: {n}");
        }
        let degraded = !status.landlock_active
            && status.rlimits_applied
            && status
                .notes
                .iter()
                .any(|n| n.contains("NO_NEW_PRIVS") || n.contains("no prctl/Landlock"));
        if degraded {
            eprintln!("sbox-probe: graceful-degradation path verified");
            return ExitCode::from(EXIT_OK);
        }
        eprintln!("sbox-probe: degradation expectations FAILED");
        return ExitCode::from(EXIT_VIOLATION);
    }

    #[cfg(not(windows))]
    let enforced = status.landlock_active;
    #[cfg(windows)]
    let enforced = status.low_integrity;
    if !enforced {
        #[cfg(not(windows))]
        println!("NOT-ENFORCED: landlock_active=false (this kernel/container has no usable Landlock)");
        #[cfg(windows)]
        println!("NOT-ENFORCED: low_integrity=false");
        for n in &status.notes {
            println!("NOT-ENFORCED: {n}");
        }
        eprintln!("sbox-probe: sandbox not enforced — reporting graceful-degradation path");
        return ExitCode::from(EXIT_NOT_ENFORCED);
    }

    // Post-sandbox expectations.
    let mut failed = 0usize;

    #[cfg(not(windows))]
    for f in &args.forbidden {
        match fs::read(f) {
            Err(e) => eprintln!("  ok: reading forbidden {} denied ({e})", f.display()),
            Ok(_) => {
                eprintln!("  FAIL: reading forbidden {} unexpectedly SUCCEEDED", f.display());
                failed += 1;
            }
        }
    }
    #[cfg(windows)]
    {
        failed += windows_expectations(&args);
    }
    for f in &args.read {
        match fs::read(f) {
            Ok(_) => eprintln!("  ok: reading allowed {} succeeded", f.display()),
            Err(e) => {
                eprintln!("  FAIL: reading allowed {} denied ({e})", f.display());
                failed += 1;
            }
        }
    }
    for dir in &args.writable {
        let new = dir.join(format!("sbox-probe-created-{}", std::process::id()));
        match fs::File::create(&new).and_then(|mut f| f.write_all(b"sandboxed")) {
            Ok(()) => {
                eprintln!("  ok: created {} under writable {}", new.display(), dir.display());
                let _ = fs::remove_file(&new);
            }
            Err(e) => {
                eprintln!("  FAIL: creating file in writable {} denied ({e})", dir.display());
                failed += 1;
            }
        }
    }
    for f in &args.rewrite {
        match fs::OpenOptions::new().append(true).open(f).and_then(|mut h| h.write_all(b" ")) {
            Ok(()) => eprintln!("  ok: rewrote pre-existing {}", f.display()),
            Err(e) => {
                eprintln!("  FAIL: rewriting pre-existing {} denied ({e})", f.display());
                failed += 1;
            }
        }
    }

    if failed == 0 {
        eprintln!("sbox-probe: all expectations hold — sandbox enforced");
        ExitCode::from(EXIT_OK)
    } else {
        eprintln!("sbox-probe: {failed} expectation(s) FAILED");
        ExitCode::from(EXIT_VIOLATION)
    }
}

/// The Windows expectations beyond the shared writable-dir check; returns
/// how many failed.
#[cfg(windows)]
fn windows_expectations(args: &Args) -> usize {
    use windows_sys::Win32::System::Memory::{
        VirtualAlloc, VirtualFree, MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_EXECUTE_READWRITE,
    };

    let mut failed = 0usize;
    for f in &args.forbidden {
        // Low integrity is no-write-up only — reads are stage 2 (AppContainer).
        match fs::read(f) {
            Ok(_) => eprintln!(
                "  note: reading {} still works (reads are confined by an AppContainer, stage 2)",
                f.display()
            ),
            Err(e) => eprintln!("  note: reading {} denied ({e})", f.display()),
        }
        match fs::OpenOptions::new().append(true).open(f) {
            Err(e) => eprintln!("  ok: writing forbidden {} denied ({e})", f.display()),
            Ok(_) => {
                eprintln!("  FAIL: writing forbidden {} unexpectedly SUCCEEDED", f.display());
                failed += 1;
            }
        }
        if let Some(dir) = f.parent() {
            let escape = dir.join(format!("sbox-probe-escape-{}", std::process::id()));
            match fs::File::create(&escape) {
                Err(e) => eprintln!("  ok: creating {} denied ({e})", escape.display()),
                Ok(_) => {
                    let _ = fs::remove_file(&escape);
                    eprintln!("  FAIL: creating {} unexpectedly SUCCEEDED", escape.display());
                    failed += 1;
                }
            }
        }
    }

    let me = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("sbox-probe.exe"));
    match std::process::Command::new(&me)
        .arg("--help")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
    {
        Err(e) => eprintln!("  ok: starting a child process denied ({e})"),
        Ok(_) => {
            eprintln!("  FAIL: starting a child process unexpectedly SUCCEEDED");
            failed += 1;
        }
    }

    // SAFETY: a plain allocation request; freed below if it succeeds.
    let page = unsafe {
        VirtualAlloc(std::ptr::null(), 4096, MEM_COMMIT | MEM_RESERVE, PAGE_EXECUTE_READWRITE)
    };
    if page.is_null() {
        eprintln!(
            "  ok: executable memory denied (dynamic code prohibited: {})",
            std::io::Error::last_os_error()
        );
    } else {
        // SAFETY: the page we were just given.
        unsafe { VirtualFree(page, 0, MEM_RELEASE) };
        eprintln!("  FAIL: allocating executable memory unexpectedly SUCCEEDED");
        failed += 1;
    }
    failed
}
