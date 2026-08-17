# Kode for organizations

Kode is dual-licensed: AGPL-3.0 for open source use, and a commercial license for organizations that need different terms. This page explains what each covers so you can figure out which applies to you.

This is a summary to orient you, not legal advice. If your situation is anything but straightforward, talk to your own counsel and to us before you ship.

## What AGPL-3.0 obliges

AGPL-3.0 is a copyleft license with a network clause. In plain terms:

- **Unmodified internal use is fine.** Running Kode as-is, inside your company, to help your engineers write code, does not trigger any disclosure obligation. You don't owe anyone your prompts, your code, or your config.
- **The trigger is distributing a modified version, including over a network.** If you modify Kode and let others interact with that modified version: whether you hand them a binary or you run it as a service they connect to: AGPL requires you to make the modified source available to those users. This is the clause that closes the "modify it, host it, never publish the changes" loophole that a plain GPL doesn't cover.
- **Combining Kode with your own separate tooling around it (not modifying Kode itself) generally doesn't extend AGPL to that tooling,** provided you're not statically linking or otherwise creating a combined work. Where that line falls is exactly the kind of question to run past counsel for your specific setup.

## When you need the commercial license

You need the commercial license if you want to:

- Modify Kode and ship or host the modified version without disclosing your source changes.
- Embed Kode inside a closed-source product you distribute.
- Avoid AGPL's obligations entirely for a hosted or resold offering.

If you're only running unmodified Kode internally, the AGPL terms above already cover you and you don't need to do anything.

## What the commercial license includes

- Proprietary use: modify, embed, and distribute without AGPL's disclosure requirement.
- No copyleft obligation on your combined work.
- Negotiable support SLA, scoped to your deployment.

Terms and pricing are negotiated directly: see procurement contact below.

## Security posture summary

- **Local-first.** Kode runs on your machine; it does not phone home your code or prompts as part of its normal operation.
- **Credential isolation.** All provider credentials live only in `~/.kode/auth/`, one file per provider, `0600` permissions on Unix. Kode never reads another tool's stored credentials and never logs token values.
- **No telemetry.** Kode does not collect or transmit usage analytics.
- **Consent-gated downloads.** `kode setup` prompts before installing the zindeks and Ingat engine binaries; nothing is fetched silently.
- **Honest verification.** The verification pipeline reports tests/lint/build results accurately: a skipped check is reported as Skipped, never as Passed. This matters for compliance workflows that trust Kode's pass/fail signal.

For the full policy, see [SECURITY.md](../SECURITY.md).

## Procurement

For commercial licensing, support agreements, or security questionnaires, contact:

**sutantodadang@gmail.com**

Further reading: [SECURITY.md](../SECURITY.md), [SUPPORT.md](../SUPPORT.md), [LICENSE-COMMERCIAL.md](../LICENSE-COMMERCIAL.md).

## Related

- [explanation-architecture.md](./explanation-architecture.md): why AGPL was chosen, in the trade-offs section
- [reference-cli.md](./reference-cli.md): `kode setup` and the consent-gated install it runs
- [../README.md](../README.md)
