# Testing

The repo's Docker image and Python harness exist **only** to give the e2e suite a reproducible, eCAL-installed environment. You don't need them to run the server in production.

There are two tiers:

```bash
make image           # docker build -t ecal-mcp:e2e .
make e2e             # tier 1: 30 in-container cases (fast, hermetic)
make e2e-realnet     # tier 2: 15 cross-container cases (real network mode + TCP)
```

## Tier 1 — `make e2e` (single container, SHM)

Starts a single container, runs two `ecal-test-publisher` processes (one slow, one fast) plus an `ecal-test-service-server` (with `echo` + `reverse` methods), then drives the MCP server over `docker exec -i` with raw NDJSON JSON-RPC. All 30 cases assert independently, and on any failure the last 300 lines of container logs are dumped. Set `ECAL_MCP_KEEP_CONTAINER=1` to keep the container after a run for poking.

This tier is fast and catches the bulk of regressions, but it never exercises three production code paths:

- **Cross-host transport selection** — everyone uses SHM on a single host.
- **Real protobuf type descriptors** — only `StringMessage` flies around.
- **Cross-host `shm_transport_domain` finding** in `ecal_diagnose_topic` — by definition cannot fire when there's only one host.

That's what tier 2 exists for.

## Tier 2 — `make e2e-realnet` (cross-container, network mode)

The realnet suite uses `docker compose` to bring up **four containers** on a user-defined Docker bridge, deliberately spread across every realistic placement variant — one mixed pub+sub host, one pure subscriber host, one pure service host, and a mixed MCP+pub host:

|     host      | roles on PUB_TOPIC and the service                            |
| ------------- | ------------------------------------------------------------- |
| `ecal-pub`    | publisher (`realnet` prefix, 10 Hz) + subscriber + person sender |
| `ecal-sub`    | subscriber-only                                               |
| `ecal-svc`    | `ecal-test-service-server` only — no pub/sub at all           |
| `ecal-mcp`    | `ecal-mcp` itself + publisher (`realnet-mcp` prefix, 5 Hz)    |

