fn main() {
    println!("cargo::rustc-check-cfg=cfg(built_by_script)");
    println!("cargo::rustc-cfg=built_by_script");
    println!("cargo::rustc-env=BUILD_GREETING=hi from build.rs");
    println!("cargo::rerun-if-changed=build.rs");
    let out = std::env::var("OUT_DIR").unwrap();
    std::fs::write(format!("{out}/generated.rs"), "pub const GENERATED: u32 = 42;\n").unwrap();
}
