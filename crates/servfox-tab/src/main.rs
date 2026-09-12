//! servfox-tab — sandboxed per-tab Servo host process.
//!
//! Task 4 delivers the **Servo integration spike** (`--spike` mode): create one
//! [`servo::Servo`] with a software (surfman-smitha) `RenderingContext`, load one
//! page in a [`servo::WebView`], wait for load-complete + screenshot, write a PPM.
//!
//! Full IPC lifecycle (sandbox + ToTab/FromTab pipe protocol) builds on this; see
//! ARCHITECTURE.md §servfox-tab.

mod spike;

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
        eprintln!(
            "servfox-tab: full IPC lifecycle is not implemented yet (task 4 delivers --spike mode).\n\
             usage: servfox-tab --spike --url <file:///...> --out <out.ppm> [--size WxH]"
        );
        2
    };
    std::process::exit(code);
}
