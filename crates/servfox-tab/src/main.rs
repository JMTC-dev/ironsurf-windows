//! servfox-tab — sandboxed per-tab Blitz host process.
//!
//! **Placeholder (task 6 of the engine pivot).** The Servo runtime was removed;
//! the full `ToTab`/`FromTab` IPC lifecycle on Blitz (sandbox apply, one
//! `HtmlDocument` per navigation, resolve→paint→`FromTab::Frame` at ≤30 fps,
//! resize/input via `blitz_dom` events, managed `NetProvider`, Boa `<script>`
//! execution) is implemented in the **NEXT task (task 7)** — see
//! ARCHITECTURE.md §"servfox-tab (binary) — the Blitz + Boa host".
//!
//! Until then this stub still parses the runtime CLI flags and exits 0 printing
//! nothing on stdout (so the shell's spawn handshake cannot desync while the
//! runtime is under reconstruction), and keeps the standalone Blitz render
//! spike ([`spike`], `--spike`).

// Managed-net rule helpers + their unit tests are kept across the pivot and
// re-wired into the Blitz NetProvider in task 7.
#[allow(dead_code)]
mod netmod;
mod spike;

/// TODO(task 7): this becomes `runtime::TabArgs` + the full Blitz IPC loop.
fn parse_runtime_flags(args: &[String]) -> Result<(), String> {
    let mut it = args.iter().skip(1).peekable();
    let mut seen: Vec<&str> = Vec::new();
    while let Some(arg) = it.next() {
        let mut value = |what: &str| -> Result<(), String> {
            it.next()
                .map(|_| ())
                .ok_or_else(|| format!("{what} requires a value"))
        };
        match arg.as_str() {
            "--tab-id" => {
                value("--tab-id")?;
                seen.push("--tab-id");
            }
            "--container-id" => {
                value("--container-id")?;
                seen.push("--container-id");
            }
            "--profile-dir" => {
                value("--profile-dir")?;
                seen.push("--profile-dir");
            }
            "--url" => {
                value("--url")?;
            }
            "--offline-fixtures" => {
                value("--offline-fixtures")?;
            }
            "--managed-net" | "--no-sandbox" => {}
            other => return Err(format!("unexpected argument {other:?}")),
        }
    }
    for required in ["--tab-id", "--container-id", "--profile-dir"] {
        if !seen.contains(&required) {
            return Err(format!("missing required flag {required}"));
        }
    }
    Ok(())
}

fn usage() -> String {
    "usage: servfox-tab --tab-id N --container-id N --profile-dir PATH \
     [--url URL] [--offline-fixtures DIR] [--managed-net] [--no-sandbox]\n\
     \x20      servfox-tab --spike --url <file:///...> --out <out.ppm> [--size WxH]"
        .to_owned()
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let code = if args.iter().any(|a| a == "--spike") {
        match spike::run(&args) {
            Ok(()) => 0,
            Err(err) => {
                eprintln!("servfox-tab spike: error: {err}");
                1
            }
        }
    } else {
        match parse_runtime_flags(&args) {
            // TODO(task 7): replace with runtime::run(tab_args) — the full
            // Blitz IPC lifecycle. Until then: exit 0, nothing on stdout.
            Ok(()) => 0,
            Err(err) => {
                eprintln!("servfox-tab: {err}\n{}", usage());
                2
            }
        }
    };
    // `_exit`, not `exit`: keep the shell-visible shutdown path identical to
    // the previous runtime (no atexit handlers, no teardown races).
    // SAFETY: nothing else needs to run; stdout is empty and stderr is unbuffered.
    unsafe { libc::_exit(code) }
}
