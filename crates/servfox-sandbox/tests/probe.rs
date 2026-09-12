//! Integration tests for the sandbox: spawn the `sbox-probe` helper with
//! tempdir-backed "container profiles" and check it *enforces* (exit 42) or,
//! when the kernel/container has no usable Landlock, *degrades gracefully*
//! (exit 40 + notes). See `src/bin/sbox_probe.rs` for the probe protocol.

use std::fs;
use std::path::Path;
use std::process::Command;

const EXIT_OK: i32 = 42;
const EXIT_NOT_ENFORCED: i32 = 40;

/// Lay out:
///   <tmp>/profile-A          — the sandboxed tab's writable profile
///   <tmp>/profile-B/secret   — another container's data (must become unreadable)
///   <tmp>/ro-tree/hello.txt  — system-ish read-only tree (stays readable)
struct Fixture {
    root: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().expect("tempdir");
        let p = root.path();
        fs::create_dir_all(p.join("profile-A")).unwrap();
        fs::create_dir_all(p.join("profile-B")).unwrap();
        fs::create_dir_all(p.join("ro-tree")).unwrap();
        fs::write(p.join("profile-B/secret.txt"), b"another container's cookies").unwrap();
        fs::write(p.join("ro-tree/hello.txt"), b"hello").unwrap();
        Fixture { root }
    }

    fn writable(&self) -> std::path::PathBuf {
        self.root.path().join("profile-A")
    }
    fn forbidden(&self) -> std::path::PathBuf {
        self.root.path().join("profile-B/secret.txt")
    }
    fn ro(&self) -> std::path::PathBuf {
        self.root.path().join("ro-tree")
    }
    fn ro_file(&self) -> std::path::PathBuf {
        self.root.path().join("ro-tree/hello.txt")
    }
}

fn probe() -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_sbox-probe"));
    // Never inherit a sandboxed environment from a parent harness.
    cmd.env_remove("SERVFOX_SANDBOXED");
    cmd
}

fn probe_args(cmd: &mut Command, fx: &Fixture) {
    cmd.arg("--writable")
        .arg(fx.writable())
        .arg("--forbidden")
        .arg(fx.forbidden())
        .arg("--ro")
        .arg(fx.ro())
        .arg("--read")
        .arg(fx.ro_file());
}

fn stdout_of(out: &std::process::Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}
fn stderr_of(out: &std::process::Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn assert_graceful_notes(stdout: &str) {
    // The degradation contract: Landlock off, but notes explain it and the
    // rlimits/no_new_privs fallback was still applied.
    let notes: Vec<&str> = stdout
        .lines()
        .filter(|l| l.starts_with("NOT-ENFORCED:"))
        .collect();
    assert!(
        !notes.is_empty(),
        "expected NOT-ENFORCED notes, got: {stdout}"
    );
    let all = notes.join("\n");
    assert!(
        all.contains("landlock_active=false")
            || all.contains("left unenforced")
            || all.contains("unavailable")
            || all.contains("NotEnforced")
            || all.contains("degraded"),
        "notes should explain the Landlock fallback: {all}"
    );
    assert!(
        all.contains("NO_NEW_PRIVS") && all.contains("rlimit"),
        "fallback must still record no_new_privs + rlimits: {all}"
    );
}

#[test]
fn probe_enforces_or_reports_missing_landlock() {
    let fx = Fixture::new();
    let mut cmd = probe();
    probe_args(&mut cmd, &fx);
    let out = cmd.output().expect("spawn sbox-probe");
    let code = out
        .status
        .code()
        .unwrap_or_else(|| panic!("probe killed by signal; stderr:\n{}", stderr_of(&out)));
    match code {
        EXIT_OK => {
            // Landlock enforced: forbidden unreadable, ro readable, writes ok.
            let so = stdout_of(&out);
            assert!(
                !so.contains("NOT-ENFORCED"),
                "enforced run must not report degradation: {so}"
            );
        }
        EXIT_NOT_ENFORCED => {
            // This kernel (or container) has no usable Landlock — the
            // graceful path is the correct outcome here.
            assert_graceful_notes(&stdout_of(&out));
        }
        other => panic!(
            "unexpected probe exit {other}\nstderr:\n{}\nstdout:\n{}",
            stderr_of(&out),
            stdout_of(&out)
        ),
    }
}

#[test]
fn graceful_degradation_notes_are_exercised_even_with_landlock() {
    // --no-landlock requests zero Landlock rules: whatever the kernel, `apply`
    // must succeed with landlock inactive and notes documenting the fallback
    // to rlimits + no_new_privs. Verifiable on every machine.
    let fx = Fixture::new();
    let mut cmd = probe();
    probe_args(&mut cmd, &fx);
    cmd.arg("--no-landlock");
    let out = cmd.output().expect("spawn sbox-probe --no-landlock");
    let code = out.status.code().unwrap_or(-1);
    assert_eq!(
        code,
        EXIT_OK,
        "--no-landlock must pass the degradation expectations\nstderr:\n{}\nstdout:\n{}",
        stderr_of(&out),
        stdout_of(&out)
    );
    assert_graceful_notes(&stdout_of(&out));
}

#[test]
fn probe_refuses_bad_setup() {
    // A missing forbidden file is a vacuous probe: must be a setup error, not
    // a (misleading) success.
    let fx = Fixture::new();
    let missing: &Path = &fx.root.path().join("profile-B/nope.txt");
    let mut cmd = probe();
    cmd.arg("--writable")
        .arg(fx.writable())
        .arg("--forbidden")
        .arg(missing);
    let out = cmd.output().expect("spawn sbox-probe");
    assert_ne!(out.status.code(), Some(EXIT_OK));
    assert!(stderr_of(&out).contains("pre-flight"));
}
