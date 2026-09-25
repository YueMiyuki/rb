use std::path::PathBuf;

fn main() {
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let include = out.join("include");
    std::fs::create_dir_all(&include).unwrap();
    std::fs::copy("csrc/add.h", include.join("add.h")).unwrap();
    cc::Build::new().file("csrc/add.c").include(&include).compile("nativeadd");
    println!("cargo::metadata=include={}", include.display());
    println!("cargo::rerun-if-changed=csrc");
}
