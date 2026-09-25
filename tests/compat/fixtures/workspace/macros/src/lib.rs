use proc_macro::TokenStream;
use quote::quote;

#[proc_macro_derive(Hello)]
pub fn derive_hello(input: TokenStream) -> TokenStream {
    let ast: syn::DeriveInput = syn::parse(input).unwrap();
    let name = &ast.ident;
    let text = format!("hello from {name}");
    quote! {
        impl #name {
            pub fn hello() -> &'static str { #text }
        }
    }
    .into()
}
