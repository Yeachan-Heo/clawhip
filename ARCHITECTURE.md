# clawhip Architecture — v0.3.0

clawhip v0.3.0 ships a daemon-first event pipeline for Discord delivery. This document describes the architecture that is present on the `release/0.3.0` branch.

## Release themes

- typed event model
- multi-delivery router
- extracted event sources
- renderer/sink separation

## High-level flow

```text
[CLI / webhook / git / GitHub / tmux]
              -> [sources]
              -> [mpsc queue]
              -> [dispatcher]
              -> [router -> renderer -> Discord / Slack / HTTP sink]
              -> [Discord REST / Slack webhook / signed HTTP delivery]
```

## Core components

### Typed event model (`crate::event`)

The daemon accepts legacy `IncomingEvent` payloads at ingress, normalizes them, and converts them into typed internal events through `crate::event::compat`. That gives v0.3.0 a typed event model without breaking the existing CLI and HTTP surfaces.

Key event families shipped in v0.3.0:

- custom events
- git commit and branch-change events
- GitHub issue opened / commented / closed events
- GitHub pull-request status change events
- agent lifecycle events
- tmux keyword and stale-session events

### Sources (`crate::source`)

Event production is split into dedicated sources behind the `Source` trait:

- `GitSource` polls configured repositories for commit and branch changes
- `GitHubSource` polls configured repositories for issue and PR changes
- `TmuxSource` monitors tmux sessions for keyword hits and stale panes

All sources feed a shared Tokio `mpsc` queue. This replaces the earlier tighter coupling between monitors, routing, and transport.

### Dispatcher (`crate::dispatch`)

`Dispatcher` is the queue consumer. For each incoming event it:

1. resolves matching deliveries with the router
2. renders content for each delivery
3. hands the rendered message to the configured sink
4. continues best-effort when one delivery fails

This is the central coordination point for the v0.3.0 pipeline.

### Router (`crate::router`)

The router now resolves **0..N deliveries per event**. In practice that means:

- multiple route rules can match the same event
- a match no longer stops at the first rule
- each resolved delivery keeps the destination target, format, template, and mention context

This is the main behavioral change behind the v0.3.0 multi-delivery architecture.

### Renderer (`crate::render`)

Rendering is now explicit. The default renderer is responsible for formatting supported event bodies into compact, alert, inline, or raw output before transport.

That keeps message formatting out of the transport layer and makes the dispatch pipeline easier to extend and test.

### Sink (`crate::sink`)

Transport is represented by the `Sink` trait. Discord delivers to channels, threads, or Discord
webhooks; Slack delivers to incoming webhooks; the generic HTTP sink delivers signed normalized
events to a configured controller endpoint; and the local-file sink appends bounded summaries.

The renderer/sink split keeps transport concerns out of routing and event modeling. Multiple
matching routes are attempted independently, so one event can fan out to HTTP and Discord and a
failure at one target does not suppress the remaining deliveries.

## Configuration model

The preferred Discord configuration surface in v0.3.0 is:

```toml
[providers.discord]
token = "..."
default_channel = "1234567890"
```

Legacy `[discord]` configuration is still accepted and normalized on load for backward compatibility.

Routes continue to use the familiar event/filter model, with a `sink` field that defaults to `"discord"`:

```toml
[[routes]]
event = "github.*"
filter = { repo = "clawhip" }
sink = "discord"
channel = "1480171113253175356"
mention = "<@1465264645320474637>"
format = "compact"
```

For direct Clawhip → Hermes delivery, configure one provider endpoint and reference the HMAC
secret by environment-variable name rather than storing the secret in TOML:

```toml
[providers.http]
endpoint = "http://127.0.0.1:8644/webhooks/clawhip-controller"
hmac_secret_env = "HERMES_CLAWHIP_HMAC_SECRET"

[[routes]]
event = "*"
sink = "http"
```

The HTTP sink serializes a bounded public-safe `clawhip.http-event.v1` body, signs the exact body
bytes with HMAC-SHA256 in `X-Hub-Signature-256`, and uses the event correlation identity for a
stable `X-Request-ID`. Plain HTTP is restricted to loopback hosts, HTTPS is allowed for remote
hosts, and redirects are disabled. Telemetry and provenance retain only a host plus stable
redacted endpoint fingerprint; secret values, raw response bodies, local paths, and webhook URL
paths are not emitted. Discord credentials are not required for HTTP-only routing.

## Delivery semantics

v0.3.0 currently uses these delivery rules:

- per-source FIFO through the shared queue
- best-effort multi-delivery; one failed delivery does not stop the others
- no built-in retry queue
- source-level tmux keyword windowing, with dispatch remaining stateless

## Operational verification

The release branch includes a live verification runbook in [`docs/live-verification.md`](docs/live-verification.md). It covers daemon status, custom events, git events, GitHub issue/PR flows, and tmux monitoring.
