# clawhip — AGENTS.md

Daemon-first event gateway for Discord. Routes GitHub, tmux, and custom events to channels.

## Working agreements

- Rust: follow existing code style, use `anyhow` for error handling.
- Config changes must be backward-compatible (old configs must still parse).
- New routes/filters: add integration tests.
- CLI subcommands: include `--help` descriptions for all flags.
- Keep the daemon lightweight — no unnecessary allocations in the hot path.

## Review guidelines

- Flag breaking changes to config schema or CLI interface as P0.
- Verify all Discord API calls handle rate limits and error responses.
- Check for hardcoded tokens or secrets — flag as P0.
- TOML config parsing: ensure new fields have sensible defaults (don't break existing configs).
- Route matching: verify glob patterns are tested with edge cases.
- tmux integration: confirm session monitoring handles missing/dead sessions gracefully.
- New dependencies must be justified — prefer std library where possible.
- Daemon lifecycle: verify clean shutdown and signal handling.
- Test coverage: flag new logic paths that lack corresponding tests.
