//! Compile-fail coverage for the topology guarantees: derive shape errors
//! plus the two token-API errors the type system promises (wrong message
//! type for a slot, reusing a consumed slot token).

#[test]
fn topology_errors_are_readable() {
    let t = trybuild::TestCases::new();
    t.compile_fail("tests/ui/topology/*.rs");
}
