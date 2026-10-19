//! IronSurf ships ONE executable: the same file is the shell and, when asked
//! differently, the sandboxed tab host. These spawn the real binary with no
//! engine boot and no display and pin the argv contract that makes that true —
//! including the exact shape the shell uses for its children (a bare
//! `--tab-id …`, no `tab` word), and the `tab` subcommand that makes the tab
//! host typeable by hand.

use std::process::{Command, Stdio};

const BIN: &str = env!("CARGO_BIN_EXE_ironsurf");

fn run(args: &[&str]) -> (i32, String, String) {
    let out = Command::new(BIN)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .and_then(|mut c| {
            // A tab host with no stdin gets EOF and exits; close the pipe so a
            // child that reads stdin never blocks.
            let _ = c.stdin.take();
            c.wait_with_output()
        })
        .expect("spawn ironsurf");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[test]
fn one_binary_one_version_string_for_both_halves() {
    let (code, stdout, _) = run(&["--version"]);
    assert_eq!(code, 0, "`ironsurf --version` failed");
    assert!(
        stdout.contains(env!("CARGO_PKG_VERSION")) && stdout.contains("ironsurf"),
        "version output was {stdout:?}"
    );
    // Asking as the tab host answers the same string: one artifact, one version.
    let (tab_code, tab_stdout, _) = run(&["tab", "--version"]);
    assert_eq!(tab_code, 0, "`ironsurf tab --version` failed");
    assert_eq!(
        tab_stdout, stdout,
        "the two halves disagree about their version"
    );
}

#[test]
fn no_arguments_is_the_shell_and_its_help_names_the_program() {
    let (code, stdout, _) = run(&["--help"]);
    assert_eq!(code, 0, "`ironsurf --help` failed: shell usage is not printed");
    assert!(
        stdout.contains("ironsurf [OPTIONS]") && stdout.contains("--tab-bin"),
        "shell usage was {stdout:?}"
    );
}

#[test]
fn the_tab_subcommand_prints_the_tab_usage_with_the_subcommand_consumed() {
    let (code, stdout, _) = run(&["tab", "--help"]);
    assert_eq!(code, 0, "`ironsurf tab --help` failed");
    assert!(
        stdout.contains("ironsurf tab") && stdout.contains("--tab-id"),
        "tab usage was {stdout:?}"
    );
    // …and it is the TAB half's help: the word `tab` was consumed by dispatch
    // instead of reaching the parser as an unknown argument, and no shell-only
    // flag is in sight.
    assert!(
        !stdout.contains("unexpected argument") && !stdout.contains("--linuxkms"),
        "the tab subcommand did not reach the tab host: {stdout:?}"
    );
}

/// Exactly how the shell spawns a tab child: this executable, `--tab-id` first,
/// no subcommand. It must land in the tab host — the shell's own usage error
/// would mean the child came up as a second GUI.
#[test]
fn a_bare_tab_id_runs_the_tab_host_not_a_second_shell() {
    let dir = std::env::temp_dir().join(format!("is-onebin-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("profile dir");
    let page = dir.join("p.html");
    std::fs::write(&page, "<html><body><p>hi</p></body></html>").expect("page");

    let mut child = Command::new(BIN)
        .arg("--tab-id").arg("1")
        .arg("--container-id").arg("1")
        .arg("--profile-dir").arg(&dir)
        .arg("--headless")
        .arg("--no-sandbox")
        .arg("--url").arg(format!("file://{}", page.display()))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn the tab host");
    // No stdin, so the tab host sees EOF and shuts down; drain stdout so a full
    // frame buffer never blocks it.
    child.stdin.take();
    let out = child.wait_with_output().expect("tab host exits");
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        !String::from_utf8_lossy(&out.stderr).contains("ironsurf [OPTIONS]"),
        "the tab child came up as the shell: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        out.status.code(),
        Some(0),
        "tab host exit: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}
