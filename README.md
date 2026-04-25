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
- **"What rate is `/control/cmd_vel` actually running at?"** → `ecal_topic_stats` with `duration_ms: 3000`. Independent of the cached `data_frequency` field.
- **"Find every protobuf topic of type `geometry_msgs.Twist`"** → `ecal_list_publishers` with `type_name_pattern: "Twist"` (case-insensitive substring; looser than `ecal_mon_cli --find`, which requires an exact type-name match).

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

## Testing

Two e2e tiers, both Docker-based:

```bash
make image           # docker build -t ecal-mcp:e2e .
make e2e             # tier 1: 30 in-container cases (single host, SHM)
make e2e-realnet     # tier 2: 15 cross-container cases (4-host topology, network mode + TCP)
```

The Docker image exists **only** for the e2e suite — you don't need it to run the server in production.

See **[`docs/TESTING.md`](docs/TESTING.md)** for the topology, what each tier covers, and the full list of assertions (cross-host transport negotiation, multi-publisher / multi-subscriber listing, mixed-role host attribution, real protobuf `FileDescriptorSet` round-tripping, etc.).
