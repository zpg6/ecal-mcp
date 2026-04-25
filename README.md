# ecal-mcp

A local **Model Context Protocol** server that exposes [Eclipse eCAL](https://github.com/eclipse-ecal/ecal) to LLM clients. Built in Rust on top of [`rustecal`](https://github.com/eclipse-ecal/rustecal) (eCAL bindings) and [`rmcp`](https://github.com/modelcontextprotocol/rust-sdk) (the official MCP SDK).

It is a **single native binary** that joins the eCAL network on the host like any other eCAL process. No daemons, no sidecars.

## Install

You need the **eCAL v6 runtime** already installed on the host (the same way every other eCAL participant on that machine has it). Grab the matching package from <https://github.com/eclipse-ecal/ecal/releases>:

- **Linux:** `apt-get install ./ecal_<ver>-jammy_<arch>.deb`
- **Windows:** run the `ecal_<ver>-win64.exe` installer (default install path `C:\eCAL`)

Then either grab a prebuilt `ecal-mcp` binary or build from source.

### Prebuilt binary (recommended)

Linux (x86_64 / aarch64):

```bash
curl -fsSL https://zpg6.github.io/ecal-mcp/install.sh | bash
```

Windows (PowerShell):

```powershell
irm https://zpg6.github.io/ecal-mcp/install.ps1 | iex
```

Each script detects your architecture, resolves the latest GitHub Release, verifies the `.sha256`, and installs `ecal-mcp` to a sensible default (`/usr/local/bin` on Linux, `%LOCALAPPDATA%\Programs\ecal-mcp\bin` on Windows — added to user `PATH`). Override with env vars:

```bash
ECAL_MCP_VERSION=v0.2.0 ECAL_MCP_PREFIX=$HOME/.local \
  curl -fsSL https://zpg6.github.io/ecal-mcp/install.sh | bash
```

```powershell
$env:ECAL_MCP_VERSION='v0.2.0'; $env:ECAL_MCP_PREFIX="$HOME\ecal-mcp"
irm https://zpg6.github.io/ecal-mcp/install.ps1 | iex
```

Released targets (each ships with a matching `.sha256`):

| Target | Archive |
|---|---|
| Linux x86_64 | `ecal-mcp-<version>-x86_64-unknown-linux-gnu.tar.gz` |
| Linux aarch64 | `ecal-mcp-<version>-aarch64-unknown-linux-gnu.tar.gz` |
| Windows x86_64 | `ecal-mcp-<version>-x86_64-pc-windows-msvc.zip` |

macOS is not yet covered by the release matrix — build from source there.

### Build from source

**Linux:**

```bash
sudo apt-get install -y clang libclang-14-dev llvm-dev protobuf-compiler
git clone https://github.com/zpg6/ecal-mcp && cd ecal-mcp
cargo build --release --bin ecal-mcp
./target/release/ecal-mcp
```

**Windows (PowerShell):** install Visual Studio 2019+ Build Tools, the [Rust toolchain](https://rustup.rs/), and `protoc` (e.g. `choco install protoc`). With eCAL installed at `C:\eCAL`:

```powershell
$env:ECAL_HOME = 'C:\eCAL'
git clone https://github.com/zpg6/ecal-mcp; cd ecal-mcp
cargo build --release --bin ecal-mcp
.\target\release\ecal-mcp.exe
```

## Wire it into an MCP client

Point your client at the binary. The path differs per OS:

**Linux** (Cursor / Claude Desktop config):

```json
{
  "mcpServers": {
    "ecal": {
      "command": "/usr/local/bin/ecal-mcp"
    }
  }
}
```

**Windows** (Cursor / Claude Desktop config — note the `.exe` and the doubled backslashes required by JSON):

```json
{
  "mcpServers": {
    "ecal": {
      "command": "C:\\Users\\<you>\\AppData\\Local\\Programs\\ecal-mcp\\bin\\ecal-mcp.exe"
    }
  }
}
```

(That's the default install path used by `install.ps1`. Substitute your `ECAL_MCP_PREFIX` if you overrode it. Forward slashes also work: `"C:/Users/<you>/AppData/Local/Programs/ecal-mcp/bin/ecal-mcp.exe"`.)

That's it. The server uses eCAL's normal registration/discovery transport — by default that's loopback UDP for local-only deployments and UDP multicast (`239.0.0.1`) once you switch to network mode in `ecal.yaml`. Topic data still flows over SHM (same host) or UDP/TCP (cross-host) like any other eCAL participant. If your other participants are working, this one will see them.

## Tools

| Tool | Purpose |
|---|---|
| `ecal_diagnose_topic` | **Start here when a topic isn't behaving.** Combines a monitoring snapshot (publishers, subscribers, type signatures, transport layers, SHM domains, process health) with a live measurement window and emits a `findings` list naming the actual anomaly: missing publisher / subscriber, metadata mismatch, no shared active transport, cross-host SHM-domain split, ongoing message drops, non-healthy producer process. When the topic doesn't exist at all, `similar_topics` lists the closest known names to catch typos. |
| `ecal_topic_stats` | Subscribe for `duration_ms` and report measured rate, payload size min/mean/max, inter-arrival gap min/mean/max, and gap stddev (jitter). The honest answer to "what's actually on the wire?" — independent of the cached `data_frequency` field. |
| `ecal_list_publishers` | Snapshot every publisher (topic, data type, transport layers, host, SHM domain, traffic stats, `data_id`/`data_clock`/`registration_clock`). Accepts `name_pattern` and `type_name_pattern` (case-insensitive substring) and `include_descriptors` (opt-in base-64 type descriptor blobs, off by default — they can be tens of KB each). |
| `ecal_list_subscribers` | Same shape and filtering as publishers. |
| `ecal_list_services` / `ecal_list_service_clients` | Service servers (with method signatures + call counts) and the clients calling them. Both accept `name_pattern`. |
| `ecal_list_processes` | Live eCAL processes with state, runtime version, and `config_file_path`. Accepts `name_pattern` against `unit_name` / `process_name`. |
| `ecal_get_logs` | Drain pending eCAL log messages with optional `min_level` (`debug4 < debug3 < debug2 < debug1 < info < warning < error < fatal`), `since_timestamp_us`, and `process_name_pattern` filters. |
| `ecal_publish` | One-shot publish (`text` for `StringMessage`, `payload_base64` for `BytesMessage`). Polls for ≥1 subscriber up to `discovery_wait_ms` so the first send actually lands. |
| `ecal_subscribe` | Listen on a topic for a fixed window and return up to N samples (UTF-8 + base-64, per-sample `topic_name`). |
| `ecal_call_service` | Call a method on every discovered server, returning each server's identity (`server_entity_id`, `server_process_id`, `server_host_name`) and response. Pass `target_server_entity_id` to drive exactly one replica; `discovered_instances` is preserved separately from filtered `instances`. |

All inputs and outputs use JSON Schemas generated by `schemars`, so MCP clients see fully typed tool definitions.

## Probing recipes

- **"Why is `/sensors/lidar` not arriving?"** → `ecal_diagnose_topic` with `topic: "/sensors/lidar"`. Read the `findings` array.
- **"What's actually publishing right now?"** → `ecal_list_publishers` with `name_pattern: "imu"` (or any substring).
- **"Find every protobuf topic of type `geometry_msgs.Twist`"** → `ecal_list_publishers` with `type_name_pattern: "Twist"` (case-insensitive substring; looser than `ecal_mon_cli --find`, which requires an exact type-name match).
- **"What rate is `/control/cmd_vel` running at?"** → `ecal_topic_stats` with `duration_ms: 3000`.
- **"Give me the protobuf descriptor for `/foo/bar`"** → `ecal_list_publishers` with `name_pattern: "/foo/bar"` and `include_descriptors: true`.
- **"Which process is calling my service?"** → `ecal_list_service_clients` with `name_pattern: "<service>"`.
- **"Show me only error logs since 5s ago"** → `ecal_get_logs` with `min_level: "error"` and `since_timestamp_us: <epoch_us - 5_000_000>`.
- **"Drive exactly replica X of my service"** → `ecal_call_service` with `target_server_entity_id: <id>` (the id is in any prior response's `server_entity_id` field).
- **"Send a synthetic sample to verify a subscriber exists"** → `ecal_publish` with `text:` or `payload_base64:`.

## Testing

The repo's Docker image and Python harness exist **only** to give the e2e suite a reproducible, eCAL-installed environment. You don't need them to run the server in production.

There are two tiers:

```bash
make image           # docker build -t ecal-mcp:e2e .
make e2e             # tier 1: 30 in-container cases (fast, hermetic)
make e2e-realnet     # tier 2: cross-container suite (real network mode + TCP)
```

### Tier 1 — `make e2e` (single container, SHM)

Starts a single container, runs two `ecal-test-publisher` processes (one slow, one fast) plus an `ecal-test-service-server` (with `echo` + `reverse` methods), then drives the MCP server over `docker exec -i` with raw NDJSON JSON-RPC. All 30 cases assert independently, and on any failure the last 300 lines of container logs are dumped. Set `ECAL_MCP_KEEP_CONTAINER=1` to keep the container after a run for poking.

### Tier 2 — `make e2e-realnet` (cross-container, network mode)

The single-container suite is fast and catches the bulk of regressions, but it never exercises three production code paths: **cross-host transport selection** (everyone uses SHM on a single host), **real protobuf type descriptors** (it only publishes `StringMessage`), and the **cross-host `shm_transport_domain` finding** in `ecal_diagnose_topic` (which by definition cannot fire when there's only one host).

The realnet suite fixes that. It uses `docker compose` to bring up two containers on a user-defined Docker bridge:

- `ecal-pub` runs `ecal-test-publisher` ("realnet" prefix, 10 Hz), `ecal-test-service-server`, and — if available in the eCAL deb — `ecal_sample_person_send` (a real protobuf "Person" topic with a real `FileDescriptorSet`).
- `ecal-mcp` runs `ecal-mcp` itself plus a *second* `ecal-test-publisher` ("realnet-mcp" prefix, 5 Hz) that publishes to the **same** topic. This gives the suite a multi-publisher cross-host topology — two publishers on one topic, one per host — which is what real production eCAL networks look like.

Both containers mount [`tests/realnet/ecal.yaml`](tests/realnet/ecal.yaml), which forces eCAL into **network mode** (UDP-multicast registration on `239.0.0.1`) and disables UDP/SHM on the subscriber side so cross-host data **must** land on TCP. Each container has a distinct hostname → distinct `shm_transport_domain` → the SHM-domain code path is actually reachable.

Assertions worth calling out:

- `ecal_list_processes` shows the remote `ecal-test-service-server` from MCP's snapshot (proves multicast registration crossed the bridge).
- A single `ecal_subscribe` returns samples from **both** publishers (interleaved `realnet ...` and `realnet-mcp ...` messages) with strictly-increasing per-publisher counters — proving cross-host data flow *and* same-host TCP data flow (subscriber.shm is disabled).
- `ecal_list_publishers` returns exactly two entries on the shared topic, with distinct `host_name` and distinct `topic_id` per publisher.
- `ecal_topic_stats` over the combined stream lands in `[5, 30] Hz` (10 Hz + 5 Hz nominal), with all size and gap fields populated and ordered (`min ≤ mean ≤ max`).
- `ecal_diagnose_topic` returns ≥2 publishers spanning *both* hostnames, ≥2 distinct `shm_transport_domain` values, the cross-host SHM-domain finding (**only** reachable in this topology), and **no** "data cannot flow" finding — and with `duration_ms > 0` on the person topic, returns a populated `live_stats` block.
- `ecal_call_service` responses carry `server_host_name = ecal-pub`, and the same call with `target_server_entity_id` routes to exactly that replica across the bridge while a bogus id matches zero.
- `ecal_list_service_clients` shows MCP's client-registration with `host_name = ecal-mcp` during a cross-host call — proving the inverse direction of registration plumbing also crosses the bridge.
- When the person sample is present, `ecal_list_publishers(name_pattern: "person", include_descriptors: true)` returns a descriptor blob whose first wire-format tag is `0x0a` (a real `FileDescriptorProto`/`FileDescriptorSet`) and contains both the upstream package literal `pb.People` and the message name `Person`.

Set `ECAL_MCP_KEEP_CONTAINERS=1` to leave the compose stack up after a run; logs from both containers are dumped automatically on failure.

## Layout

```
.
├── Cargo.toml               # crate definition, three binaries
├── src/
│   ├── main.rs              # ecal-mcp: rmcp ServerHandler + tool router
│   └── bin/
│       ├── test_publisher.rs        # ecal-test-publisher (e2e helper)
│       └── test_service_server.rs   # ecal-test-service-server (e2e helper)
├── Dockerfile               # only used by the e2e harness
├── Makefile                 # convenience targets
└── tests/
    ├── e2e.py               # tier 1: in-container e2e driver (30 test cases)
    ├── e2e_realnet.py       # tier 2: cross-container e2e driver (network mode)
    └── realnet/
        ├── docker-compose.yml   # two-container topology on a user-defined bridge
        └── ecal.yaml            # eCAL config: network mode + cross-host TCP
```

## Notes

- **stderr logging**: `tracing` writes to stderr only, so it never corrupts the MCP NDJSON framing on stdout.
- **Graceful shutdown**: `Ecal::finalize()` is always called on the way out, even if the MCP service fails or the client disconnects.
- **Type-safe schemas**: every tool uses a `serde::Deserialize + schemars::JsonSchema` argument struct; schemas are auto-generated and shipped to the client.
- **Monitoring is async**: eCAL refreshes registration on its own cadence (default 1000ms), so a process that started <1s ago may not appear in a snapshot yet.
