fn main() {
    let include = std::env::var("DEP_NATIVE_INCLUDE").expect("DEP_NATIVE_INCLUDE from native-sys");
    assert!(std::path::Path::new(&include).join("add.h").exists(), "header missing in {include}");
    println!("cargo::rustc-check-cfg=cfg(has_native_header)");
    println!("cargo::rustc-cfg=has_native_header");
}
