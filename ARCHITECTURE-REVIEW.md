# ARCHITECTURE Review

## Executive summary

This is a **good vision document but not yet a good implementation plan**.

The proposal correctly identifies the main disease in v0.2: the runtime is still fundamentally `daemon -> router -> DiscordClient`, with monitors and HTTP handlers tightly coupled to Discord delivery (`src/daemon.rs:30-44`, `src/daemon.rs:100-105`, `src/router.rs:31-34`, `src/monitor.rs:52-57`, `src/monitor.rs:565-579`). The document also points in the right direction by introducing source abstraction, provider abstraction, and a migration story.

But as written, it **understates the migration cost**, **overstates how "typed" the new model really is**, and puts some trait boundaries in the wrong places. My blunt take: **as a north-star architecture, this is promising; as a v0.3 execution spec, it is not ready**.

---

## 1) What's good about the proposed design

### 1.1 It identifies the real coupling problems

The doc's diagnosis is broadly correct:
- current daemon startup constructs both `Router` and `DiscordClient`, then passes both into the monitor loop (`src/daemon.rs:35-44`)
- HTTP ingress dispatches straight into the router, which dispatches straight into Discord (`src/daemon.rs:100-105`, `src/router.rs:31-34`)
- the monitor loop also depends on both router and Discord transport (`src/monitor.rs:52-57`, `src/monitor.rs:565-579`)
- route resolution is Discord-shaped (`DeliveryTarget::Channel | Webhook`) and only one route is selected (`src/router.rs:10-13`, `src/router.rs:108-145`)

So yes: the proposal is solving an actual architectural problem, not an imaginary one.

### 1.2 Extracting event sources is the right general direction

`EventSource` is the cleanest proposed abstraction. The current code already has source-like behavior buried in the monitor loop and tmux wrapper:
- git polling / GitHub polling in `src/monitor.rs:144-330`
- tmux polling in `src/monitor.rs:331-516`
- another tmux monitoring path in `src/tmux_wrapper.rs:78-314`

That duplication is a strong signal that "source" is a real boundary.

### 1.3 Keeping TOML and aiming for backward compatibility is pragmatic

Staying on TOML is the right call. The current config already has normalization and compatibility behavior for Discord token/default channel handling (`src/config.rs:245-255`, `src/config.rs:271-298`, `src/config.rs:449-474`). That means there is a real foundation for migration instead of a rewrite-everything fantasy.

### 1.4 The phased rollout is directionally sensible

The document at least tries to phase the work instead of pretending Claw OS appears in one commit (`ARCHITECTURE.md:373-405`). That's good. The problem is the phase contents, not the fact that phases exist.

### 1.5 Local-first and compile-time plugin loading are sane defaults

For a Rust daemon-first tool, avoiding a database and avoiding runtime plugin loading on day one are both good instincts (`ARCHITECTURE.md:461-463`). Those choices reduce the chance of building a framework-shaped crater.

---

## 2) What's missing or underspecified

### 2.1 The proposal says "typed events" but the payload is still untyped

The proposed event envelope uses `EventKind`, which is good, but the payload is still `serde_json::Value` (`ARCHITECTURE.md:64-70`). That is only half a type system.

Current v0.2 is stringly-typed in two places:
- event kind is a string (`src/events.rs:41-54`)
- payload is arbitrary JSON and renderers pull fields out by string keys (`src/events.rs:491-717`, `src/events.rs:744-765`)

The proposal fixes the first half and leaves the second half intact. Worse, route filters are still string-key/string-value maps (`ARCHITECTURE.md:197-207`), which means routing remains schema-implicit.

So the document currently oversells this. It is not "all data flows as typed events." It is "event kind becomes typed, payload stays dynamic."

### 2.2 Provider registration / construction is not actually designed

The proposal picks trait objects for providers (`ARCHITECTURE.md:455`) but the trait itself includes `async fn init(...) -> Result<Self>` (`ARCHITECTURE.md:135-151`). That constructor lives on `Self`, not on a registry/factory abstraction. So the design still does not answer:
- who discovers providers?
- who instantiates them?
- how are provider-specific configs parsed?
- how does runtime selection work from `provider = "discord"`?

