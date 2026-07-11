//! Compile-fail and compile-pass coverage for the topology guarantees:
//! derive shape and attribute errors, the two token-API errors the type system
//! promises (wrong message type for a slot, reusing a consumed slot token),
//! and supported derive attributes.

#[test]
fn topology_derive_ui() {
    let t = trybuild::TestCases::new();
    t.compile_fail("tests/ui/topology/*.rs");
    t.pass("tests/ui/topology-pass/*.rs");
}
