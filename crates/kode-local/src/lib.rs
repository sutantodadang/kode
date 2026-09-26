//! Local, in-process decision models for Kode: Laya task routing and Qwen3
//! context reranking on ONNX Runtime. Core crate: never prints, never
//! depends on ratatui; results travel back as values.

pub mod calibrate;
pub mod dataset;
pub mod device;
pub mod error;
pub mod laya;
pub mod log;
pub mod manifest;
pub mod models;
pub mod pins;
pub mod rerank;
pub mod route;
pub mod sequence;
pub mod teacher;
pub mod temps;

pub use error::LocalError;