Current code avoids this problem by not pretending to have a provider system: `DiscordClient::from_config(...)` is a concrete constructor on a concrete type (`src/discord.rs:17-44`).

If you want trait objects, you almost certainly need a separate `ProviderFactory` / registry abstraction. The current doc does not define one.

### 2.3 Inbound providers, event sources, channel providers, and outbound providers are not cleanly distinguished

The architecture diagram introduces both:
- `Event Sources`
- `Inbound Providers`
- `Channel Providers`
- `Outbound Providers`

(`ARCHITECTURE.md:41-53`)

But the text later defines `EventSource` and `Provider` in a way that already overlaps those responsibilities (`ARCHITECTURE.md:130-190`). Inbound webhooks are shown as both a source concept and a provider concept. Outbound providers are also channel providers. This is vocabulary drift before implementation starts.

### 2.4 Multi-route delivery semantics are missing

This is a major omission.

Current router behavior is single-match, single-target:
- `route_for()` uses `.find(...)`, not collect-all (`src/router.rs:108-121`)
- `preview_delivery()` returns one `DeliveryPreview` (`src/router.rs:36-79`)
- `dispatch()` sends exactly one message (`src/router.rs:31-34`)

The proposal wants one event to fan out to multiple providers (`ARCHITECTURE.md:217-231`) but does not define:
- deterministic ordering
- whether failures are fail-fast, best-effort, or aggregated
- retries / backoff
- duplicate suppression / idempotency
- per-provider timeout behavior
- whether partial success is considered success

That is not an implementation detail. That is the behavior.

### 2.5 The security model is missing

This matters more than the doc seems to realize.

Current routing already supports optional dynamic token execution, including shell commands (`src/router.rs:44-63`, `src/dynamic_tokens.rs:50-91`). In v0.2 that is already a trust-boundary feature. Once you add:
- inbound providers/webhooks
- plugin-style extension points
- multi-provider delivery
- stored templates / transforms

...you need an explicit security story. Right now there is none. The doc does not say:
- whether inbound events may trigger dynamic tokens
- how plugin code is trusted/sandboxed
- what happens if a Notion/Slack route uses shell-backed templates
- how secrets are scoped per provider

That is a serious gap.

### 2.6 Persistence is too vague for the amount of state being proposed

`SessionManager` and `ProjectStore` are presented as if they are straightforward add-ons (`ARCHITECTURE.md:248-303`), but the current system has almost no persistence model beyond config. The closest thing to stateful runtime tracking is in-memory tmux registration (`src/daemon.rs:21-28`, `src/daemon.rs:119-133`, `src/monitor.rs:20-35`).

The proposal does not specify:
- write frequency / compaction
- crash recovery guarantees
- file locking / concurrent writers
- schema versioning
- garbage collection / retention
- how session identity is derived

For "local markdown/JSON" this is not a nit. It is the difference between usable and corrupting user state.

### 2.7 The plugin story is aspirational, not specified

The document opens with "Everything is a plugin" (`ARCHITECTURE.md:7`) but the repo's current plugin system is just manifest discovery for bridge scripts (`src/plugins.rs:11-84`), and the runtime only exposes a `plugin list` CLI path (`src/main.rs:208-229`).

There is currently no plugin lifecycle, no execution model, no event hook API, and no runtime integration. So the proposal needs to distinguish:
- compile-time internal extension points
- bundled script bridges
- future runtime plugins

Right now it conflates them.

### 2.8 Dead-code cleanup is missing from the migration plan

This repo already shows architectural drift. `src/main.rs` only wires `daemon`, `monitor`, `router`, `discord`, etc. (`src/main.rs:1-13`), but the repository also contains `src/server.rs` and `src/watch.rs`, which appear to be older alternate architectures and are not part of the current module tree. `src/watch.rs` even refers to config types that do not exist in the current `config.rs`.

Before adding more abstraction, the proposal should explicitly say: clean out or quarantine dead architecture experiments first.

---

## 3) Potential pitfalls and risks

### 3.1 The proposed `Provider` trait mixes too many responsibilities

