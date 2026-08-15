//! Opt-in integration test against a real `zindeks` binary. Not run by
//! default — the CI/dev machine must have `zindeks` on PATH and the target
//! repo already indexed.
//!
//! Run with: `cargo test -p kode-intel --test live_zindeks -- --ignored`

use std::path::PathBuf;

use kode_core::config::ZindeksConfig;
use kode_intel::{CodeIntelligence, ZindeksAdapter};

fn repo_root() -> PathBuf {
    if let Ok(root) = std::env::var("KODE_IT_ROOT") {
        return PathBuf::from(root);
    }
    // crates/kode-intel -> repo root is two levels up.
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .expect("crates/kode-intel should have a repo root two levels up")
        .to_path_buf()
}

#[tokio::test]
#[ignore = "requires zindeks binary"]
async fn connects_and_reports_health_against_real_zindeks() {
    let cfg = ZindeksConfig::default();
    let root = repo_root();

    let adapter = ZindeksAdapter::connect(&cfg, &root)
        .await
        .expect("failed to connect to zindeks — is the binary on PATH?");

    adapter
        .ensure_bound()
        .await
        .expect("repo should already be indexed on this machine");

    let health = adapter.health().await.expect("health_check failed");
    assert!(
        health.documents > 0,
        "expected an indexed repo to report documents > 0, got {health:?}"
    );

    let results = adapter.search("status", 5).await.expect("search failed");
    // Not asserting non-empty: a real BM25/semantic search may legitimately
    // return zero hits for a query, but the call itself must succeed.
    let _ = results;
}
