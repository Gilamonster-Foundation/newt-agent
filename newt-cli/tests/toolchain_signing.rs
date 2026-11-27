//! Explicit, serial real-resource grounding of signing's pure parser/identity
//! guards and the shared terminal-confirmation contract. Disposable keys only.
//! Run with `cargo test -p newt-agent --test toolchain_signing -- --sign-toolchains-3327`.

#[cfg(unix)]
mod common;
#[cfg(unix)]
#[path = "support/toolchain_signing.rs"]
mod native;

fn main() {
    newt_core::answer_nextest_listing("toolchain_signing");
    if std::env::args().any(|arg| arg == "--sign-toolchains-3327") {
        #[cfg(unix)]
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(native::run());
        #[cfg(not(unix))]
        panic!("signing requires supported ownership checks");
    }
}
