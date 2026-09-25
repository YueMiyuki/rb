use std::path::Path;
use std::process::Command;

#[test]
fn runs_the_binary() {
    let out = Command::new(env!("CARGO_BIN_EXE_tf")).output().unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "3");
}

#[test]
fn has_a_scratch_dir() {
    assert!(Path::new(env!("CARGO_TARGET_TMPDIR")).is_dir());
}

#[test]
fn runs_in_the_package_root() {
    assert!(Path::new("Cargo.toml").is_file());
    assert_eq!(std::env::current_dir().unwrap(), Path::new(env!("CARGO_MANIFEST_DIR")));
}
