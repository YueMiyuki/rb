#[test]
fn only_with_extra() {
    assert!(cfg!(feature = "extra"));
}
