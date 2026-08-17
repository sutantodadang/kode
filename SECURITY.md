# Security Policy

## Supported Versions

| Version | Supported |
|---|---|
| 0.1.x | Yes |

## Reporting a Vulnerability

Report vulnerabilities privately. Do not open a public issue for a security report.

Email sutantodadang@gmail.com with subject line `kode security`.

We aim to:

- Acknowledge your report within 72 hours.
- Work with you on coordinated disclosure, targeting resolution within 90 days.

## Scope

Kode stores credentials in `~/.kode/auth/` (file mode 0600 on unix) and never logs token values. Kode spawns zindeks and Ingat as local processes but does not bundle or maintain their source.

Reports about zindeks or Ingat themselves should go to their own issue trackers, not this one.
