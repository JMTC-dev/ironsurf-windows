//! Windows main-thread stack size for the `ironsurf` binary.
//!
//! Windows reserves 1 MiB for a program's main thread; Linux gives it 8 MiB.
//! The tab host runs Boa, Stylo and blitz layout on its main thread — all
//! deeply recursive — and a JS-heavy page (a Next.js blog) overflowed the
//! smaller stack (`0xC00000FD`, "thread 'main' has overflowed its stack").
//! Reserve what Linux does. Binaries only: the library has no entry point,
//! and spawned threads take their size from `std::thread` on every platform.

const MAIN_STACK_BYTES: u32 = 8 * 1024 * 1024;

fn main() {
    let target = std::env::var("TARGET").unwrap_or_default();
    if target.ends_with("-windows-msvc") {
        println!("cargo:rustc-link-arg-bins=/STACK:{MAIN_STACK_BYTES}");
    } else if target.ends_with("-windows-gnu") || target.ends_with("-windows-gnullvm") {
        println!("cargo:rustc-link-arg-bins=-Wl,--stack,{MAIN_STACK_BYTES}");
    }
    println!("cargo:rerun-if-changed=build.rs");
}
