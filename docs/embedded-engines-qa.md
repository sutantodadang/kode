# Embedded engines — QA and release acceptance

Status: **partial** — the combined native session is verified on Windows
x86_64 against the released zindeks asset. Linux/macOS targets and the
optional router/model gates are recorded as unverified below, not claimed.

## Pinned upstreams

| Component | Revision | Artifact |
| --- | --- | --- |
| zindeks | tag `v0.10.1` = `6d9f9137c7b3f4f52b94cbd3f476be16e327c4d0` | four ABI 1 assets (see checksums) |
| ingat-core | `9be69c00c420abceb61218ea43fba7cb363512c2` (v0.2.0), features `sqlite-store` | git dependency in `crates/kode-memory/Cargo.toml` |
| Kode | workspace `0.4.12` | — |

### zindeks v0.10.1 FFI asset SHA-256

| Target | SHA-256 |
| --- | --- |
| x86_64 Windows MSVC | `551ef2c2cb03a70ca367de912ac8a72ebc101a4d3a2d93cbd9b690f0ddd23b7c` |
| x86_64 Linux GNU | `0fe903a78dc0a3bba0c3401df3b73a50f9c15e190c2154194b923ab84d31246b` |
| aarch64 Linux GNU | `93fb4104120bb9e56d1b78fe963b6cfefef32e5e11fb45f0e1982904cf99d515` |
| aarch64 macOS | `cbc9069e2dbd55aa2adbcfb6ac0080494ba0dba8db41f64ae37735f77e0b181a` |

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

`.github/workflows/ci.yml` job `native-engines` runs on **windows-x86_64**,
downloads the pinned asset above, verifies its SHA-256, and runs both
`--ignored` gates.

**Linux asset is currently broken upstream**: the v0.10.1
`zindeks-ffi-linux-{x86_64,aarch64}` libraries were built with Zig's default
`*-linux` target, which links **musl** (`DT_NEEDED libc.so`). On a glibc runner
`dlopen` fails with `.../libc.so: invalid ELF header`. The fix (build the linux
FFI assets with the `-gnu` ABI so they need `libc.so.6`) is committed upstream
and the gate moves to a Linux/Windows matrix once a gnu-ABI asset is published.

## Not yet verified (unrun manual gates)

- **Linux / macOS release targets**: the linux assets need the upstream gnu-ABI
  rebuild above; macOS aarch64 was not run here. Windows x86_64 is verified.
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