The current code has a fairly natural separation already:
- rendering lives on events (`src/events.rs:491-717`)
- route selection lives in router (`src/router.rs:36-145`)
- transport lives in `DiscordClient` (`src/discord.rs:46-94`)

The proposed `Provider` trait combines:
- config-time construction
- capability declaration
- rendering
- sending

(`ARCHITECTURE.md:135-165`)

That is the wrong place to fuse concerns.

Concretely, `render()` does not belong on the same trait as `send()` unless you want every provider to own formatting policy. But current code already shows that formatting is partly event-specific and partly route/template-specific. Making the provider own rendering will either:
- duplicate formatting logic across providers, or
- force generic rendering decisions into transport adapters.

I would split this.

### 3.2 `broadcast` is a risky default for the core pipeline

I do **not** think `tokio::sync::broadcast` is the right default for the main event pipeline.

Why:
- today there is effectively one critical consumer path: route + deliver (`src/router.rs:31-34`, `src/monitor.rs:565-579`)
- `broadcast` is best when all subscribers independently want the same stream
- `broadcast` drops messages for lagging receivers; slow consumers get lag errors instead of backpressure
- every subscriber clones every event whether it needs it or not

That is fine for metrics/dashboard/debug taps. It is not great for the primary "this event must be routed and delivered" path.

My recommendation:
- **Use `mpsc` for source -> dispatcher ingress**
- optionally use `broadcast` only for non-critical observers (metrics, dashboard, debug tail, maybe session snapshots)

A hybrid model is much more defensible than making broadcast the spine of the system.

### 3.3 Multi-provider routing is a bigger semantic break than the document admits

Because the current router is single-match and single-delivery (`src/router.rs:31-34`, `src/router.rs:108-121`), moving to multi-provider routing means changing:
- router return type
- dispatch behavior
- error propagation
- tests
- CLI/HTTP response semantics
- monitor behavior when one sink fails and another succeeds

That is not an additive change hiding behind a new `provider` field.

### 3.4 The proposed generic target model is too weak

`target: String` is convenient in a document (`ARCHITECTURE.md:197-207`), but it is a poor long-term abstraction.

Discord channel IDs, Discord webhooks, Slack channel names, Slack webhook URLs, Notion database IDs, Jira project keys, and thread references do not have the same validation rules or capabilities.

Current code's `DeliveryTarget` enum is narrow but at least honest (`src/router.rs:10-13`). The proposed generic string target risks turning the new architecture back into "stringly typed but with more layers."

### 3.5 The scope explosion is real

The doc is trying to ship, in sequence:
- typed events
- provider abstraction
- event bus
- multi-provider delivery
- session manager
- project store
- agent runtime integration
- bidirectional sync
- orchestration layer

(`ARCHITECTURE.md:373-405`)

That is not one release worth of architectural change from a 0.2 codebase that is still strongly centered on Discord transport. If treated literally, this plan is likely to stall halfway through a refactor.

### 3.6 The proposal understates current duplication and drift

There is already overlap between `monitor.rs` and `tmux_wrapper.rs` for tmux tracking logic, and there are dead alternate modules sitting in the repo. If this isn't addressed first, new abstractions will be layered on top of unresolved duplication rather than replacing it.

---

## 4) Concrete suggestions for improvement

### 4.1 Shrink v0.3 dramatically

My strong recommendation:

#### v0.3 should only do this:
1. introduce a real internal event model
2. introduce `Source` abstraction
3. introduce `Sink` abstraction with **Discord only**
4. change router from single target to `Vec<ResolvedDelivery>`
5. keep config migration limited to Discord-compatible forms

#### v0.4+ can do this:
- Slack
- persistent session/project state
- inbound sync
- dashboard
- agent orchestration features

Right now the doc mixes a necessary refactor with a product re-founding.

### 4.2 Replace `Provider` with smaller abstractions

Suggested split:
- `EventSource` / `Source`
- `Router` / `Matcher`
- `Renderer` (or route/template renderer)
- `Sink` / `Destination`
- `ProviderFactory` or `Registry` for config-time construction

That split matches the current code's natural seams much better:
- source logic: `src/monitor.rs`, `src/tmux_wrapper.rs`
- routing logic: `src/router.rs`
- rendering logic: `src/events.rs`
- transport logic: `src/discord.rs`

