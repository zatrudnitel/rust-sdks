pub mod logic;

// `pub` so `examples/local_run.rs` can drive `engine::run` directly (bypassing
// the napi layer, which needs a JS env for the `ThreadsafeFunction`).
#[cfg(all(windows, feature = "engine"))]
pub mod engine;
#[cfg(all(windows, feature = "engine"))]
mod napi_api;
