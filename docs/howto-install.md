# How to install Kode

## Script install (recommended)

Linux or macOS:

```
curl -fsSL https://raw.githubusercontent.com/sutantodadang/kode/main/install.sh | sh
```

Windows (PowerShell):

```
powershell -c "irm https://raw.githubusercontent.com/sutantodadang/kode/main/install.ps1 | iex"
```

What the script does:

1. Detects your OS and CPU architecture.
2. Fetches the latest release from `github.com/sutantodadang/kode/releases`.
3. Verifies the downloaded archive's sha256 checksum against the published value.
4. Unpacks the `kode` binary into the default install directory (see below).
5. Prints a note if that directory is not already on your `PATH`.

Override the install directory with the `KODE_INSTALL_DIR` environment variable before running the script. Defaults:

- Unix: `~/.local/bin`
- Windows: `%LOCALAPPDATA%\Programs\kode`

## Manual binary install

Download the archive matching your platform from the releases page:

```
https://github.com/sutantodadang/kode/releases
```

Available targets:

| Target | Platform |
|---|---|
| `x86_64-unknown-linux-gnu` | Linux, 64-bit Intel/AMD |
| `aarch64-unknown-linux-gnu` | Linux, 64-bit ARM |
| `x86_64-apple-darwin` | macOS, Intel |
| `aarch64-apple-darwin` | macOS, Apple Silicon |
| `x86_64-pc-windows-msvc` | Windows, 64-bit |

Steps:

1. Download `kode-<target>.tar.gz` (Unix) or `kode-<target>.zip` (Windows).
2. Unpack it.
3. Move the `kode` binary onto your `PATH`: for example `~/.local/bin` on Unix or `%LOCALAPPDATA%\Programs\kode` on Windows.

## Build from source

Requires a Rust toolchain (`cargo`).

```
git clone https://github.com/sutantodadang/kode.git
cd kode
cargo install --path crates/kode
```

This builds and installs the `kode` binary via cargo's usual install location (typically `~/.cargo/bin`, already on `PATH` if you followed rustup's setup).

## Verify the install

```
kode --version
kode doctor
```

`kode --version` confirms the binary runs and reports its version. `kode doctor` runs a fuller diagnostic across config, LLM auth, zindeks, Ingat, git, and environment: useful right after install to catch a missing engine or a PATH issue before you hit it mid-task.

## Troubleshooting

**`kode: command not found` after install.** The install directory is not on your `PATH`. Add it: on Unix, append `export PATH="$HOME/.local/bin:$PATH"` to your shell profile; on Windows, add `%LOCALAPPDATA%\Programs\kode` to your user `PATH` environment variable and open a new terminal.

**Windows SmartScreen blocks the binary.** Right-click `kode.exe`, choose Properties, and check "Unblock" at the bottom of the General tab, then Apply. This is standard for unsigned binaries downloaded from the internet.

**Linux: binary won't run / glibc errors.** The Linux release targets glibc on x86_64 and arm64. Very old distributions with an outdated glibc, or musl-based distros like Alpine, will not run the prebuilt binary: build from source instead.

**Behind a proxy.** The install script uses your shell's standard proxy environment variables (`HTTPS_PROXY`, `HTTP_PROXY`). Set them before running the script if your network requires a proxy for outbound HTTPS.

## Related

- [tutorial-getting-started.md](./tutorial-getting-started.md): full walkthrough after install
- [howto-auth-providers.md](./howto-auth-providers.md): next step, logging in
- [reference-cli.md](./reference-cli.md): full command reference
- [../README.md](../README.md)
