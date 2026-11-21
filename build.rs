use std::{env, path::PathBuf};

fn main() {
    let source = "src/headless_cursor.c";
    println!("cargo:rerun-if-changed={source}");
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("linux") {
        return;
    }
    let library =
        PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR")).join("headless_cursor.so");
    let mut compiler = cc::Build::new().get_compiler().to_command();
    let status = compiler
        .args(["-shared", "-fPIC", "-nostdlib", "-o"])
        .arg(library)
        .arg(source)
        .status()
        .expect("start the C compiler");
    assert!(status.success(), "failed to build the headless cursor shim");
}
