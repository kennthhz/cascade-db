//! T5.1 (FR-35): the `!Send` closure must be rejected AT COMPILE TIME
//! (the plan's test-review bullet: the trybuild pattern, asserted at
//! test time — never a commented-out line).

#[test]
fn submit_to_rejects_non_send_closure() {
    let t = trybuild::TestCases::new();
    t.compile_fail("tests/compile_fail/*.rs");
}
