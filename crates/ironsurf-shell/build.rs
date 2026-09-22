fn main() {
    // Debug info keeps the `.slint` element ids in the generated code, so the
    // headless layout tests (tests/inspector_layout.rs) can read a widget's real
    // geometry back through i-slint-backend-testing's `ElementHandle` API.
    let config = slint_build::CompilerConfiguration::new().with_debug_info(true);
    slint_build::compile_with_config("ui/ironsurf.slint", config)
        .expect("slint build: ui/ironsurf.slint must compile");
}
