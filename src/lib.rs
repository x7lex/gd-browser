pub mod level;
pub mod sim;
pub mod render;
pub mod assets;
pub mod bot;
pub mod stream;
pub mod bridge;
pub mod physics;

/// the browser build (wasm-bindgen glue; see web/)
#[cfg(target_family = "wasm")]
pub mod web;
