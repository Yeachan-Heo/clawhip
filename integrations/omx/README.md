# clawhip × OMX native hook bridge

This directory ships a clawhip-side OMX integration that forwards native OMX hook envelopes to the clawhip daemon without making OMX users hand-roll generic `IncomingEvent` HTTP payloads.

## What is included

- `clawhip-sdk.mjs` — small OMX-facing client that hides clawhip discovery + transport details
- `clawhip-hook.mjs` — sample `.omx/hooks/*.mjs` plugin that forwards contract-compliant events to clawhip
- `install-hook.sh` — copies the sample hook + SDK into an OMX workspace

## Transport and discovery order

The SDK chooses the lightest transport that preserves native semantics:

1. **CLI transport** — preferred when `clawhip` is available (`CLAWHIP_BIN` or `PATH`)
   - sends the raw v1 envelope to `clawhip omx hook`
2. **HTTP transport** — fallback when a daemon URL is discoverable
   - checks `CLAWHIP_OMX_DAEMON_URL`
   - then `CLAWHIP_DAEMON_URL`
   - then `CLAWHIP_CONFIG` / `~/.clawhip/config.toml`
   - finally falls back to `http://127.0.0.1:25294`

Override the transport explicitly with:

```bash
export CLAWHIP_OMX_TRANSPORT=cli   # or http
```

## Install into an OMX workspace

```bash
./integrations/omx/install-hook.sh /path/to/repo/.omx/hooks
```

Then validate inside that OMX workspace:

```bash
omx hooks validate
omx hooks test
```

If you already have a serialized v1 hook envelope, clawhip also exposes a matching thin client:

```bash
clawhip omx hook --file payload.json
# or
cat payload.json | clawhip omx hook
```

## Contract boundary notes

The SDK only forwards the frozen v1 `normalized_event` surface:

- `started`
- `blocked`
- `finished`
- `failed`
- `retry-needed`
- `pr-created`
- `test-started`
- `test-finished`
- `test-failed`
- `handoff-needed`

`tool.use` is intentionally **not** a new v1 canonical event. Use `tool_name`, `command`, and `error_summary` metadata on one of the frozen events instead.

## Manual usage

```js
import { createClawhipOmxClient } from './clawhip-sdk.mjs';

const client = await createClawhipOmxClient();
await client.emitSessionStarted({
  context: {
    session_name: 'issue-65-native-sdk',
    repo_path: '/repo/clawhip',
    branch: 'feat/issue-65-native-sdk',
    status: 'started',
  },
});
```

Or forward an existing OMX hook event from a plugin:

```js
export async function onHookEvent(event, sdk) {
  const client = await createClawhipOmxClient();
  return await client.emitFromHookEvent(event);
}
```
