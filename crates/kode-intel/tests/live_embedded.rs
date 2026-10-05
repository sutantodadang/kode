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
    // A shared name: `new` exists many times in this repo. zindeks 0.10.4
    // (zindeks#10) links cross-file calls to the right `new`; earlier
    // releases dropped them, leaving every `new` with no outside callers.
    let event_bus_new = intel
        .exact_symbols("new", Some("crates/kode-core/src/event.rs"))
        .await
        .unwrap();
    assert_eq!(event_bus_new.len(), 1, "{event_bus_new:?}");
    let shared = intel.exact_symbols("new", None).await.unwrap();
    assert!(shared.len() > 1, "expected a shared name: {shared:?}");
    let callers = intel
        .trace_ids(&[event_bus_new[0].id], TraceDirection::Inbound, 1)
        .await
        .unwrap();
    // `exec::run` calls `EventBus::new(256)`.
    assert!(
        callers
            .iter()
            .any(|n| n.file == "crates/kode/src/exec.rs" && n.name == "run"),
        "no cross-file callers of EventBus::new: {callers:?}"
    );

    intel.refresh().await.unwrap();
    drop(intel);
    let _ = std::fs::remove_dir_all(&store);
}
