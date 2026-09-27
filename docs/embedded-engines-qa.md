# Embedded engines — QA and release acceptance

Status: **partial** — the combined native session is verified on Windows
x86_64 (local) and on Linux x86_64, Windows x86_64 and macOS aarch64 in CI
against the released zindeks asset. The fresh-host, multi-process recovery and
optional router/model gates are recorded as unverified below, not claimed.

## Pinned upstreams

| Component | Revision | Artifact |
| --- | --- | --- |
| zindeks | tag `v0.10.2` = `e79bc89a3b68870448db5c69d4420ec509484959` | four ABI 1 assets (see checksums) |
| ingat-core | `9be69c00c420abceb61218ea43fba7cb363512c2` (v0.2.0), features `sqlite-store` | git dependency in `crates/kode-memory/Cargo.toml` |
| Kode | workspace `0.4.12` | — |

### zindeks v0.10.2 FFI asset SHA-256

| Target | SHA-256 |
| --- | --- |
| x86_64 Windows MSVC | `eb8d84bba5981ca54900b261f3f0c93c32b13c259de063b8312860105a89f849` |
| x86_64 Linux GNU | `7e1281d0249e1f5d0caabe42a1bcb9e0a28082b77f1e1ee265cc201f82b0d0ea` |
| aarch64 Linux GNU | `7983af13c2903a9f5046fcaf9e0025ca567bf64a2796ae00fc1cc84a53d2652c` |
| aarch64 macOS | `3a6c733f53710bb303bc12c1d0d641d859ea75711a3396473f21aeb4f4780085` |

Each archive contains the shared library, `include/zindeks.h`,
`NOTICE.sqlite.md` and `metadata.json` (`abi_version=1`, `sqlite=3.53.4`).

## Runtime versions

- zindeks embedded runtime reports SQLite `3.53.4` / `3053004` via
  `health_check.sqlite_version[_number]` (floor enforced at ABI open).
- Kode's native memory uses `ingat-core`'s `rusqlite 0.40.2` bundled engine
  (`sqlite-store` feature), asserted `>= 3.51.3` in `ingat-core`.
- The two SQLite builds coexist in one process: zindeks hides its vendored
  SQLite symbols; ingat-core's rusqlite is a separate static build.

## Combined native session (verified)

Environment: Windows x86_64, `KODE_ZINDEKS_DYLIB` = released
`zindeks-ffi-windows-x86_64` asset.

```
cargo test -p kode-intel --test embedded_real -- --ignored      # ok
cargo test -p kode --test native_engines -- --ignored          # ok
```

- `embedded_real::embedded_index_then_query_works` — open → `ensure_bound`
  (not-indexed) → `index_repository` → `health` (`documents >= 1`,
  `sqlite_version` present) → `get_context` → `file_outline`.
- `native_engines::native_memory_and_code_share_one_process` — 20 memories
  committed via `EmbeddedIngat` while `EmbeddedZindeks` indexes and queries the
  graph; both backends report healthy; no interposition or fallback.
- `native_engines::bad_library_path_errors_without_fallback` — a missing
  library errors (`IntelError::Unavailable`); no external transport is spawned.
- `native_engines::embedded_memory_commits_and_recalls` — memory-only roundtrip.

zindeks-side gates (in the zindeks checkout):

```
zig build test          # ok (accuracy scorecard 1.00)
zig build ffi-test      # ok (export allowlist + C ABI smoke)
```

## CI gate

`.github/workflows/ci.yml` job `native-engines` runs a matrix of
**linux-x86_64, windows-x86_64 and macos-aarch64**, downloads the pinned v0.10.2
asset for each, verifies its SHA-256, and runs both `--ignored` gates.

The v0.10.1 linux FFI assets were musl-linked (`DT_NEEDED libc.so`) and failed
to `dlopen` on glibc runners (`.../libc.so: invalid ELF header`); v0.10.2 builds
the linux assets with the `-gnu` ABI (needs `libc.so.6`).

## Not yet verified (unrun manual gates)

- **Fresh-host install**: `kode setup` downloading + checksum-verifying the
  embedded library, then `kode index`, was not exercised end-to-end here.
- **Two-process recovery**: concurrent writers, busy owner >5s, interrupted
  import, newer-schema rejection — covered by unit/integration tests in
  `kode-memory`/`ingat-core`, not by a scripted multi-process run here.
- **Router/model parity**: `kode-local` Laya/reranker golden tests remain
  optional (`#[ignore]`, require installed models) and were not run.

## Notes

- `docs/superpowers/` (the plan/spec sources) is git-ignored; this document is
  the tracked QA record.
- The zindeks ReleaseSafe asset required a vendored-C UBSan fix
  (`-fno-sanitize=undefined` on SQLite/tree-sitter/grammars) shipped in
  v0.10.1; earlier assets trap when loaded at a relocated image base.