### 4.3 Make events actually typed or stop claiming they are

Pick one:

#### Option A: real typing
Use an enum/variant model like:
- `EventEnvelope { id, timestamp, source, priority, body }`
- `EventBody::{GitCommit(GitCommit), GitHubIssueOpened(...), ...}`

#### Option B: honest dynamic model
Keep payload as JSON, but stop claiming strong typing and instead define schemas + validation.

Current proposal is stuck in the middle.

### 4.4 Use `mpsc` for the primary pipeline

Recommended flow:
- sources send `EventEnvelope` into an `mpsc` queue
- dispatcher/router worker consumes in order
- dispatcher resolves 0..N deliveries
- sink workers handle transport/retry policy
- optional broadcast/debug stream mirrors dispatched events for observability

That fits the current codebase's actual needs better than full pub/sub fan-out.

### 4.5 Write the migration plan as vertical slices, not layer slices

Current phases are too abstraction-led. I would rewrite them as:

1. **Internal compatibility layer**
   - keep current CLI/API
   - wrap `IncomingEvent` into new internal event type
   - no provider changes yet

2. **Router generalization**
   - router resolves multiple deliveries
   - Discord sink still only sink

3. **Source extraction**
   - move git/tmux polling behind `Source`
   - remove duplication between `monitor.rs` and `tmux_wrapper.rs`

4. **Second provider**
   - add Slack only after Discord path is stable

5. **Stateful features**
   - add session/project persistence after event pipeline settles

That path is much more believable.

### 4.6 Add explicit delivery semantics to the doc

The proposal should answer, in writing:
- are events best-effort or reliable?
- do we retry? where?
- what counts as success for 3 matched routes if 1 fails?
- do we persist failed deliveries?
- do we dedupe identical events?
- is ordering guaranteed per source / per target / globally?

Without this, implementers will invent incompatible behaviors.

### 4.7 Add a compatibility matrix section

For config migration, define old -> new mapping explicitly:
- `[discord].token` -> `[providers.discord].token`
- `[discord].default_channel` -> `[providers.discord].default_channel` or `[defaults].channel`
- route with `channel` only -> `provider = "discord"`, `target = channel`
- route with `webhook` only -> `provider = "discord"`, `target = webhook`, `mode = "webhook"`
- `monitors.git` -> `sources.git`
- `monitors.tmux` -> `sources.tmux`

Then back it with tests.

### 4.8 Clean the repo before doing the refactor

Explicitly remove or quarantine legacy architectural experiments (`src/server.rs`, `src/watch.rs`) before landing the new model. Otherwise the repo will contain three half-architectures at once.

---

## 5) Whether the migration path from v0.2 is realistic

### Short answer
**Partially realistic in a reduced scope; unrealistic as currently written.**

### What is realistic
- wrapping the existing Discord transport behind a first sink/provider abstraction is realistic, because `DiscordClient` is already a reasonably isolated transport adapter (`src/discord.rs:17-94`)
- extracting git/tmux monitoring behind source traits is realistic, because those loops are already grouped by concern (`src/monitor.rs:144-516`)
- config aliasing for some Discord fields is realistic, because current config normalization already does compatibility work (`src/config.rs:245-255`, `src/config.rs:449-474`)

### What is not realistic
- claiming "no breaking CLI changes" while also changing routing to multi-provider and adding new source/provider models (`ARCHITECTURE.md:367-371`) is too optimistic
- shipping session manager + project store + agent runtime as part of the same architectural epoch is too much scope (`ARCHITECTURE.md:388-405`)
- presenting config migration as simple aliases hides the hardest part: route semantics and provider-specific target parsing

### My verdict
If you trim v0.3 to **event model + router generalization + Discord-as-first-sink + source extraction**, yes.

If you keep the full current vision attached to v0.3-era migration language, no.

---

## 6) Compare the proposed abstractions against the actual current code — are the trait boundaries in the right places?

### `EventSource`
**Mostly yes.**

This is the cleanest boundary in the proposal. The current code already behaves like it wants this abstraction.

