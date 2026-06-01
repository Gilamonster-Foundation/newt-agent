fn main() {
    // PyO3 `extension-module` intentionally leaves Python symbols unresolved so
    // the Python interpreter can supply them at dlopen time. Linux's linker
    // accepts this for shared libraries by default; macOS rejects undefined
    // symbols unless told to defer them. Pass the flag that maturin would
    // normally supply when building without maturin (plain `cargo build`).
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        println!("cargo:rustc-link-arg=-undefined");
        println!("cargo:rustc-link-arg=dynamic_lookup");
    }
}
