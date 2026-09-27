# Embedded engines — QA and release acceptance

Status: **partial** — the v0.10.3 combined native session, warm-index watcher,
fresh installation, and upgrade from v0.10.2 are verified on Windows x86_64.
All four release archives and their metadata were checksum-verified; execution
on other platforms needs the updated CI run. See [the v0.10.3 QA record](zindeks-0.10.3-qa-2026-09-27.md)
and [the earlier merge QA record](merge-qa-2026-09-27.md).

## Pinned upstreams

| Component | Revision | Artifact |
| --- | --- | --- |
| zindeks | tag `v0.10.3` = `418da3ff065e956509b0e6746e9c17075d920c19` | four ABI 1 assets (see checksums) |
| ingat-core | `9be69c00c420abceb61218ea43fba7cb363512c2` (v0.2.0), features `sqlite-store` | git dependency in `crates/kode-memory/Cargo.toml` |
| Kode | workspace `0.5.2` | — |

### zindeks v0.10.3 FFI archive SHA-256

| Target | SHA-256 |
| --- | --- |
| x86_64 Windows MSVC | `a40c922774dc7b579eae095d7080075fd508d6c01cfe5dae06aaeb7dbd2b885b` |
| x86_64 Linux GNU | `ad375454785c4250db529b26c2e1632f1d4c2cffe79022be3407a6b5807aecd2` |
| aarch64 Linux GNU | `d1affcc9c70063373138c9810b8fad4d621c53f09313a3097f69766cf34b8c26` |
| aarch64 macOS | `8b5a02a46f4f36f53d1a307ab275511d21ecfa36ddad1a07d096323d849d3c33` |

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
  Also reopens a warm index with watching enabled, queries context, and verifies
  that the watcher observes a subsequent file edit before shutdown.
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
**linux-x86_64, linux-aarch64, windows-x86_64 and macos-aarch64**, downloads
the pinned v0.10.3 asset for each, verifies its SHA-256, and runs the real engine
and fresh-install CLI gates. The ARM64 and fresh-install additions still need
a CI run; the previously verified matrix had three targets.

The v0.10.1 linux FFI assets were musl-linked (`DT_NEEDED libc.so`) and failed
to `dlopen` on glibc runners (`.../libc.so: invalid ELF header`); v0.10.2 builds
the linux assets with the `-gnu` ABI (needs `libc.so.6`).

## Not yet verified (unrun manual gates)

- **Updated platform CI**: the installer fix, strengthened native tests,
  Linux ARM64 and fresh-install gates need the next CI run.
- **Full release acceptance**: power-loss simulation, legacy export with a
  locked source, performance measurements and manual TUI acceptance remain
  unrun. The merge QA record lists the local scenarios and model checks that
  have now passed.

## Notes

- `docs/superpowers/` (the plan/spec sources) is git-ignored; this document is
  the tracked QA record.
- The zindeks ReleaseSafe asset required a vendored-C UBSan fix
  (`-fno-sanitize=undefined` on SQLite/tree-sitter/grammars) shipped in
  v0.10.1; earlier assets trap when loaded at a relocated image base.
