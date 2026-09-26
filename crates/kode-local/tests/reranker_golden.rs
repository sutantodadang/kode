//! Run with: KODE_MODEL_DIR=build/local-models KODE_ORT_DYLIB=<lib>
//!   cargo test -p kode-local --test reranker_golden -- --ignored

use std::path::PathBuf;

use kode_local::device::{DevicePref, init_runtime};
use kode_local::rerank::QwenReranker;
use serde::Deserialize;

#[derive(Deserialize)]
struct Case {
    query: String,
    doc: String,
    input_ids: Vec<u32>,
    score: f32,
}

#[derive(Deserialize)]
struct Sanity {
    query: String,
    relevant: String,
    irrelevant: String,
}

#[derive(Deserialize)]
struct Fixtures {
    cases: Vec<Case>,
    sanity: Vec<Sanity>,
}

fn load() -> (QwenReranker, Fixtures) {
    let dylib = std::env::var_os("KODE_ORT_DYLIB").expect("set KODE_ORT_DYLIB");
    init_runtime(std::path::Path::new(&dylib)).unwrap();
    let dir = PathBuf::from(std::env::var_os("KODE_MODEL_DIR").expect("set KODE_MODEL_DIR"))
        .join("qwen3-reranker-0.6b");
    let text = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/reranker_golden.json"
    ))
    .expect("run scripts/local-models/build_reranker.py first");
    (
        QwenReranker::load(&dir, DevicePref::Cpu).unwrap(),
        serde_json::from_str(&text).unwrap(),
    )
}

#[test]
#[ignore]
fn inputs_and_scores_match_reference() {
    let (reranker, fx) = load();
    for (i, c) in fx.cases.iter().enumerate() {
        assert_eq!(
            reranker.input_ids(&c.query, &c.doc),
            c.input_ids,
            "case {i}: input_ids differ"
        );
        let s = reranker.score(&c.query, &c.doc).unwrap();
        assert!((s - c.score).abs() <= 1e-3, "case {i}: {s} vs {}", c.score);
    }
}

#[test]
#[ignore]
fn relevant_candidates_rank_first() {
    let (reranker, fx) = load();
    for s in &fx.sanity {
        let scores = reranker
            .score_all(&s.query, &[s.relevant.clone(), s.irrelevant.clone()])
            .unwrap();
        assert!(scores[0] > scores[1], "{}: {scores:?}", s.query);
    }
}
