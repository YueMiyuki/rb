use proc_macro::TokenStream;

/// Expands to `42`.
///
/// ```
/// assert_eq!(tf_macros::answer!(), 42);
/// ```
#[proc_macro]
pub fn answer(_input: TokenStream) -> TokenStream {
    "42".parse().unwrap()
}
