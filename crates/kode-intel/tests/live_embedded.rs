//! Live check of the SQL-backed symbol lookups against the pinned engine.
//! Skips unless `KODE_ZINDEKS_DYLIB` points at a zindeks library.

use std::path::{Path, PathBuf};

use kode_intel::{CodeIntelligence, EmbeddedZindeks, TraceDirection};

#[tokio::test]
#[ignore]
async fn embedded_sql_lookups_work_against_pinned_engine() {
    let Some(dylib) = std::env::var_os("KODE_ZINDEKS_DYLIB").map(PathBuf::from) else {
        eprintln!("KODE_ZINDEKS_DYLIB unset; skipping live embedded test");
        return;
    };
    let repo_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let store = std::env::temp_dir().join(format!("kode-live-embedded-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&store);
    let intel = EmbeddedZindeks::open(&dylib, &repo_root, &store, false).unwrap();
    intel.index_repository().await.unwrap();

    let rows = intel
        .exact_symbols("append_turn", Some("crates/kode/src/session.rs"))
        .await
        .unwrap();
    assert_eq!(rows.len(), 1, "{rows:?}");
    let nodes = intel
        .trace_ids(&[rows[0].id], TraceDirection::Inbound, 1)
        .await
        .unwrap();
    assert!(
        nodes.iter().any(|n| n.name == "record_completed_turn"),
        "{nodes:?}"
    );
    intel.refresh().await.unwrap();
    drop(intel);
    let _ = std::fs::remove_dir_all(&store);
}
