//! Combined native-engine acceptance.
//!
//! Verifies the two in-process engines — embedded Ingat memory and embedded
//! zindeks code intelligence — run in one Kode process without interfering,
//! and that a native engine failure never falls back to another backend.
//!
//! The real combined test is gated on `KODE_ZINDEKS_DYLIB` (the verified
//! zindeks asset) and is `#[ignore]` so it can never pass without a real
//! library:
//!
//! ```text
//! KODE_ZINDEKS_DYLIB=/path/to/zindeks.dll \
//!   cargo test -p kode --test native_engines -- --ignored
//! ```

use std::path::PathBuf;

use kode_intel::{CodeIntelligence, EmbeddedZindeks};
use kode_memory::{
    EmbeddedIngat, EngineeringMemory, MemoryContext, MemoryKind, MemoryQuery, NewMemory, Provenance,
};

fn temp(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "kode-native-engines-{label}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

fn memory(index: usize) -> NewMemory {
    NewMemory {
        kind: MemoryKind::ProjectRule,
        summary: format!("memory note {index}"),
        body: format!("durable engineering note number {index} about the build"),
        tags: vec!["acceptance".to_string()],
        provenance: Provenance::ExplicitUser,
        context: MemoryContext {
            repository: Some("kode".to_string()),
            ..Default::default()
        },
        team: false,
    }
}

/// A native engine that cannot load must fail — never silently fall back to an
/// external transport.
#[tokio::test]
async fn bad_library_path_errors_without_fallback() {
    let missing = temp("missing-lib").join("zindeks-does-not-exist.dll");
    let repo = temp("badlib-repo");
    std::fs::create_dir_all(&repo).unwrap();
    let store = temp("badlib-store");

    let err = match EmbeddedZindeks::open(&missing, &repo, &store, false) {
        Ok(_) => panic!("open must fail for a missing library"),
        Err(err) => err,
    };
    assert!(
        matches!(err, kode_intel::IntelError::Unavailable(_)),
        "expected Unavailable, got {err:?}"
    );

    let _ = std::fs::remove_dir_all(&repo);
}

/// Embedded memory works standalone (no external service): 20 writes commit,
/// then recall returns them.
#[tokio::test]
async fn embedded_memory_commits_and_recalls() {
    let db = temp("mem-only").join("memory.sqlite3");
    let memory_backend = EmbeddedIngat::open(&db).expect("open embedded memory");

    for index in 0..20 {
        memory_backend.remember(&memory(index)).await.unwrap();
    }
    assert_eq!(memory_backend.stats().await.unwrap().total, 20);
    memory_backend.health().await.unwrap();

    let found = memory_backend
        .search(&MemoryQuery {
            text: "build note".to_string(),
            repository: None,
            kind: None,
            limit: 5,
        })
        .await
        .unwrap();
    assert!(
        !found.is_empty(),
        "expected recall to return stored memories"
    );

    let _ = std::fs::remove_dir_all(db.parent().unwrap());
}

/// Both engines in one process: 20 memories commit while the code graph is
/// indexed and queried; both report healthy and neither interposes the other's
/// SQLite.
#[tokio::test]
#[ignore = "requires KODE_ZINDEKS_DYLIB pointing at a verified zindeks asset"]
async fn native_memory_and_code_share_one_process() {
    let library = std::env::var_os("KODE_ZINDEKS_DYLIB")
        .map(PathBuf::from)
        .expect("set KODE_ZINDEKS_DYLIB to the verified zindeks asset");
    assert!(
        library.is_file(),
        "library not found: {}",
        library.display()
    );

    let repo = temp("combined-repo");
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::write(
        repo.join("src/lib.rs"),
        "pub fn answer() -> u32 { 42 }\n\npub struct Widget { pub n: u32 }\n",
    )
    .unwrap();
    let code_store = temp("combined-code-store");
    let mem_db = temp("combined-mem").join("memory.sqlite3");

    // zindeks: open, index, query.
    let code = EmbeddedZindeks::open(&library, &repo, &code_store, false).expect("open zindeks");
    code.index_repository().await.expect("index repository");

    // ingat: 20 durable writes in the same process.
    let memory_backend = EmbeddedIngat::open(&mem_db).expect("open embedded memory");
    for index in 0..20 {
        memory_backend.remember(&memory(index)).await.unwrap();
    }

    // Code queries run alongside the committed memories.
    let code_health = code.health().await.expect("code health");
    assert!(
        code_health.documents >= 1,
        "expected indexed documents, got {code_health:?}"
    );
    assert!(!code_health.status.is_empty());
    let results = code.search("answer", 5).await.expect("code search");
    assert!(
        results.iter().all(|r| !r.path.is_empty()),
        "code search returned an empty path: {results:?}"
    );

    // Both stores are healthy and independent.
    let mem_stats = memory_backend.stats().await.expect("memory stats");
    assert_eq!(mem_stats.total, 20);
    memory_backend.health().await.expect("memory health");

    let _ = std::fs::remove_dir_all(&repo);
    let _ = std::fs::remove_dir_all(&code_store);
    if let Some(parent) = mem_db.parent() {
        let _ = std::fs::remove_dir_all(parent);
    }
}