### `Provider`
**No. Wrong boundary.**

The current code suggests these boundaries instead:
- event rendering boundary in `src/events.rs:491-717`
- route-resolution boundary in `src/router.rs:36-145`
- transport boundary in `src/discord.rs:46-94`

The proposal's provider trait collapses these into one layer (`ARCHITECTURE.md:135-165`). I think that is a mistake.

### `Router`
**Half right.**

The router is absolutely the right place for rule matching. It is **not** yet specified well enough for:
- 0..N route matches
- per-route formatting/template overrides
- partial failure behavior
- fallback/default route rules

Current router logic is very simple and deterministic because it only returns one target (`src/router.rs:36-145`). Once you generalize it, the API should probably produce a list of resolved deliveries, not perform transport itself.

### `EventBus`
**Not as the central primitive.**

If you want a bus at all, it should be behind a dispatcher abstraction and probably not implemented as `broadcast` for the core route-and-deliver path.

### `SessionManager` / `ProjectStore`
**Too early / too detached from current code.**

They may be valid future concepts, but they are not natural next seams from the current implementation. They feel product-driven, not refactor-driven.

---

## 7) Is EventBus (`broadcast`) the right choice or would channels/mpsc be better?

**For the core pipeline, mpsc is better.**

### Why mpsc fits better now
Current clawhip has one main job: accept events and deliver notifications. That is a queue/dispatcher problem, not a pub/sub problem.

`mpsc` gives you:
- a single authoritative ingress queue
- explicit backpressure
- simpler failure handling
- easier ordering guarantees
- clearer ownership of retries

### Why broadcast is still useful
`broadcast` is fine for:
- debug/event tail views
- metrics subscribers
- dashboards
- maybe eventually-consistent session observers

### Recommended architecture
- `mpsc<EventEnvelope>` for source -> dispatcher
- dispatcher resolves deliveries
- per-sink worker queues for transport/retry
- optional `broadcast<EventEnvelope>` mirror for observability

That is a much safer default than making broadcast the backbone.

---

## 8) Config backward compat feasibility

### Short answer
**Feasible, but only if you treat it as a real migration layer, not a couple of aliases.**

### Why it is feasible
The current config code already has a compatibility mindset:
- Discord token may come from env or config (`src/config.rs:271-298`)
- legacy default channel is folded into defaults (`src/config.rs:253-255`)
- validation already supports webhook-only delivery without a bot token (`src/config.rs:311-332`)

So the codebase is not hostile to compatibility work.

### Why it is harder than the doc suggests
The current route schema is Discord-specific:
- `channel: Option<String>`
- `webhook: Option<String>`
- optional `mention`, `format`, `template`

(`src/config.rs:70-82`)

The proposed route schema is provider-based with generic `target` (`ARCHITECTURE.md:197-207`). Mapping old routes to new routes is manageable for Discord, but only if you explicitly encode whether the legacy target was:
- Discord bot-channel delivery, or
- Discord webhook delivery

That is a real semantic migration, not just field renaming.

### Recommendation
Support backward compatibility in two stages:
1. parse old config into old structs
2. normalize into a new internal config model

Do **not** try to make one serde struct represent both old and new worlds cleanly. That tends to become a pile of optional fields and hidden precedence rules.

### Required tests
Before shipping, add golden tests for:
- old Discord-only config
- mixed legacy + new config
- routes without provider
- routes with webhook target
- conflicting legacy/new fields
- monitors -> sources alias behavior

---

## Final verdict

### As a vision
**Good.** The proposal is solving the correct problem.

### As a v0.3 implementation plan
**Not good enough yet.** I would not treat this document as implementation-ready.

### Brutally honest bottom line
The proposal is trying to jump from "Discord router with some monitors" to "Claw OS" in one architectural breath. That is too much. The right move is to ship a much smaller v0.3:
- extract real source/sink boundaries
- generalize router to multiple deliveries
- keep Discord as the only real sink at first
- add a compatibility layer
- defer session/project/orchestration ambition until the pipeline is stable

If you do that, this can become a strong architecture.

If you do not, there is a serious risk of spending a long time building abstractions that look future-proof while making the present code harder to change.
