# ecal-mcp — agent skill

Diagnose and operate Eclipse eCAL pub/sub networks via the `ecal-mcp` server. This skill teaches the preferred workflow for the questions agents actually face on an eCAL bus, in the order an agent should reach for them.

> **When to use this skill:** the user asks anything of the form "is X publishing?", "why isn't subscriber Y getting data?", "is topic Z at N Hz?", "which replica answered the service call?", or "what's on the eCAL network?". If the question is about Eclipse eCAL pub/sub or services and you have access to the `ecal-mcp` server (MCP tools or the `ecal-mcp` CLI), use this skill.

> **Naming follows upstream eCAL.** Field names mirror eCAL's own headers (`SDataTypeInformation.name`, `STopic.transport_layer`, `STopic.direction`, `SServer.service_id`, `STopic.data_frequency` stored in mHz, …). Where we depart we say so in the field doc — e.g. `registered_data_frequency_hz` is the same value converted to Hz.

> **Where we depart from upstream.** Most of the wire follows upstream eCAL one-for-one. The fields below are **MCP-layer inventions** with no direct upstream counterpart — keep them in mind when grepping the eCAL headers expecting a match:
>
> - `did_you_mean` (finding code + structured candidates; always fires on negative-existence findings — `candidates: []` and `suggestion: null` are the documented "we looked, nothing close" signal, not silent absence)
> - `shm_domain_split` (finding code; upstream has no such concept, `STopic.shm_transport_domain` is just a per-entity string; suppressed entirely when a real transport is already negotiated or every host's domain == its hostname — pure noise otherwise)
> - `type_signatures[]` and its per-side occurrence / `topic_id` rollup
> - `delta_direction`, `meets_spec`, `ratio_to_target`, `tolerance_pct`, `jitter_pct` on `ecal_topic_stats`
> - `multiple_publishers`, `rate_below_spec`, `rate_above_spec` finding codes
> - `descriptor_fingerprint` (first-16-hex SHA-256 over the descriptor bytes; upstream's `SDataTypeInformation::operator==` covers all three components, this is a cheap field-level proxy)
> - `severity` on every `Finding` (used to sort the list)
>
> The other fields lifted directly from upstream `SMonitoring` — topic / process / service / transport-layer field set, `STopic.message_drops`, `data_clock`, `connections_*`, etc. — are copied through with their upstream names and types. Two caveats: the `hosts` roster is **derived** by ecal-mcp (not a member of `SMonitoring`), and `STopic.data_frequency` (millihertz, `int32`) is converted to Hz and surfaced as `registered_data_frequency_hz`. `findings[].code` is an ecal-mcp diagnostic enum — the codes are upstream-grounded but the field is not part of `SMonitoring`.

---

## Tool surface (what's available)

Discover live with `ecal-mcp tools` — that command emits the canonical JSON descriptors. The set in this build:

| Tool                        | One-line purpose                                                            |
| --------------------------- | --------------------------------------------------------------------------- |
| `ecal_get_monitoring`       | Single-call snapshot: pubs + subs + services + clients + processes + hosts. |
| `ecal_list_hosts`           | Per-host roster (counts, `shm_transport_domains`, `runtime_versions`).      |
| `ecal_list_processes`       | Roster of eCAL processes on the network.                                    |
| `ecal_list_publishers`      | All registered publishers; `name_pattern` substring-filters by topic.       |
| `ecal_list_subscribers`     | All registered subscribers; same filter shape.                              |
| `ecal_list_services`        | RPC servers + per-server `service_id`, methods, `version`, ports.           |
| `ecal_list_service_clients` | RPC clients; rare to need.                                                  |
| `ecal_topic_stats`          | Live measurement window for a topic. Pass `expected_hz` for spec check.     |
| `ecal_diagnose_topic`       | First-tool-to-reach for "why is this topic broken?". Returns findings.      |
| `ecal_subscribe`            | Capture N samples from a topic and return them.                             |
| `ecal_publish`              | One-shot publish from the agent process.                                    |
| `ecal_call_service`         | Invoke an RPC method; supports targeted single-replica calls.               |
| `ecal_get_logs`             | Drain pending eCAL log buffer (filterable by level / process / timestamp).  |

---

## Decision tree (use this verbatim)

```
User asks…
├── "is topic X flowing / why isn't it?"            → ecal_diagnose_topic
│   └── If finding has code=did_you_mean            → use detail.candidates[*].topic_name
├── "is topic X at N Hz / within spec?"             → ecal_topic_stats expected_hz=N (tolerance_pct=… if needed)
├── "is the topology correct (1:1 vs 1:N producers)?" → ecal_diagnose_topic (look for findings[multiple_publishers])
├── "what's on the network?"                        → ecal_get_monitoring     (single coherent snapshot)
├── "which hosts are visible?"                      → ecal_list_hosts
├── "show me a few samples on topic X"              → ecal_subscribe duration_ms=… max_samples=…
├── "call service S, only on replica service_id=E"  → ecal_call_service target_service_id=E
└── "what payload type does topic X carry?"         → ecal_list_publishers name_pattern=X  (datatype_information.name + encoding + descriptor_fingerprint + descriptor_len)
```

If the very first call doesn't answer the question, the second call is **almost always** `ecal_get_monitoring` (one shot) or one of the narrower `ecal_list_*` tools to confirm the topology assumption.

---

## Workflow recipes

### 1. "Why is topic `/foo` silent?" — _the_ most common question

One call:

```text
ecal_diagnose_topic topic_name=/foo duration_ms=2000
```

Branch on `findings[].code` (do **not** pattern-match `message` — it will be reworded):

Every `Finding` now carries a `severity` (`error` / `warning` / `info`) and the list is sorted by **likely-root-cause** with `severity` as a tiebreaker, so a load-bearing `warning` (e.g. `multiple_publishers`) sorts above a lower-priority advisory regardless of severity. **Switch on `code`**, then read `detail`. Severity is for UX (red/yellow/grey), not the sort key.

| `code`                         | Severity      | Action                                                                                                                            |
| ------------------------------ | ------------- | --------------------------------------------------------------------------------------------------------------------------------- |
| `no_publishers_or_subscribers` | `error`       | Topic doesn't exist. Inspect any `did_you_mean` finding — `detail.candidates[*]` is the ranked typo list.                          |
| `no_publisher`                 | `error`       | Subscribers exist; producer side missing. Report the subscriber's _expected_ type from `subscribers[].datatype_information.name`. A `did_you_mean` finding now also fires here, keyed off the publisher roster. |
| `no_subscriber`                | `error`       | Publisher running but nobody listening — usually fine, sometimes a deploy-order bug. `did_you_mean` likewise fires off the subscriber roster. |
| `type_metadata_mismatch`       | `error`       | `detail.signatures[]` carries one row per distinct `(name, encoding)` with `descriptor_fingerprint`, `descriptor_len`, `publisher_count`, `subscriber_count`, and `publisher_topic_ids` / `subscriber_topic_ids` attributing each row back to entities (the same data is also surfaced on the response's top-level `type_signatures[]`). `live_stats.type_name` / `.encoding` are nulled while a mismatch is in effect (the live receive callback binds to whichever signature TCP delivered first). |
| `no_common_transport`          | `error`       | Fires when both publishers and subscribers advertise non-empty active transport layers but `intersect_active_transports` finds **no common active layer**. Limitation: this is a *runtime-active* check — eCAL monitoring's `SProcess` / rustecal `ProcessInfo` does not carry per-layer enable flags (those live on `STopic.transport_layer`), so we cannot detect a layer that's *configured-but-not-yet-active* during early TCP session setup. The detection therefore races with handshake. |
| `unhealthy_process`            | `error`       | Owning process called `Process::SetState` with severity≥2; quote `detail.state_info`. `detail` carries `host_name`, `unit_name`, `process_id`, `topic_id` for the offending row. |
| `multiple_publishers`          | `warning`     | ≥2 publishers registered for a topic that also has ≥1 subscriber — the 1:N producer anti-pattern. `detail.publishers[]` attributes each one (host/unit/pid/topic_id/rate/data_clock); `detail.rate_divergence_ratio` and `host_divergence` summarise the spread. `ecal_topic_stats` will also echo `publishers[]` whenever this case is in effect. |
| `message_drops`                | `warning`     | Subscriber-side eCAL counted dropped samples. `detail` carries `host_name`, `unit_name`, `process_id`, `topic_id`, `message_drops` (cumulative), and — when prior state is cached — `drop_rate_per_sec`, `drops_since_last_snapshot`, `window_seconds` (see the rate-cache note below). **Application-layer hint:** TCP active + nonzero drops + (when plumbed) nonzero `data_latency_us.mean` ⇒ callback-blocked. See `/tmp/ecal/doc/rst/advanced/message_drops.rst` for the per-layer drop taxonomy. |
| `rate_below_spec` / `rate_above_spec` | `warning` | Live `wire_data_frequency_hz` is outside `expected_hz ± tolerance_pct`. `detail` mirrors `ecal_topic_stats` (`wire_data_frequency_hz`, `expected_hz`, `tolerance_pct`, `ratio_to_target`, `delta_hz`, `delta_direction`). Pass `expected_hz` (and optionally `tolerance_pct`) to `ecal_diagnose_topic` to enable. |
| `no_samples_observed`          | `warning`     | Pub registered, but the live window saw nothing. Most likely: producer process alive but not calling `Send`.                      |
| `shm_domain_split`             | `warning`     | >1 host with >1 SHM domain string. **Suppressed entirely in v8** when a real common transport (e.g. TCP) is already negotiated, OR when every host's `shm_transport_domain == host_name` (the "no real config divergence" cross-host topology) — pure noise otherwise. The split is still observable structurally via `len(shm_transport_domains) >= 2`. |
| `did_you_mean`                 | `info`        | Paired with `no_publishers_or_subscribers`, `no_publisher`, or `no_subscriber`. **Always fires** on negative-existence — `detail.candidates[]` may be empty and `detail.suggestion` may be `null`, both meaning "we looked, nothing close". Non-empty `detail.candidates[*]` is structured `{topic_name, score, direction, occurrences, host_names[], registered_data_frequency_hz}`. The `score` is a 3-gram-overlap heuristic — useful for ranking, not a normalized distance. |

> **`drop_rate_per_sec` requires a long-lived process.** The `message_drops` finding's rate fields (`drop_rate_per_sec`, `drops_since_last_snapshot`, `window_seconds`) are deltas against an **in-process** cache (60 s TTL, keyed by `(topic_id, subscriber_topic_id)`). A fresh `ecal-mcp` CLI invocation starts with an empty cache, so on the very first diagnose call after spawn these three keys are absent (only the cumulative `message_drops` is populated). Persistent rate tracking requires `ecal-mcp serve` (or any long-lived host that keeps the same process alive across calls); the per-call CLI shell-out cannot produce them. Treat their absence as "no prior snapshot", not "no drops".

### 2. "Is `/foo` at the expected rate?"

```text
ecal_topic_stats topic_name=/foo duration_ms=4000 expected_hz=50 tolerance_pct=0.05
```

Read `meets_spec` first (boolean). If false:

- Use **`wire_data_frequency_hz`** as the reported rate (`= 1e6 / mean_gap_us`). Cross-check with the cached `registered_data_frequency_hz` on `ecal_list_publishers` if you need the registration value (upstream `STopic.data_frequency` mHz, converted to Hz; *not* the wire).
- `ratio_to_target` is the multiplicative ratio (`0.02` = "2 % of target", `1.0` = on target).
- `delta_direction` is a self-describing label: `on_spec`, `below_spec`, `above_spec` — quote it instead of inferring from `delta_hz` sign.
- Echo `tolerance_pct` from the response so the report is self-describing.
- For jitter signal: `jitter_pct` < 0.002 (0.2 %) ⇒ steady; `jitter_pct` > 0.05 ⇒ bursty stream.
- **`publishers[]` is echoed whenever ≥2 publishers exist on the topic** — same `(host_name, unit_name, process_name, topic_id, registered_data_frequency_hz)` rollup `ecal_diagnose_topic` would surface inside `findings[multiple_publishers]`. If you see this, your rate read is a *combined* one, not a per-pub one.

### 3. "Survey the network — what's running?"

One call covers everything in a single snapshot:

```text
ecal_get_monitoring
```

Returns `publishers`, `subscribers`, `services`, `clients`, `processes`, and a derived `hosts` roster. Prefer this over four sequential `ecal_list_*` calls — those re-snapshot independently and can race.

Caveat: **`processes` may return different subsets on back-to-back CLI invocations** (each CLI invocation re-discovers eCAL). `ecal_list_hosts` aggregates over all of `publishers`, `subscribers`, `services`, `clients`, `processes`, and the raw process records (the latter so hosts that have eCAL processes with no pubs/subs/services still contribute their `shm_transport_domain` to the per-host union) — so prefer it for a stable host roster over `ecal_list_processes` alone.

### 4. "Call service `S` on exactly one replica"

```text
ecal_list_services name_pattern=S        # collect service_id of the replica you want
ecal_call_service service=S method=M text=… target_service_id=<service_id>
```

Read the response for evidence:

- `discovered_instances` = how many replicas were visible at call time.
- `requested_target_service_id` echoes your filter, so `len(responses)==0` with `discovered_instances > 0` and a non-null `requested_target_service_id` ⇒ "filter rejected every replica" (vs. `discovered_instances: 0` ⇒ "service is gone").
- `responses[].service_id` = which replica actually answered. With `target_service_id` set this should be exactly one and equal to the requested id.
- `instance_index` is an index within `responses[]`, NOT into the discovered fleet. With a target filter it's always `0`.

`methods[].call_count` is an **in-process** counter incremented on each handled request (server) or issued call (client) since *that process started*. It is **not** a cluster-wide total; rediscovery handshakes can also bump it, and a server restart resets it. Treat it as a per-replica fingerprint (diff two snapshots to attribute traffic), not a usage metric.

`service_id` is **not stable across process restarts** (it's a per-process scalar `eCAL::EntityIdT` — `uint64_t` — minted fresh on each server start). Re-discover via `ecal_list_services` after every restart. Note: upstream C++ headers declare `EntityIdT = uint64_t`; the C ABI / rustecal / ecal-mcp surface it as `int64`/`i64`. Fits all current values; only matters for raw memcpy interop.

### 5. "What type does `/foo` carry?"

```text
ecal_list_publishers name_pattern=/foo include_descriptors=true
```

Each row carries `datatype_information.name`, `.encoding`, `.descriptor_len`, `.descriptor_fingerprint` (always present when `descriptor_len > 0` — first 16 hex chars of SHA-256), and (opt-in) `.descriptor_base64`. Same shape on subscribers (subscriber-side `name`/`encoding` are the _expected_ type — what the subscriber declared at construction). Two rows that share `(name, encoding)` but differ on `descriptor_fingerprint` are running incompatible schema versions of the same logical type.

### 5b. "What's a service method's request / response type?"

`ecal_list_services` (and the `servers[]` field on `ecal_get_monitoring`) carries the same shape on every method:

```text
ecal_list_services name_pattern=S include_descriptors=true
```

Each `methods[*]` row has `request_datatype_information` and `response_datatype_information`, both with the full `{name, encoding, descriptor_len, descriptor_fingerprint, descriptor_base64?}` triple — mirroring `TopicEntry.datatype_information` and upstream `SMethod.{request,response}_datatype_information` (`monitoring.h:149-150`). Pass `include_descriptors=true` to get the protobuf descriptor blob for a typed RPC call.

---

## Field-level gotchas (what to trust, what to ignore)

These will save you a debug session:

- **`transport_layer[*].active=false` does NOT mean "transport broken".** eCAL v6 `CPublisher::Send` short-circuits with `GetSubscriberCount() == 0`, so a healthy publisher with no matched subscriber will sit with every layer at `active=false` indefinitely. Symmetrical for subscribers awaiting their first sample. Use `data_clock` (advances on every `Send`) for "is the producer alive?".
- **`registered_data_frequency_hz` is a _cached registration_ value**, upstream `STopic.data_frequency` (stored in millihertz, converted to Hz at our boundary). It is **not** the live wire rate. Quote `ecal_topic_stats.wire_data_frequency_hz` for the authoritative live rate.
- **`registration_clock` is the local monitoring counter**, not a producer heartbeat. Flat across snapshots ⇒ stale registration ⇒ producer may have died (entries persist for `registration_timeout`, default 10s).
- **`state_severity` is the health enum (0=unknown, 1=healthy, 2=warning, 3=critical, 4=failed)**. Treat `>= 2` as unhealthy. `state_severity_level` is an _independent_ finer enum (1..5); default-registered processes have `(severity=0, level=1)`, which does NOT mean "healthy" — only `severity` does.
- **Subscriber-side `datatype_information` is the _expected_ type**, not a peer-derived value. In a `no_publisher` finding, that's the type the subscriber declared at construction.
- **`registered_data_frequency_hz` is direction-ambiguous.** It is emitted on every row regardless of `direction`. On a publisher row it's the publisher-declared send rate (cached registration value); on a subscriber row it's the subscriber-declared expected rate (often `0` — most subscribers don't fill it in). Do **not** read it as an inferred consumption rate.
- **`methods[*].request_datatype_information` / `response_datatype_information`** are the upstream-faithful name for service method types — full `{name, encoding, descriptor_len, descriptor_fingerprint, descriptor_base64?}`. A row whose two `name` / `encoding` fields are empty and `descriptor_len == 0` means "type not advertised" (e.g. raw `add_method` registration), not "no payload". Send `text=…` or `payload_base64=…`.
- **`service_id` is per-server, not per-method.** Each `ServiceEntry` carries one `service_id`; the `methods[]` array shares it. (Upstream `SServer.service_id` is a scalar `eCAL::EntityIdT` — `uint64_t` — not a struct; we surface the same 64-bit value as `int64`/`i64` through the C ABI and on out the wire. Fits all current values; the width discrepancy only matters for raw memcpy interop.) It is **not stable across restarts** — don't persist it; re-discover via `ecal_list_services`.
- **`response_base64` may be absent on `ecal_call_service` / samples in `ecal_subscribe`** when the payload is short printable UTF-8 (≤256 bytes): the bytes are fully recoverable from `response_text` / `text`. Binary or large responses always include the base-64 form.
- **Subscriber-side `connections_external` is structurally `0`.** Upstream `/tmp/ecal/ecal/core/src/pubsub/ecal_subscriber_impl.cpp` zeros both `connections_local` and `connections_external` on every subscriber registration with the comment *"we do not know the number of connections"* — so even when the publisher correctly reports `connections_external == 1` for a matched cross-host subscriber, the subscriber-side row will always read `0`. This is by upstream design, not a transport edge case. To check whether a publisher and subscriber are bound, read the **publisher's** `connections_local` / `connections_external` plus `STopic.data_clock` (advances on every `Send`).
- **`ecal_subscribe.samples_truncated_at_cap` is an MCP response-cap, NOT `STopic.message_drops`.** It counts samples that arrived during the window but were dropped because `samples_collected` had already reached `max_samples`. Raise `max_samples` if you need more headroom — it does **not** indicate a transport- or callback-level drop. Use `findings[message_drops]` / `STopic.message_drops` for actual wire drops.

## CLI usage (when MCP isn't available)

The same `ecal-mcp` binary is also a CLI:

```bash
# Discover tools (pretty by default; --compact for one-line):
ecal-mcp tools

# Invoke a tool. Args are -a key=value (or --json '{...}' for complex).
ecal-mcp call ecal_diagnose_topic -a topic_name=/foo -a duration_ms=2000

# eCAL discovery is asynchronous; --settle-ms waits before the call
# (default 2000ms; set to 0 if you've already let discovery converge).
ecal-mcp call ecal_list_publishers --settle-ms 3000
```

Output is JSON on stdout, one document per `call`. eCAL's startup banners go to stderr — pipe stdout to `jq` directly without filtering.

---

## Anti-patterns (don't do these)

1. **Don't pattern-match `findings[].message`.** Switch on `code`. The `message` is human-readable and subject to rewording.
2. **Don't quote `registered_data_frequency_hz` from `ecal_list_publishers` as the live rate.** Use `ecal_topic_stats`.
3. **Use `wire_data_frequency_hz` from `ecal_topic_stats`** as the reported live rate (`= 1e6 / mean_gap_us`). Don't confuse it with `registered_data_frequency_hz` (the cached registration value).
4. **Don't persist `service_id` / `topic_id`.** They're per-process-lifetime; re-discover after every restart.
5. **Don't trust `ecal_list_processes` as the canonical roster.** Derive it from `ecal_get_monitoring` (which already aggregates) or from the union of pub/sub/service lists.
6. **Don't subscribe with `ecal_subscribe` just to ask "does the topic exist?"** Use `ecal_diagnose_topic` — no measurement window cost when you pass `duration_ms=0`.

---

## Upstream eCAL index (when a field surprises you)

The MCP surfaces upstream eCAL terminology where possible. If a field or finding doesn't behave the way this skill says, jump into the curated upstream pointers below — designed for ~10 minutes of focused reading. Pinned to eCAL `v6.1.1` (C++ core) and rustecal `main`. Rendered docs: <https://eclipse-ecal.github.io/ecal/>.

### Start here

- [Project README](https://github.com/eclipse-ecal/ecal/blob/v6.1.1/README.md): one-page pitch — local SHM + UDP/TCP IPC, pub/sub + RPC, zero-config peer discovery.
- [Docs index / introduction](https://eclipse-ecal.github.io/ecal/stable/getting_started/introduction.html): canonical "what is eCAL, what problems does it solve" narrative.
- [Setup guide](https://eclipse-ecal.github.io/ecal/stable/getting_started/setup.html): how to install eCAL on Windows / Ubuntu (PPA + `.deb`); does _not_ cover `ecal.yaml` — for that, see Configuration overview below.
- [Samples chapter](https://eclipse-ecal.github.io/ecal/stable/getting_started/samples.html): index of the canonical sample programs the rest of the docs reference.

### Concept docs (RST)

- [Monitoring](https://eclipse-ecal.github.io/ecal/stable/getting_started/monitor.html): how registration → mon snapshot → CLI/GUI works; required reading for any "list topics" tool.
- [Services](https://eclipse-ecal.github.io/ecal/stable/getting_started/services.html): RPC model, server/client/method semantics, how it differs from pub/sub.
- [Network mode](https://eclipse-ecal.github.io/ecal/stable/getting_started/network.html): local vs cloud, what "network_enabled" actually changes, multicast TTL caveats.
- [Player / Recorder](https://eclipse-ecal.github.io/ecal/stable/getting_started/recorder.html): HDF5 measurement format, how recording and replay interact with live topics.
- [eCAL internals](https://eclipse-ecal.github.io/ecal/stable/advanced/ecal_internals.html): the single best "how does the bus actually work" doc — registration layer, data layers, gates.
- [Transport layers overview](https://eclipse-ecal.github.io/ecal/stable/advanced/transport_layers.html): which layer is default for each scope (SHM same-host, UDP-MC inter-host; TCP optional inter-host alternative) and how the per-publisher `auto`/`on`/`off` mode works.
- [Threading model](https://eclipse-ecal.github.io/ecal/stable/advanced/threading_model.html): which internal threads `Initialize` spins up (UDP registration / monitoring / logging / payload) and per-entity threads (TCP 2/instance, SHM sync 1/memfile); explains why blocking a callback causes drops.
- [Message drops](https://eclipse-ecal.github.io/ecal/stable/advanced/message_drops.html): per-layer drop causes (UDP buffer overflow / TCP backpressure / SHM overwrite) plus application-layer drops when callbacks block — the "why messages disappear" reference.
- [eCAL in Docker](https://eclipse-ecal.github.io/ecal/stable/advanced/ecal_in_docker.html): SHM /dev/shm sharing, --ipc=host, hostname/uuid pitfalls — load-bearing for any container test harness.

### Canonical types (C++ headers)

- [`ecal/ecal.h`](https://github.com/eclipse-ecal/ecal/blob/v6.1.1/ecal/core/include/ecal/ecal.h): umbrella header; just `#include`s every public header. Use it to discover what the public surface actually is — no symbols are declared here.
- [`ecal/core.h`](https://github.com/eclipse-ecal/ecal/blob/v6.1.1/ecal/core/include/ecal/core.h): lifecycle (`Initialize` / `Finalize` / `Ok` / `IsInitialized`) and `GetVersion*`; this is the file every `int main` actually depends on.
- [`ecal/init.h`](https://github.com/eclipse-ecal/ecal/blob/v6.1.1/ecal/core/include/ecal/init.h): `Init::` bitflag values (`Publisher`, `Subscriber`, `Service`, `Monitoring`, `Logging`, `TimeSync`, `All`, `Default`, `None`) passed as the second arg to `Initialize`.
- [`ecal/monitoring.h`](https://github.com/eclipse-ecal/ecal/blob/v6.1.1/ecal/core/include/ecal/monitoring.h): `GetMonitoring(...)` API + entity filter masks; the function every tool calls.
- [`ecal/types/monitoring.h`](https://github.com/eclipse-ecal/ecal/blob/v6.1.1/ecal/core/include/ecal/types/monitoring.h): `SMonitoring`, `STopic`, `SProcess`, `SServer`, `SClient` structs returned by `GetMonitoring`.
- [`ecal/types.h`](https://github.com/eclipse-ecal/ecal/blob/v6.1.1/ecal/core/include/ecal/types.h): `SDataTypeInformation` (encoding / type / descriptor) — the topic-type contract used everywhere.
- [`ecal/registration.h`](https://github.com/eclipse-ecal/ecal/blob/v6.1.1/ecal/core/include/ecal/registration.h): public registration query API — `GetPublisherIDs` / `GetSubscriberIDs`, `GetPublishedTopicNames` / `GetSubscribedTopicNames`, `GetServerMethodNames` / `GetClientMethodNames`, plus `Add/RemPublisherEventCallback` for live discovery.
- [`ecal/pubsub/types.h`](https://github.com/eclipse-ecal/ecal/blob/v6.1.1/ecal/core/include/ecal/pubsub/types.h): `STopicId`, `SReceiveCallbackData`, the `ePublisherEvent` / `eSubscriberEvent` enums (incl. `dropped`), and the pub/sub event-callback payload structs.
- [`ecal/pubsub/publisher.h`](https://github.com/eclipse-ecal/ecal/blob/v6.1.1/ecal/core/include/ecal/pubsub/publisher.h): `CPublisher` (constructor takes data-type info + optional `Publisher::Configuration` + optional event callback), `Send` overloads (buffer / `CPayloadWriter` / `std::string`), `GetSubscriberCount`, `GetTopicId`.
- [`ecal/pubsub/subscriber.h`](https://github.com/eclipse-ecal/ecal/blob/v6.1.1/ecal/core/include/ecal/pubsub/subscriber.h): `CSubscriber` API; receive callback signature.
- [`ecal/service/types.h`](https://github.com/eclipse-ecal/ecal/blob/v6.1.1/ecal/core/include/ecal/service/types.h): `SServiceMethodInformation`, `SServiceResponse`, call states.
- [`ecal/service/server.h`](https://github.com/eclipse-ecal/ecal/blob/v6.1.1/ecal/core/include/ecal/service/server.h) & [`client.h`](https://github.com/eclipse-ecal/ecal/blob/v6.1.1/ecal/core/include/ecal/service/client.h): RPC server/client classes.
- [`ecal/process.h`](https://github.com/eclipse-ecal/ecal/blob/v6.1.1/ecal/core/include/ecal/process.h): host name / process id / severity reporting used in every monitoring sample.
- [`ecal/time.h`](https://github.com/eclipse-ecal/ecal/blob/v6.1.1/ecal/core/include/ecal/time.h): time API and pluggable time sync interface.
- [`ecal/log.h`](https://github.com/eclipse-ecal/ecal/blob/v6.1.1/ecal/core/include/ecal/log.h) + [`types/logging.h`](https://github.com/eclipse-ecal/ecal/blob/v6.1.1/ecal/core/include/ecal/types/logging.h): logging API + `SLogMessage` shape consumed by mon tools.

### Implementation deep-dives

- [`ecal/core/src/registration/`](https://github.com/eclipse-ecal/ecal/tree/v6.1.1/ecal/core/src/registration): how registration samples are produced/consumed across SHM and UDP — read when "topic vanishes after 10s" mysteries appear.
- [`ecal_registration_provider.cpp`](https://github.com/eclipse-ecal/ecal/blob/v6.1.1/ecal/core/src/registration/ecal_registration_provider.cpp): the periodic broadcast loop driving discovery.
- [`ecal/core/src/monitoring/ecal_monitoring_impl.cpp`](https://github.com/eclipse-ecal/ecal/blob/v6.1.1/ecal/core/src/monitoring/ecal_monitoring_impl.cpp): how `GetMonitoring` aggregates the registration cache — explains stale entries and timeouts.
- [`ecal/core/src/pubsub/ecal_publisher_impl.cpp`](https://github.com/eclipse-ecal/ecal/blob/v6.1.1/ecal/core/src/pubsub/ecal_publisher_impl.cpp) & [`ecal_subscriber_impl.cpp`](https://github.com/eclipse-ecal/ecal/blob/v6.1.1/ecal/core/src/pubsub/ecal_subscriber_impl.cpp): per-layer writer/reader plumbing; useful when a counter is zero on one side.
- [`ecal/core/src/ecal_descgate.cpp`](https://github.com/eclipse-ecal/ecal/blob/v6.1.1/ecal/core/src/ecal_descgate.cpp): description (type+descriptor) propagation — why `tdesc` is sometimes empty.

### Apps & CLI tools

- [`ecal_mon_cli`](https://github.com/eclipse-ecal/ecal/tree/v6.1.1/app/mon/mon_cli): scriptable monitor; the canonical reference for "what should a topic listing look like".
- [`ecal_mon_tui`](https://github.com/eclipse-ecal/ecal/tree/v6.1.1/app/mon/mon_tui): terminal UI variant; useful design reference for live displays.
- [`ecal_mon_gui`](https://github.com/eclipse-ecal/ecal/tree/v6.1.1/app/mon/mon_gui): canonical GUI; field names there match `SMonitoring` exactly.
- [`ecal_play`](https://github.com/eclipse-ecal/ecal/tree/v6.1.1/app/play): HDF5 replay tool — read `play_core` for measurement playback semantics.
- [`ecal_rec`](https://github.com/eclipse-ecal/ecal/tree/v6.1.1/app/rec): recorder daemon + GUI; `rec_client_core` shows how to subscribe-everything correctly.

### Rust bindings (rustecal)

- [rustecal README](https://github.com/eclipse-ecal/rustecal/blob/main/README.md): scope, supported version of eCAL, crate layout.
- [`rustecal-core/src/core_types/monitoring.rs`](https://github.com/eclipse-ecal/rustecal/blob/main/rustecal-core/src/core_types/monitoring.rs): Rust mirror of `SMonitoring` — handy when comparing what fields ecal-mcp surfaces.
- [`rustecal-core/src/core_types/logging.rs`](https://github.com/eclipse-ecal/rustecal/blob/main/rustecal-core/src/core_types/logging.rs): Rust mirror of `SLogMessage`.
- [`rustecal-core`](https://github.com/eclipse-ecal/rustecal/tree/main/rustecal-core): init / config / monitoring entrypoints in Rust.
- [`rustecal-pubsub`](https://github.com/eclipse-ecal/rustecal/tree/main/rustecal-pubsub): typed publisher/subscriber wrappers.
- [`rustecal-service`](https://github.com/eclipse-ecal/rustecal/tree/main/rustecal-service): typed service client/server wrappers.
- [`rustecal-sys`](https://github.com/eclipse-ecal/rustecal/tree/main/rustecal-sys): raw `bindgen` FFI surface — the ground truth for "is this symbol actually exposed".

### Examples (canonical idioms)

- [`person_send` (protobuf)](https://github.com/eclipse-ecal/ecal/tree/v6.1.1/serialization/protobuf/samples/pubsub/person_send): the textbook typed publisher; pair with `person_receive`.
- [`blob_send` (binary)](https://github.com/eclipse-ecal/ecal/tree/v6.1.1/ecal/samples/cpp/pubsub/binary/blob_send): untyped raw-buffer publisher; pair with `blob_receive`.
- [`hello_send` (string)](https://github.com/eclipse-ecal/ecal/tree/v6.1.1/serialization/string/samples/pubsub/hello_send): minimal "publish a string" — the smallest correct program.
- [`math_server` / `math_client`](https://github.com/eclipse-ecal/ecal/tree/v6.1.1/serialization/protobuf/samples/services): canonical typed RPC pair.
- [`mirror_server` / `mirror_client`](https://github.com/eclipse-ecal/ecal/tree/v6.1.1/ecal/samples/cpp/services): untyped binary RPC; the simplest service example.
- [Rust `person_send`](https://github.com/eclipse-ecal/rustecal/tree/main/rustecal-samples/pubsub/person_send) & [`monitoring_receive`](https://github.com/eclipse-ecal/rustecal/tree/main/rustecal-samples/monitoring/monitoring_receive): Rust mirrors of the C++ idioms above.

### Configuration & transport layers

- [Configuration overview](https://eclipse-ecal.github.io/ecal/stable/configuration/options.html): every key in `ecal.yaml` and what it controls.
- [Local-only config](https://eclipse-ecal.github.io/ecal/stable/configuration/local.html) & [network config](https://eclipse-ecal.github.io/ecal/stable/configuration/network.html): the two recipes 95% of deployments start from.
- [`ecal/config/configuration.h`](https://github.com/eclipse-ecal/ecal/blob/v6.1.1/ecal/core/include/ecal/config/configuration.h): the C++ struct shape that `ecal.yaml` deserializes into.
- [`ecal/config/transport_layer.h`](https://github.com/eclipse-ecal/ecal/blob/v6.1.1/ecal/core/include/ecal/config/transport_layer.h): per-layer (SHM / UDP-MC / TCP) config keys and defaults.
- [`ecal/config/publisher.h`](https://github.com/eclipse-ecal/ecal/blob/v6.1.1/ecal/core/include/ecal/config/publisher.h) & [`subscriber.h`](https://github.com/eclipse-ecal/ecal/blob/v6.1.1/ecal/core/include/ecal/config/subscriber.h): per-endpoint overrides (layer enable, SHM zero-copy, ack timeouts).
- [SHM layer detail](https://eclipse-ecal.github.io/ecal/stable/advanced/layers/shm.html), [SHM zero-copy](https://eclipse-ecal.github.io/ecal/stable/advanced/layers/shm_zerocopy.html), [TCP](https://eclipse-ecal.github.io/ecal/stable/advanced/layers/tcp.html), [UDP-MC](https://eclipse-ecal.github.io/ecal/stable/advanced/layers/udp_mc.html): the per-layer mechanics and tuning knobs.

> The index above is not a complete API reference or tutorial — open the linked targets on demand. Pinned to eCAL `v6.1.1` and rustecal `main`; refresh on version bumps.
