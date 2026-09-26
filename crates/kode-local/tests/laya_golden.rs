//! Parity with the Python reference (`scripts/local-models/build_laya.py`).
//! Run with: KODE_MODEL_DIR=build/local-models KODE_ORT_DYLIB=<onnxruntime lib>
//!   cargo test -p kode-local --test laya_golden -- --ignored

use std::path::PathBuf;

use kode_local::device::{DevicePref, init_runtime};
use kode_local::laya::{LayaConfig, LayaModel, LayaTokenizer};
use kode_local::sequence::{QuestionDef, build_sequence};
use serde::Deserialize;

#[derive(Deserialize)]
struct Case {
    state: String,
    question: QuestionDef,
    input_ids: Vec<u32>,
    markers: Vec<usize>,
    probs: Vec<f32>,
}

fn cases() -> Vec<Case> {
    let text = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/laya_golden.json"
    ))
    .expect("run scripts/local-models/build_laya.py first");
    serde_json::from_str(&text).unwrap()
}

fn laya_dir() -> PathBuf {
    PathBuf::from(std::env::var_os("KODE_MODEL_DIR").expect("set KODE_MODEL_DIR"))
        .join("laya-multilingual")
}

#[test]
#[ignore]
fn sequences_match_python_reference() {
    let dir = laya_dir();
    let cfg = LayaConfig::load(&dir).unwrap();
    let tok = LayaTokenizer::load(&dir, &cfg).unwrap();
    for (i, c) in cases().iter().enumerate() {
        let seq = build_sequence(&tok, &c.state, &c.question, cfg.max_len, cfg.head_max_len);
        assert_eq!(seq.ids, c.input_ids, "case {i}: input_ids differ");
        assert_eq!(seq.markers, c.markers, "case {i}: markers differ");
    }
}

#[test]
#[ignore]
fn probabilities_match_python_reference() {
    let dylib = std::env::var_os("KODE_ORT_DYLIB").expect("set KODE_ORT_DYLIB");
    init_runtime(std::path::Path::new(&dylib)).unwrap();
    let model = LayaModel::load(&laya_dir(), DevicePref::Cpu).unwrap();
    for (i, c) in cases().iter().enumerate() {
        let p = model.probs(&c.state, &c.question).unwrap();
        assert_eq!(p.len(), c.probs.len(), "case {i}");
        for (a, b) in p.iter().zip(&c.probs) {
            assert!((a - b).abs() <= 1e-3, "case {i}: {p:?} vs {:?}", c.probs);
        }
    }
}
