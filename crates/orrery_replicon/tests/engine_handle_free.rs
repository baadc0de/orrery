//! Compile-time coverage for the engine-handle registration boundary.

#[test]
fn engine_handle_free_payloads() {
    // kache remaps diagnostics through `/kache/home`, which trybuild cannot
    // normalize back to `$DIR`; compile the isolated fixtures directly so the
    // committed stderr is checkout-independent.
    std::env::set_var("RUSTC_WRAPPER", "");
    let cases = trybuild::TestCases::new();
    cases.pass("tests/ui/engine_handle_free_payload_compiles.rs");
    cases.compile_fail("tests/ui/entity_in_replicated_payload_does_not_compile.rs");
}
