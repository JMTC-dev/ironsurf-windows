//! servfox-tab — sandboxed per-tab Servo host process.
//!
//! Full IPC lifecycle (task 5): apply the OS sandbox, embed one
//! [`servo::Servo`] with a software (surfman-smitha) `RenderingContext` whose
//! data path is the container profile dir, drive one [`servo::WebView`], and
//! speak the `servfox-common` `ToTab`/`FromTab` protocol over stdin/stdout.
//!
//! `--spike` mode (task 4) is preserved for standalone render proofs; see
//! [`spike`] and `docs/SERVO_INTEGRATION.md`.

mod netmod;
mod runtime;
mod spike;
mod wake;

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
        match runtime::TabArgs::parse(&args) {
            Ok(tab_args) => runtime::run(tab_args),
            Err(err) => {
                eprintln!(
                    "servfox-tab: {err}\n\
                     usage: servfox-tab --tab-id N --container-id N --profile-dir PATH \
                     [--url URL] [--managed-net] [--no-sandbox]\n\
                     \x20      servfox-tab --spike --url <file:///...> --out <out.ppm> [--size WxH]"
                );
                2
            }
        }
    };
    // `_exit`, not `exit`: Mesa/GL and the engine's threads are still live,
    // and running libc atexit handlers under them races into SIGSEGV. All
    // IPC output is already flushed and the cookie jar persisted.
    // SAFETY: nothing else needs to run; descriptors are flushed.
    unsafe { libc::_exit(code) }
}
