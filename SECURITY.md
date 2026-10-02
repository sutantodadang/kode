# Security Policy

## Supported versions

Kode is pre-1.0. Security fixes land on the latest minor release line only; please upgrade (`kode update`) before reporting.

| Version | Supported |
|---|---|
| 0.5.x | Yes |
| < 0.5 | No |

## Reporting a vulnerability

Report vulnerabilities privately. Do not open a public issue, discussion, or PR for a security report.

Email sutantodadang@gmail.com with the subject line `kode security`. Please include the Kode version (`kode --version`), your OS, steps to reproduce, and the impact you observed.

We aim to:

- Acknowledge your report within 72 hours.
- Work with you on coordinated disclosure, targeting a fix within 90 days.
- Credit you in the release notes, unless you prefer to stay anonymous.

## Scope

In scope: the `kode` binary and the code in this repository, including the install scripts in `scripts/`.

How Kode handles sensitive data:

- Provider credentials live only in `~/.kode/auth/`, one file per provider (mode `0600` on Unix). Kode never reads other tools' credential files and never logs token values.
- Child processes started by agent tools do not inherit credential environment variables (`*_API_KEY`, `*_TOKEN`, `*_SECRET`).
- The zindeks engine library and the optional local router models are downloaded only with consent (`kode setup`) and are sha256-verified against pinned checksums before use. Ingat's core is compiled into the binary.
- Every release archive is published with a sha256 checksum. The install scripts and `kode update` check it; the install scripts warn and continue if no checksum tool or sidecar is available.

Out of scope: vulnerabilities in zindeks or Ingat themselves. Report those to their own repositories ([zindeks](https://github.com/sutantodadang/zindeks), [Ingat](https://github.com/sutantodadang/Ingat)). Model provider behavior is also out of scope.
