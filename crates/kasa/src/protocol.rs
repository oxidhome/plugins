//! The pure Kasa protocol core: codecs, request builders and reply parsers
//! with no I/O. Everything here compiles natively and to `wasm32-wasip2`
//! and is unit-tested against the known-answer vectors of the Scrypted
//! plugin's `docs/PROTOCOL.md`.

pub mod camera;
pub mod cipher;
pub mod discovery;
pub mod http;
pub mod iot;
mod json;
pub mod linkie;
pub mod multipart;
pub mod rtp;
pub mod talk;
