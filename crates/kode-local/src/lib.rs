//! Local, in-process decision models for Kode: Laya task routing and Qwen3
//! context reranking on ONNX Runtime. Core crate: never prints, never
//! depends on ratatui; results travel back as values.

pub mod device;
pub mod error;
pub mod models;
pub mod pins;
pub mod route;
pub mod sequence;

pub use error::LocalError;
