//! TP-Link Kasa plugin for `OxidHome`.
//!
//! [`protocol`] is the pure protocol core; [`kasa::Kasa`] is the plugin
//! logic over the [`platform`] seams; [`wasm`] wires it to the component
//! exports and WASI.
pub mod config;
pub mod kasa;
pub mod mapping;
pub mod media;
pub mod platform;
pub mod protocol;
pub mod wasm;

#[cfg(test)]
mod e2e;
#[cfg(test)]
mod fake;
