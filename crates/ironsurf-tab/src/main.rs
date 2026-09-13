//! ironsurf-tab — sandboxed per-tab Blitz host process.
//!
//! The full `ToTab`/`FromTab` IPC lifecycle on Blitz (task 7c): OS sandbox
//! at startup, one `HtmlDocument` per navigation driven on a single thread,
//! `resolve` → `vello_cpu` paint → `FromTab::Frame` at ≤30 fps, managed
//! container networking through `netmod`. Input events route through the
//! `runtime::Tab::handle_input` extension point (link clicks + full input
//! semantics land in task 7d). The standalone render spike ([`spike`]) is
//! kept behind `--spike`.

mod js;
mod netmod;
mod runtime;
mod spike;

fn usage() -> String {
    "usage: ironsurf-tab --tab-id N --container-id N --profile-dir PATH \
     [--url URL] [--offline-fixtures DIR] [--managed-net] [--no-sandbox]\n\
     \x20      ironsurf-tab --spike --url <file:///...> --out <out.ppm> [--size WxH] [--out-rgba out.rgba]"
        .to_owned()
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let code = if args.iter().any(|a| a == "--spike") {
        match spike::run(&args) {
            Ok(()) => 0,
            Err(err) => {
                eprintln!("ironsurf-tab spike: error: {err}");
                1
            }
        }
    } else {
        match runtime::TabArgs::parse(&args) {
            Ok(tab_args) => runtime::run(tab_args),
            Err(err) => {
                eprintln!("ironsurf-tab: {err}\n{}", usage());
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
