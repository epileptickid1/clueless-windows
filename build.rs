fn main() {
    println!("cargo:rerun-if-changed=src/windows_native.c");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        cc::Build::new().file("src/windows_native.c").compile("clueless_windows");
        for library in ["ole32", "user32", "gdi32"] {
            println!("cargo:rustc-link-lib={library}");
        }
    }
}
