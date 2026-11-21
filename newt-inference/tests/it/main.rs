//! The crate's integration tests. Each file here is a module of one test
//! executable, `it`, so it is linked once instead of once per file. A file
//! that is not declared below is not built, and its tests do not run.
//! `tests/openai_auth_env.rs` is a separate executable on purpose; see its
//! header.

mod live_tool_probe;
mod local_stream_tests;
mod ollama_tests;
mod openai_auth_tests;
mod registry_tests;
mod vllm_tests;
