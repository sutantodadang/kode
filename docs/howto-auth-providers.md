# How to authenticate with a model provider

Kode keeps its own credential store at `~/.kode/auth/` and never reads another tool's auth files (not `~/.codex`, not opencode's data directory). Credentials are file-permissioned `0600` on Unix. Token values are never logged, at any verbosity level.

## Log in to Codex

Codex uses OAuth+PKCE through your browser:

```
kode auth login codex
```

What happens: Kode starts a local PKCE flow, opens your default browser to the Codex login page, and waits for the redirect. Once you approve, Kode exchanges the code for a token and writes it to `~/.kode/auth/codex.json`. The access token auto-refreshes on later runs using the stored refresh token, so you normally only do this once.

On success, Kode prints the live model list fetched from the backend right away, for example `gpt-5.6-sol`. This list is fetched fresh, not hardcoded: if the backend adds a model, `kode models` and this login output reflect it immediately.

## Log in to an opencode-family provider

opencode-go, opencode, kilo, and lmstudio authenticate by pasting an API key:

```
kode auth login opencode-go
kode auth login opencode
kode auth login kilo
kode auth login lmstudio
```

Kode prompts you to paste the key, then writes it to `~/.kode/auth/<provider>.json`.

## Check what's logged in

```
kode auth status
```

Lists every provider with stored credentials. It does not print token values.

## Log out

```
kode auth logout codex
```

Removes the stored credential file for that provider. This does not affect any other tool's separate login state, since Kode never shared credentials with them in the first place.

## Switching provider or model

In the TUI, use the slash commands:

- `/provider`: switch the active provider
- `/model`: switch the active model for the current provider
- `/effort`: switch reasoning effort (`minimal`, `low`, `medium`, `high`, `xhigh`, `max`, `ultra`)

These write through to `[model]` in `.kode/config.toml` (`provider`, `model`, `effort` keys), so the choice persists across sessions in that repo. You can also edit `.kode/config.toml` directly: see [reference-config.md](./reference-config.md).

For a one-off override without touching config, pass `--model` and `--effort` to `kode exec`.

## Security notes

- Credentials live only in `~/.kode/auth/`, one JSON file per provider, `0600` on Unix.
- Kode never reads `~/.codex`, opencode's config, or any other tool's stored credentials: logging in to Kode is a separate, independent login even if you're already authenticated elsewhere.
- Token values never appear in logs, even with `-v`/`-vv`.

## Troubleshooting

**Browser doesn't open for `codex` login.** Some headless or remote-shell environments can't launch a browser. Run the login from a machine with a browser, or copy the printed URL manually into a browser on another device if Kode offers one.

**Refresh token expired.** Codex sessions eventually expire. Run `kode auth login codex` again to re-authenticate; this overwrites the stale credential file.

**Empty model list after login.** If the live model list can't be fetched from the backend, Kode falls back to a static list of known model candidates so you can still select something and keep working. Run `kode models` later to re-fetch the live list once connectivity is restored.

## Related

- [tutorial-getting-started.md](./tutorial-getting-started.md): auth as part of first-run flow
- [reference-cli.md](./reference-cli.md): full `auth` subcommand reference
- [reference-config.md](./reference-config.md): `[model]` config section
- [../README.md](../README.md)