`PUB_TOPIC` therefore has **2 publishers across 2 hosts** and **2 dedicated remote subscribers across 2 hosts** (plus MCP's transient subs from `ecal_subscribe` / `ecal_diagnose_topic`), while the service lives on a host that shares no topic transport state with anyone.

All four containers mount [`tests/realnet/ecal.yaml`](../tests/realnet/ecal.yaml), which:

- sets `communication_mode: "network"` — the v6 switch that makes the registration-attribute builder use the **network** UDP group/TTL (default group `239.0.0.1`, [`ecal/core/src/registration/config/builder/registration_attribute_builder.cpp`](https://github.com/eclipse-ecal/ecal/blob/v6.1.1/ecal/core/src/registration/config/builder/registration_attribute_builder.cpp))
- leaves `registration.shm_transport_domain: ""`, so each host's domain falls back to `gethostname()` per [`ecal/core/src/ecal_process.cpp`](https://github.com/eclipse-ecal/ecal/blob/v6.1.1/ecal/core/src/ecal_process.cpp) (`GetShmTransportDomain` → `GetHostName`). With distinct compose `hostname:` values per service, this guarantees distinct SHM domains and unlocks the cross-host SHM-domain finding in `ecal_diagnose_topic`.
- enables `publisher.layer.{shm,udp,tcp}` but **disables** `subscriber.layer.{shm,udp}`, leaving subscribers TCP-only. Because eCAL negotiates the data layer as the *intersection* of publisher-enabled and subscriber-`read_enabled` layers, ordered by `priority_network` (`tcp, udp` here), cross-host data **must** land on TCP. See [`CPublisherImpl::DetermineTransportLayer2Start`](https://github.com/eclipse-ecal/ecal/blob/v6.1.1/ecal/core/src/pubsub/ecal_publisher_impl.cpp).

A single suite run therefore exercises four independent bridge hops:

| direction              | bridge mechanism                                           |
| ---------------------- | ---------------------------------------------------------- |
| `pub → mcp` data       | TCP, cross-host topic data                                 |
| `mcp → sub` data       | TCP, MCP's own publisher → remote subscriber               |
| `mcp → svc` RPC        | cross-host service call to a no-other-state host           |
| `pub, sub, svc → mcp`  | UDP-multicast registration on `239.0.0.1` (network group)  |

### Assertions worth calling out

- **`ecal_list_processes`** shows `ecal-test-service-server` from `ecal-svc` and (when present) `ecal_sample_person_send` from `ecal-pub` (proves multicast registration crossed the bridge from each non-MCP host). We deliberately don't gate on `ecal_test_publisher` or `ecal_test_subscriber` here — those unit_names run on two hosts each, and process-level snapshots can race on duplicate unit_names. Topic-level registration for the same processes is deterministic, and we pin those separately below.
- **Multi-publisher data flow** — a single `ecal_subscribe` returns samples from **both** publishers (interleaved `realnet ...` and `realnet-mcp ...` messages) with strictly-increasing per-publisher counters — proving cross-host data flow *and* same-host TCP data flow (subscriber.shm is disabled).
- **`ecal_list_publishers`** returns exactly two entries on the shared topic, with distinct `host_name` and distinct `topic_id` per publisher.
- **`ecal_list_subscribers`** returns ≥2 entries on the shared topic spanning **both** `ecal-pub` and `ecal-sub`. This is the only assertion in the suite that requires a *long-lived, non-MCP* subscriber to register cross-bridge — MCP's own ad-hoc subscribers (e.g. inside `ecal_subscribe` / `ecal_diagnose_topic`) are short-lived measurement windows and may not be present at the moment of the snapshot, so a stable remote subscriber is what proves the sub-direction registration plumbing actually crosses the bridge.
- **Mixed-role host attribution** — `ecal-pub` appears in BOTH the publisher and subscriber sets for `PUB_TOPIC` simultaneously (production-realistic placement), while `ecal-svc` appears in NEITHER. Catches set-by-host dedup bugs and role-bleed regressions in one shot.
- **`ecal_topic_stats`** over the combined stream lands in `[5, 30] Hz` (10 Hz + 5 Hz nominal), with all size and gap fields populated and ordered (`min ≤ mean ≤ max`).
- **`ecal_diagnose_topic`** returns ≥2 publishers spanning *both* publisher hostnames, ≥2 remote subscribers spanning *both* `ecal-pub` and `ecal-sub`, ≥2 distinct `shm_transport_domain` values, the cross-host SHM-domain finding (**only** reachable in this topology — N ≥ 2 hosts trips it), and **no** `data cannot flow` finding. With `duration_ms > 0` on the person topic, it also returns a populated `live_stats` block.
- **`ecal_call_service`** responses carry `service_host_name = ecal-svc` (a host with no other eCAL state on the MCP side — strongest possible "service routing is independent of topic routing" guarantee).
- **`target_service_id`** filters the response set to a single replica's reply (and a bogus id matches zero). Note: in the current Rust handler this is a *response-side* filter — `ecal-mcp` issues one `ClientInstance::call` per discovered replica and then drops non-matching responses. eCAL's `CClientInstance` is already per-server, so each replica receives exactly one call regardless. The behavioral guarantee is "exactly one matching response," not "only one network call."
- **`ecal_list_services`** reports the service uniquely on `ecal-svc` — a registration-level proof complementary to the per-call `service_host_name` assertion.
- **`ecal_list_service_clients`** shows MCP's client-registration with `host_name = ecal-mcp` during a cross-host call — proving the inverse direction of registration plumbing also crosses the bridge.
- **Real protobuf descriptor** — when the person sample is present, `ecal_list_publishers(name_pattern: "person", include_descriptors: true)` returns a descriptor blob whose first wire-format tag is `0x0a` (a real `FileDescriptorProto`/`FileDescriptorSet`) and contains both the upstream package literal `pb.People` and the message name `Person`.

Set `ECAL_MCP_KEEP_CONTAINERS=1` to leave the compose stack up after a run; logs from all containers are dumped automatically on failure.

## Environment caveats

The realnet suite assumes the host's Docker bridge correctly forwards UDP multicast between containers on the same user-defined network. eCAL handles its end of the contract (`SO_REUSEADDR` on receivers, multicast-join on the bound interface, configurable TTL — `network.ttl: 1` is sufficient for a single L2 segment), but **IGMP snooping**, restrictive firewall policy, or unusual `userland-proxy` setups can still drop multicast traffic. If `make e2e-realnet` consistently fails at "cross-host process visibility," that's the first thing to investigate.

## Why two tiers, not one

Tier 1 catches schema regressions, JSON-RPC framing bugs, tool-routing bugs, and same-host SHM-path regressions cheaply. Tier 2 catches everything that only manifests across a real network bridge: cross-host transport negotiation, multicast registration plumbing, host-aware service routing, real protobuf `FileDescriptorSet` round-tripping, and `ecal_diagnose_topic`'s cross-host findings. Both tiers run in CI on every push and PR to `main` (tier 1 first, then tier 2); tier 2 is heavier because the Docker image build dominates, but it is no longer gated behind on-demand invocation.
