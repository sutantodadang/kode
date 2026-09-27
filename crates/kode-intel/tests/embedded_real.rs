//! Real-engine acceptance for the embedded zindeks backend.
//!
//! Gated on `KODE_ZINDEKS_DYLIB` pointing at a verified zindeks asset. The
//! test is `#[ignore]` so it never silently passes without a real library:
//!
//! ```text
//! KODE_ZINDEKS_DYLIB=/path/to/zindeks.dll \
//!   cargo test -p kode-intel --test embedded_real -- --ignored
//! ```
//!
//! The library must be built with the vendored-C UBSan fix in zindeks
//! `build.zig` (`-fno-sanitize=undefined` on SQLite/tree-sitter/grammars).
//! Without it the ReleaseSafe asset traps (`ud1`, STATUS_ILLEGAL_INSTRUCTION)
//! when the host loads it at a relocated image base — see the zindeks build
//! comment. Verified against a local ReleaseSafe build of zindeks.

use std::path::PathBuf;

use kode_intel::{CodeContextRequest, CodeIntelligence, EmbeddedZindeks, IntelError};

fn lib_path() -> Option<PathBuf> {
    let path = PathBuf::from(std::env::var_os("KODE_ZINDEKS_DYLIB")?);
    path.is_file().then_some(path)
}

fn temp_repo() -> PathBuf {
    let mut dir = std::env::temp_dir();
    dir.push(format!("kode-embedded-real-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(
        dir.join("src/lib.rs"),
        "pub fn answer() -> u32 { 42 }\n\npub struct Widget { pub n: u32 }\n",
    )
    .unwrap();
    dir
}

fn temp_store() -> PathBuf {
    let mut dir = std::env::temp_dir();
    dir.push(format!("kode-embedded-store-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

#[tokio::test]
#[ignore = "requires KODE_ZINDEKS_DYLIB pointing at a verified zindeks asset"]
async fn embedded_index_then_query_works() {
    let library = lib_path().expect("set KODE_ZINDEKS_DYLIB to the verified zindeks asset");
    let repo = temp_repo();
    let store = temp_store();

    let backend =
        EmbeddedZindeks::open(&library, &repo, &store, false).expect("open embedded zindeks");

    // A fresh repository is not indexed yet, and `ensure_bound` must not index.
    assert!(matches!(
        backend.ensure_bound().await,
        Err(IntelError::NotIndexed(_))
    ));

    backend.index_repository().await.expect("index repository");

    let health = backend.health().await.expect("health");
    assert!(
        health.documents >= 1,
        "expected at least one document, got {health:?}"
    );
    assert!(!health.status.is_empty());
    assert!(
        health.sqlite_version.is_some(),
        "expected sqlite_version from zindeks >= 0.10.0"
    );

    let ctx = backend
        .get_context(CodeContextRequest {
            query: "answer".to_string(),
            working_set: vec![],
            max_tokens: Some(500),
        })
        .await
        .expect("context");
    assert!(!ctx.text.is_empty(), "expected non-empty context");

    let outline = backend.file_outline("src/lib.rs").await.expect("outline");
    assert!(
        outline
            .symbols
            .iter()
            .any(|s| s.name.contains("answer") || s.name.contains("Widget")),
        "outline missing expected symbols: {outline:?}"
    );

    // Poll once to enqueue native work, then cancel its reply and close the
    // adapter. Closing must drain the work before unloading the library.
    for index in 0..200 {
        std::fs::write(
            repo.join(format!("src/cancel_{index}.rs")),
            format!("pub fn cancel_{index}() -> u32 {{ {index} }}\n"),
        )
        .unwrap();
    }
    {
        use std::future::Future;
        let mut request = Box::pin(backend.index_repository());
        let state =
            std::future::poll_fn(|cx| std::task::Poll::Ready(request.as_mut().poll(cx))).await;
        assert!(
            state.is_pending(),
            "expected queued index work before cancellation"
        );
    }
    drop(backend);
    let reopened = EmbeddedZindeks::open(&library, &repo, &store, false)
        .expect("reopen after canceled request and shutdown");
    assert!(reopened.health().await.unwrap().documents >= 201);
    reopened
        .ensure_bound()
        .await
        .expect("rebind after cancellation");
    drop(reopened);

    // A warm open attaches self-referential search state and starts the
    // watcher. Those pointers must refer to the final native server address.
    let watched =
        EmbeddedZindeks::open(&library, &repo, &store, true).expect("open warm index with watcher");
    assert!(watched.watching());
    assert!(watched.health().await.unwrap().documents >= 201);
    let warm_context = watched
        .get_context(CodeContextRequest {
            query: "answer".into(),
            working_set: vec![],
            max_tokens: Some(500),
        })
        .await
        .expect("query warm watched index");
    assert!(!warm_context.text.is_empty());
    std::fs::write(repo.join("src/watched.rs"), "pub fn watched_update() {}\n").unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            if watched.health().await.unwrap().documents >= 202 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("watcher applies edit after warm open");
    drop(watched);
    let _ = std::fs::remove_dir_all(&repo);
    let _ = std::fs::remove_dir_all(&store);
}
