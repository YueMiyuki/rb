//! Test-command fixture: unit tests, doctests, integration tests, examples and benches.

/// The answer, computed by a proc-macro.
pub const ANSWER: u32 = tf_macros::answer!();

/// Adds two numbers.
///
/// ```
/// assert_eq!(tf::add(2, 2), 4);
/// ```
///
/// ```compile_fail
/// let _: u32 = tf::add("2", 2);
/// ```
///
/// ```no_run
/// loop { tf::add(1, 1); }
/// ```
pub fn add(a: u32, b: u32) -> u32 {
    a + b
}

#[cfg(test)]
mod tests {
    #[test]
    fn adds() {
        assert_eq!(super::add(1, 2), 3);
        assert_eq!(super::ANSWER, 42);
    }

    #[test]
    #[should_panic(expected = "overflow")]
    fn overflows() {
        super::add(u32::MAX, 1);
    }

    #[test]
    #[ignore]
    fn slow() {}
}
