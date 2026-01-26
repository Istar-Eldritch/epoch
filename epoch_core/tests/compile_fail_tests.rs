//! Compile-fail tests to verify anti-patterns are prevented at compile time.
//!
//! These tests use trybuild to verify that certain code patterns fail to compile
//! with appropriate error messages.

#[test]
fn compile_fail_tests() {
    let t = trybuild::TestCases::new();
    t.compile_fail("tests/compile_fail/*.rs");
}
