//! Local MCP server that exposes Eclipse eCAL pub/sub & service introspection
//! and interaction over stdio, built on top of `rustecal` and `rmcp`.
//!
//! Field naming follows upstream eCAL closely (`SDataTypeInformation`,
//! `STransportLayer`, `SServer.service_id`, `STopic.direction`, …) so an
//! agent grepping eCAL headers/docs and grepping our JSON payloads finds
//! the same names. Where we depart from upstream we say so in field-level
//! docs (e.g. `registered_data_frequency_hz` is the `data_frequency` mHz
//! value converted to Hz).

use std::borrow::Cow;
use std::collections::HashMap;
use std::ffi::CStr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use clap::Parser;
use parking_lot::Mutex;
use rmcp::{
    handler::server::wrapper::{Json, Parameters},
    model::{Implementation, ServerCapabilities, ServerInfo as RmcpServerInfo},
    schemars, tool, tool_handler, tool_router, ErrorData as McpError, ServerHandler, ServiceExt,
};
use rustecal::pubsub::publisher::Timestamp;
use rustecal::{
    Ecal, EcalComponents, PublisherMessage, ServiceClient, ServiceRequest, TypedPublisher,
    TypedSubscriber,
};
use rustecal_core::core_types::monitoring::{TransportLayer, TransportLayerType};
use rustecal_core::log_level::LogLevel;
use rustecal_core::types::DataTypeInfo;
use rustecal_types_bytes::BytesMessage;
use rustecal_types_string::StringMessage;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tracing_subscriber::EnvFilter;

mod cli;

// ---------------------------------------------------------------------------
// Common shapes (named after their upstream eCAL counterparts)
// ---------------------------------------------------------------------------

/// Mirrors `eCAL::SDataTypeInformation` (header: `ecal/core/include/ecal/types.h`).
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
struct DataTypeInformation {
    /// Type identifier as advertised by the producer, e.g. `"proto:foo.Bar"`, `"string"`, `""`
    /// (untyped). Equivalent to `SDataTypeInformation.name`.
    name: String,
    /// Encoding hint, e.g. `"proto"`, `"utf-8"`, `""`. Equivalent to
    /// `SDataTypeInformation.encoding`.
    encoding: String,
    /// Length of the type descriptor blob (e.g. a serialized `FileDescriptorSet` for protobuf
    /// topics). Always present, cheap.
    descriptor_len: usize,
    /// First 16 hex chars of SHA-256 over the descriptor bytes, computed server-side. Two endpoints
    /// whose `name` and `encoding` agree but whose `descriptor_fingerprint` differs are running
    /// incompatible schema versions of the same logical type — the equality semantics upstream
    /// `SDataTypeInformation::operator==` enforces (`/tmp/ecal/ecal/core/include/ecal/types.h`).
    /// `None` when `descriptor_len == 0`. The 16-hex prefix is collision-safe for any realistic
    /// schema set while keeping the field cheap on the wire.
    #[serde(skip_serializing_if = "Option::is_none")]
    descriptor_fingerprint: Option<String>,
    /// Descriptor blob itself, base-64 encoded. Only included when `include_descriptors=true` is
    /// passed to the listing tool, since descriptors can be tens of KB per topic.
    #[serde(skip_serializing_if = "Option::is_none")]
    descriptor_base64: Option<String>,
}

fn datatype_information(info: DataTypeInfo, include_descriptors: bool) -> DataTypeInformation {
    let descriptor_len = info.descriptor.len();
    let descriptor_fingerprint = if descriptor_len > 0 {
        let mut hasher = Sha256::new();
        hasher.update(&info.descriptor);
        let digest = hasher.finalize();
        Some(hex_prefix(&digest, 16))
    } else {
        None
    };
    let descriptor_base64 = if include_descriptors && descriptor_len > 0 {
        Some(B64.encode(&info.descriptor))
    } else {
        None
    };
    DataTypeInformation {
        name: info.type_name,
        encoding: info.encoding,
        descriptor_len,
        descriptor_fingerprint,
        descriptor_base64,
    }
}

fn hex_prefix(bytes: &[u8], hex_chars: usize) -> String {
    let needed = (hex_chars + 1) / 2;
    let mut s = String::with_capacity(hex_chars);
    for b in bytes.iter().take(needed) {
        s.push_str(&format!("{:02x}", b));
    }
    s.truncate(hex_chars);
    s
}

/// Mirrors `eCAL::Monitoring::STransportLayer` (header:
/// `ecal/core/include/ecal/types/monitoring.h`). Each topic has a *vector* of these — one per
/// transport kind that has been negotiated for the topic. `active=false` means the kind is
/// configured but no sample has moved on it yet (very common in v6 with no matched peer; see
/// `direction` field-doc for the v6 footgun).
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
struct TransportLayerEntry {
    /// One of `"udp_mc"`, `"shm"`, `"tcp"`, `"none"`, or `"unknown:<n>"` for unrecognized values.
    /// Maps to upstream `STransportLayer.type` (an `eTransportLayerType` enum,
    /// `monitoring.h:60-66`). The JSON key is `type` (matching upstream); the Rust field name is
    /// `layer_type` only because `type` is a Rust keyword.
    #[serde(rename = "type")]
    layer_type: String,
    /// Protocol version the layer negotiated. Useful for spotting v0/v1 mismatches across hosts.
    /// Maps to `STransportLayer.version`.
    version: i32,
    /// `true` once eCAL has actually moved a sample on this layer. **eCAL v6 footgun:**
    /// `CPublisher::Send` short-circuits with `GetSubscriberCount() == 0` and returns *before*
    /// invoking `Write`, so a healthy publisher with no matched subscriber will keep every layer at
    /// `active=false` indefinitely. Symmetric on the subscriber side until the first sample
    /// arrives. **Do not** read `active=false` as "transport broken"; cross-check with `data_clock`
    /// (advances on every `Send`) for liveness.
    active: bool,
}

fn transport_kind(t: &TransportLayerType) -> String {
    match t {
        TransportLayerType::None => "none".into(),
        TransportLayerType::UdpMulticast => "udp_mc".into(),
        TransportLayerType::Shm => "shm".into(),
        TransportLayerType::Tcp => "tcp".into(),
        TransportLayerType::Unknown(n) => format!("unknown:{n}"),
    }
}

fn transport_layer_entry(l: &TransportLayer) -> TransportLayerEntry {
    TransportLayerEntry {
        layer_type: transport_kind(&l.transport_type),
        version: l.version,
        active: l.active,
    }
}

/// Side discriminator on a `TopicEntry`. Mirrors `STopic.direction` (string `"publisher"` /
/// `"subscriber"`) but typed for ergonomics.
#[derive(Debug, Clone, Copy, Serialize, schemars::JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum Direction {
    Publisher,
    Subscriber,
}

/// One monitored publisher or subscriber registration. Mirrors `eCAL::Monitoring::STopic`
/// (`ecal/core/include/ecal/types/monitoring.h`) and rustecal's `TopicInfo`
/// (`rustecal-core/src/core_types/monitoring.rs`). Same row shape on both sides — discriminate via
/// `direction`.
// TODO(v8): rustecal exposes message_drops/data_frequency but not
// data_latency_us; patch upstream to bind STopic.data_latency_us as
// SStatistics{count,min,max,mean,variance} and surface it on TopicEntry +
// live_stats.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
struct TopicEntry {
    topic_name: String,
    /// `"publisher"` or `"subscriber"`. Same field as upstream `STopic.direction` — kept on every
    /// entry so a reader who copies one row out of the listing (e.g. into `findings[].detail`)
    /// doesn't lose the side label.
    direction: Direction,
    /// Type/encoding/descriptor advertised by this endpoint. On a **subscriber** this is the
    /// *expected* type declared at construction, not a peer-derived value — useful when reporting
    /// `no_publisher`. Field name matches upstream `STopic.datatype_information`.
    datatype_information: DataTypeInformation,
    /// `gethostname()` on the producing process — typically the short hostname, not an FQDN. Used
    /// together with `process_id` to identify the producing process across the network.
    host_name: String,
    /// eCAL "shm transport domain" — defaults to `host_name`. Only matters for **cross-host-name**
    /// endpoints (e.g. two containers presenting different hostnames on the same Linux host): in
    /// that case both must agree on this string for SHM transport to be eligible. Same-host /
    /// same-hostname endpoints share SHM regardless of this value.
    shm_transport_domain: String,
    process_id: i32,
    /// Producing process's executable path. On Linux this is the **full path** (read from
    /// `/proc/self/exe` by `eCAL::Process::GetProcessName`), not just the basename.
    process_name: String,
    /// User-supplied "unit name" passed to `Ecal::initialize` on the producing process (the
    /// application/component name, *not* a host or thread name). Falls back to the process basename
    /// when not set.
    unit_name: String,
    /// `eCAL_SEntityId.entity_id` of this publisher / subscriber endpoint (a per-process 64-bit id;
    /// layout is a rustecal/MCP-side implementation detail, not an upstream guarantee). **Unique
    /// within a process and the network for that process's lifetime, but NOT stable across process
    /// restarts.** Don't persist it — re-discover after every restart.
    topic_id: i64,
    /// Number of **same-host** counterpart endpoints the publisher has an active connection to.
    /// **Only populated on publisher rows**: eCAL explicitly sets both `connections_local` and
    /// `connections_external` to `0` on subscriber registrations (the subscriber side does not know
    /// how many publishers are connected to it).
    connections_local: i32,
    /// Number of **cross-host** counterpart endpoints the publisher is connected to. Same caveat as
    /// `connections_local`: always `0` on subscriber rows by design.
    connections_external: i32,
    /// Subscriber-side accumulator: count of missing samples detected by comparing successive
    /// `send_clock` values on incoming messages. **Effectively always 0 on publisher rows** (the
    /// publisher side doesn't fill this field in registration).
    message_drops: i32,
    /// Cached samples-per-second rate, derived from the upstream eCAL `STopic.data_frequency` field
    /// which is **stored in millihertz as `int32`** (`monitoring.h:105`). We divide by 1000 at the
    /// boundary so callers don't have to remember the unit. This is the **publisher's self-reported
    /// registration value**, refreshed on the eCAL `registration_refresh` cadence (default 1000 ms)
    /// — it is **not** the live wire rate. For wire measurements call `ecal_topic_stats` (which
    /// returns `wire_data_frequency_hz`).
    registered_data_frequency_hz: f64,
    /// eCAL `data_clock` — monotonic send/receive action counter. On a publisher this advances on
    /// every `Send` (including the no-subscriber path, where eCAL still calls
    /// `RefreshSendCounter`); on a subscriber it advances per accepted sample. Diffing this across
    /// two snapshots is the cheap liveness signal.
    data_clock: i64,
    /// eCAL monitoring's local counter for this entity: the monitoring layer increments it every
    /// time it ingests a registration update for the topic (`CMonitoringImpl::RegisterTopic` does
    /// `topic.registration_clock++`). It is *not* sent over the wire by the producer, so don't read
    /// it as "heartbeats since the producer started" — read it as "how many registration cycles
    /// this monitor has seen for that entity". Still useful as a freshness signal: flat across
    /// snapshots ⇒ no recent registration update ⇒ the producer may have died (entries persist
    /// until `registration_timeout`, default 10s).
    registration_clock: i32,
    /// All transport layers eCAL has negotiated for this endpoint. Mirrors upstream
    /// `STopic.transport_layer` (vector of `STransportLayer{type, version, active}`) — singular
    /// field name, plural contents, byte-for-byte with the C++ struct. Filter by `active=true` to
    /// see what's currently flowing; see the `active` field-doc on `TransportLayerEntry` for the v6
    /// zero-subscriber footgun.
    transport_layer: Vec<TransportLayerEntry>,
}

/// One method on a service server / client. Mirrors `eCAL::Monitoring::SMethod`
/// (`ecal/core/include/ecal/types/monitoring.h`) and rustecal's `MethodInfo`.
#[derive(Debug, Serialize, schemars::JsonSchema)]
struct MethodEntry {
    /// Method name (`SMethod.method_name`). Pair with `service_name` on the parent entry to drive a
    /// call via `ecal_call_service`.
    method_name: String,
    /// Full `SDataTypeInformation` for the request side, mirroring upstream
    /// `SMethod.request_datatype_information` (`monitoring.h:149-150` / `service/types.h:118`).
    /// Same shape as `TopicEntry.datatype_information` so callers can use one piece of vocabulary
    /// across pub/sub and services. When the server didn't advertise a type (e.g. rustecal's raw
    /// `add_method`), `name` and `encoding` are empty and `descriptor_len == 0`;
    /// `descriptor_base64` honors `include_descriptors=true` on the listing tool.
    request_datatype_information: DataTypeInformation,
    response_datatype_information: DataTypeInformation,
    /// Per-method counter incremented **in-process** on each handled request (server) or issued
    /// call (client) since *process start* — **not** a cluster-wide total. Resets when the owning
    /// process restarts. Rediscovery handshakes can also bump it. Useful as a per-replica
    /// fingerprint: diff two `ecal_list_services` snapshots to attribute traffic to a specific
    /// server `service_id`, but don't read it as a cluster-wide call count.
    call_count: i64,
}

/// One service-server registration (one replica). Mirrors `eCAL::Monitoring::SServer`
/// (`ecal/core/include/ecal/types/monitoring.h`) and rustecal's `ServerInfo`. Multiple
/// `ServiceEntry` rows for the same `service_name` are independent replicas; disambiguate via
/// `service_id`.
#[derive(Debug, Serialize, schemars::JsonSchema)]
struct ServiceEntry {
    service_name: String,
    /// `eCAL_SEntityId.entity_id` of this server registration, named `service_id` to match upstream
    /// `SServer.service_id` (the holding `SEntityId` is itself called `service_id` upstream;
    /// `monitoring.h:164`). **This is the value `target_service_id` on `ecal_call_service`
    /// consumes** — pass it verbatim to scope a call to one specific replica. Per-process, NOT
    /// stable across server restarts; don't persist.
    service_id: i64,
    host_name: String,
    process_id: i32,
    process_name: String,
    unit_name: String,
    /// eCAL service protocol version (`SServer.version`). Useful for spotting v0/v1 protocol
    /// mismatches across replicas.
    version: u32,
    /// Server's TCP port for the legacy v0 protocol (0 when not bound). Servers may bind only v0,
    /// only v1, or both; a `0` here on a v1-only server simply means the legacy port is not in use.
    tcp_port_v0: u32,
    /// Server's TCP port for the v1 protocol (0 when not bound).
    tcp_port_v1: u32,
    methods: Vec<MethodEntry>,
}

/// One service-client registration (a process holding a `ServiceClient` for `service_name`).
/// Mirrors `eCAL::Monitoring::SClient` (`ecal/core/include/ecal/types/monitoring.h`) / rustecal
/// `ClientInfo`. Clients deregister shortly after their last call, so this view is
/// timing-sensitive.
#[derive(Debug, Serialize, schemars::JsonSchema)]
struct ClientEntry {
    service_name: String,
    /// `eCAL_SEntityId.entity_id` of this *client* registration. Named `service_id` to match
    /// upstream `SClient.service_id`; per-process, not stable across restarts.
    service_id: i64,
    host_name: String,
    process_id: i32,
    process_name: String,
    unit_name: String,
    /// eCAL service protocol version negotiated by this client.
    version: u32,
    methods: Vec<MethodEntry>,
}

/// Response of `ecal_list_service_clients`. Same row shape as the `clients` field of
/// `MonitoringResult`.
#[derive(Debug, Serialize, schemars::JsonSchema)]
struct ServiceClientsResult {
    clients: Vec<ClientEntry>,
}

/// One log row. Mirrors `eCAL::Logging::SLogMessage` (`ecal/core/include/ecal/types/logging.h`) /
/// rustecal `LogMessage`, drained from the in-process eCAL logging buffer by `Log::get_logging()`
/// (no remote/historical lookup).
#[derive(Debug, Serialize, schemars::JsonSchema)]
struct LogEntry {
    /// Severity level (e.g. "Info", "Warning", "Error", "Fatal", "Debug1"…).
    level: String,
    /// Microseconds since epoch.
    timestamp_us: i64,
    /// Host the logging process is running on.
    host_name: String,
    /// OS-level process name / executable path of the emitting process.
    process_name: String,
    /// OS process id of the emitting process.
    process_id: i32,
    /// eCAL log unit name (upstream `eCAL_Logging_SLogMessage.unit_name` / C++
    /// `SLogMessage::unit_name`). Populated from rustecal's `LogMessage.thread_name`, which is the
    /// same C field — the rustecal Rust name is historical and a comment in rustecal flags a
    /// planned rename.
    unit_name: String,
    /// Raw message text as logged via `eCAL::Logging::Log(...)`.
    content: String,
}

/// Response of `ecal_get_logs`: drained log buffer for *this* process, after the optional
/// `min_level` / `since_timestamp_us` / `process_name_pattern` filters.
#[derive(Debug, Serialize, schemars::JsonSchema)]
struct LogsResult {
    logs: Vec<LogEntry>,
}

/// Arguments for `ecal_get_logs`.
#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
struct GetLogsArgs {
    /// Minimum severity to include, ordered verbose → fatal: `debug4 < debug3 < debug2 < debug1 <
    /// info < warning < error < fatal`. Defaults to `info` when omitted (i.e. drop debug noise).
    /// Pass `all` (or `none`) to include debug entries — `since_timestamp_us` is purely a time
    /// filter and does not widen severity.
    #[serde(default)]
    min_level: Option<String>,
    /// Drop entries with `timestamp_us` strictly less than this. Useful for "only show me logs
    /// since I started watching".
    #[serde(default)]
    since_timestamp_us: Option<i64>,
    /// Optional case-insensitive substring filter on the emitting process name.
    #[serde(default)]
    process_name_pattern: Option<String>,
}

/// Severity *priority* used for `min_level` filtering. eCAL's numeric
/// `eLogLevel` values are bit flags
/// (`Info=1, Warning=2, Error=4, Fatal=8, Debug1=16, Debug2=32, Debug3=64,
/// Debug4=128`) which do **not** form a usable `>=` ordering (e.g. Debug1
/// > Fatal numerically), so we project them onto an ordinal scale here
/// (verbose at the bottom, fatal at the top):
///
/// `debug4=1, debug3=2, debug2=3, debug1=4, info=5, warning=6, error=7, fatal=8`
///
/// `min_level="info"` therefore yields "info | warning | error | fatal"
/// (i.e. drop debug noise), `min_level="warning"` drops info, etc. Anything
/// we don't recognise sorts as `0` so the filter is effectively disabled.
fn level_priority(level: LogLevel) -> i32 {
    match level {
        LogLevel::Debug4 => 1,
        LogLevel::Debug3 => 2,
        LogLevel::Debug2 => 3,
        LogLevel::Debug1 => 4,
        LogLevel::Info => 5,
        LogLevel::Warning => 6,
        LogLevel::Error => 7,
        LogLevel::Fatal => 8,
        LogLevel::None | LogLevel::All => 0,
    }
}

fn parse_min_level(label: &str) -> i32 {
    match label.to_ascii_lowercase().as_str() {
        "debug4" => 1,
        "debug3" => 2,
        "debug2" => 3,
        "debug1" => 4,
        "info" => 5,
        "warning" | "warn" => 6,
        "error" | "err" => 7,
        "fatal" => 8,
        "all" | "none" => 0,
        _ => 0,
    }
}

// ---------------------------------------------------------------------------
// Topic stats / diagnose
// ---------------------------------------------------------------------------

/// Arguments for `ecal_topic_stats`. Drives a live measurement window (transient subscriber,
/// inter-arrival math) plus the same optional `expected_hz` / `tolerance_pct` spec contract used by
/// `ecal_diagnose_topic`.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct TopicStatsArgs {
    topic_name: String,
    /// Window in milliseconds. Defaults to 2000. Hard-capped at 60000.
    #[schemars(range(max = 60000))]
    #[serde(default)]
    duration_ms: Option<u64>,
    /// Optional rate target. When set, the response includes `meets_spec`, `delta_hz`
    /// (`wire_data_frequency_hz - expected_hz`), `delta_direction`, and `ratio_to_target` so the
    /// caller doesn't have to do the arithmetic. `meets_spec=true` iff the measured **wire data
    /// frequency** is within `tolerance_pct` of the target. `wire_data_frequency_hz = 1e6 /
    /// mean_gap_us`.
    #[serde(default)]
    expected_hz: Option<f64>,
    /// Fractional tolerance for `meets_spec` (e.g. `0.10` ⇒ ±10%). Defaults to `0.10` when
    /// `expected_hz` is supplied. Ignored otherwise. Tighter values (e.g. `0.02`) are useful for
    /// steady heartbeats; looser for bursty streams. Echoed back in the response as `tolerance_pct`
    /// so reports are self-describing.
    #[serde(default)]
    tolerance_pct: Option<f64>,
}

/// Sign-explicit direction enum echoed alongside `delta_hz` so callers don't need to read the
/// schema doc to know what a negative number means. `on_spec` is emitted iff `meets_spec=true`.
#[derive(Debug, Clone, Copy, Serialize, schemars::JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum DeltaDirection {
    OnSpec,
    BelowSpec,
    AboveSpec,
}

/// Live window statistics from `ecal_topic_stats`, also embedded as
/// `DiagnoseTopicResult.live_stats` when `ecal_diagnose_topic` runs a measurement window.
/// Spec-related fields (`delta_*`, `meets_spec`, `ratio_to_target`, `tolerance_pct`) are produced
/// by `compute_spec_check`. `publishers[]` is filled only when `≥ 2` publishers are registered for
/// the topic name (`collect_echoed_publishers`).
#[derive(Debug, Serialize, schemars::JsonSchema)]
struct TopicStatsResult {
    /// Echo of the caller's requested topic. Useful when this struct is nested as `live_stats`
    /// inside a diagnose response.
    topic_name: String,
    /// Actual measurement window length, in milliseconds. `0` when the caller of `ecal_topic_stats`
    /// passed `duration_ms=0` (no sleep). `ecal_diagnose_topic` represents an auto-skipped live
    /// step as `live_stats: null`, NOT as a populated `TopicStatsResult` with `duration_ms=0`.
    duration_ms: u64,
    /// Total samples received during the measurement window. Zero is valid (no publisher emitted
    /// within the window).
    samples_observed: u64,
    /// Inter-arrival-derived rate: `1e6 / mean_gap_us`. This is the cadence the publisher is
    /// actually emitting at, independent of where the measurement window cut off. Omitted when
    /// fewer than 2 samples were observed.
    #[serde(skip_serializing_if = "Option::is_none")]
    wire_data_frequency_hz: Option<f64>,
    /// Sum of payload bytes observed in the window (excludes eCAL framing).
    total_bytes: u64,
    /// Min / mean / max payload size in bytes over the window. Omitted when no samples were
    /// observed.
    #[serde(skip_serializing_if = "Option::is_none")]
    min_size_bytes: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    mean_size_bytes: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_size_bytes: Option<usize>,
    /// Inter-arrival gap (microseconds) between consecutive samples. Requires at least 2 samples;
    /// otherwise omitted.
    #[serde(skip_serializing_if = "Option::is_none")]
    min_gap_us: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    mean_gap_us: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_gap_us: Option<i64>,
    /// Standard deviation of the gaps. Useful as a "is this stream jittery?" signal. Requires at
    /// least 2 samples.
    #[serde(skip_serializing_if = "Option::is_none")]
    gap_stddev_us: Option<f64>,
    /// `gap_stddev_us / mean_gap_us` — fractional jitter (e.g. `0.002` = 0.2%). Rule of thumb: `<
    /// 0.002` ⇒ steady, `> 0.05` ⇒ bursty.
    #[serde(skip_serializing_if = "Option::is_none")]
    jitter_pct: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    first_timestamp_us: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    last_timestamp_us: Option<i64>,
    /// Type/encoding observed on the wire (from the publisher's side). May differ from the
    /// monitoring snapshot if the publisher is mid-restart.
    #[serde(skip_serializing_if = "Option::is_none")]
    type_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    encoding: Option<String>,
    /// Echo of the caller's `expected_hz` when present. Omitted otherwise.
    #[serde(skip_serializing_if = "Option::is_none")]
    expected_hz: Option<f64>,
    /// `wire_data_frequency_hz - expected_hz`. Positive ⇒ topic is faster than spec, negative ⇒
    /// slower. See `delta_direction` for a self-describing label. Only present when both inputs
    /// were resolvable.
    #[serde(skip_serializing_if = "Option::is_none")]
    delta_hz: Option<f64>,
    /// Self-describing label: `on_spec` (within tolerance), `below_spec` (slower than target), or
    /// `above_spec` (faster than target). Only present when `expected_hz` was supplied AND we have
    /// a `wire_data_frequency_hz`.
    #[serde(skip_serializing_if = "Option::is_none")]
    delta_direction: Option<DeltaDirection>,
    /// `wire_data_frequency_hz / expected_hz`. Useful as a multiplicative ratio (`0.02` ≈ "running
    /// at 2% of target"; `1.0` = on target).
    #[serde(skip_serializing_if = "Option::is_none")]
    ratio_to_target: Option<f64>,
    /// `true` iff `|delta_hz| / expected_hz <= tolerance_pct`. Only
    /// present when `expected_hz` was supplied AND we have a
    /// `wire_data_frequency_hz`.
    #[serde(skip_serializing_if = "Option::is_none")]
    meets_spec: Option<bool>,
    /// Echo of the tolerance the caller used (or the `0.10` default) when `expected_hz` was
    /// supplied. Lets reports be self-describing without the consumer having to know the default.
    /// Omitted when `expected_hz` was not supplied.
    #[serde(skip_serializing_if = "Option::is_none")]
    tolerance_pct: Option<f64>,
    /// Echoed only when **two or more** publishers are registered for this topic, so the
    /// rogue-publisher case is visible from `ecal_topic_stats` alone (without a separate
    /// `ecal_list_publishers` call). Each row carries enough to attribute the contributor:
    /// `{host_name, unit_name, process_name, topic_id, registered_data_frequency_hz}`. Empty
    /// omitted.
    #[serde(skip_serializing_if = "Option::is_none")]
    publishers: Option<Vec<EchoedPublisher>>,
}

/// One publisher row echoed alongside `TopicStatsResult.publishers` when more than one publisher is
/// registered for a topic. Carries just enough monitoring-snapshot data to attribute the
/// contributor without a separate `ecal_list_publishers` call.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
struct EchoedPublisher {
    /// Host the publishing process is running on (upstream `STopic.host_name`).
    host_name: String,
    /// `STopic.unit_name` echoed from the publisher registration row (same field as
    /// `TopicEntry.unit_name`).
    unit_name: String,
    /// `STopic.process_name` echoed from the publisher registration row (same field as
    /// `TopicEntry.process_name`).
    process_name: String,
    /// Per-publisher entity id (`STopic.topic_id`, surfaced as `i64` through the C ABI; underlying
    /// `EntityIdT` is `uint64_t`).
    topic_id: i64,
    /// Cached registration rate from `STopic.data_frequency` (mHz upstream, converted to Hz here).
    /// Not the live wire rate.
    registered_data_frequency_hz: f64,
}

/// Arguments for `ecal_diagnose_topic`. Drives the full negative-existence
/// + transport + rate diagnostic for one topic name (need not exist).
#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct DiagnoseTopicArgs {
    /// eCAL topic to diagnose. Need not exist — if no publishers or subscribers are registered for
    /// this name, you'll get a `no_publishers_or_subscribers` finding plus a `did_you_mean` finding
    /// listing the closest existing topic names.
    topic_name: String,
    /// How long to sample the topic for live rate/stats, in milliseconds. Defaults to 1500.
    /// Hard-capped at 60000. Set to 0 to skip the live step. **The live step is auto-skipped
    /// (returns `live_stats: null`) when no endpoints are registered**, regardless of this value.
    #[schemars(range(max = 60000))]
    #[serde(default)]
    duration_ms: Option<u64>,
    /// Optional rate target. When supplied, the diagnose call will emit `findings[rate_below_spec]`
    /// or `findings[rate_above_spec]` whenever `live_stats.wire_data_frequency_hz` falls outside
    /// `tolerance_pct` of this target. Mirrors `ecal_topic_stats.expected_hz` so the same rate-spec
    /// contract works without a second tool call.
    #[serde(default)]
    expected_hz: Option<f64>,
    /// Fractional tolerance for the rate-spec check above. Defaults to `0.10` when `expected_hz` is
    /// supplied. Ignored otherwise.
    #[serde(default)]
    tolerance_pct: Option<f64>,
}

/// Stable, branchable codes emitted on `DiagnoseTopicResult.findings[].code`. Agents should switch
/// on `code`; humans read `message`.
#[derive(Debug, Clone, Copy, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
enum FindingCode {
    /// Zero publishers and zero subscribers registered for the requested topic. Named after eCAL's
    /// own vocabulary (`SMonitoring.publishers` / `.subscribers`).
    NoPublishersOrSubscribers,
    /// Subscriber(s) exist but no publisher is registered.
    NoPublisher,
    /// Publisher(s) exist but no subscriber is registered.
    NoSubscriber,
    /// More than one `(name, encoding)` signature across endpoints — raw bytes still flow but typed
    /// deserialization will fail.
    TypeMetadataMismatch,
    /// Both sides are registered with active layers, but their active sets don't intersect — data
    /// cannot flow.
    NoCommonTransport,
    /// Multiple distinct `shm_transport_domain` values across multiple hosts on this topic, so SHM
    /// is ineligible for at least one pair. Suppressed when a non-empty `common_active_transport`
    /// already covers the topic, or when every endpoint's `shm_transport_domain` equals its
    /// `host_name` (the v8 default — same-host pairs share SHM regardless of cross-host domain
    /// splits).
    ShmDomainSplit,
    /// At least one subscriber reports `message_drops > 0`. `detail.message_drops` is the
    /// upstream cumulative counter; `drop_rate_per_sec` / `drops_since_last_snapshot` /
    /// `window_seconds` are deltas against an in-process cache (60 s TTL, keyed by
    /// `(topic_id, subscriber_topic_id)`) and are absent on the first call after process spawn —
    /// persistent rate tracking requires `ecal-mcp serve`, not the per-call CLI shell-out.
    MessageDrops,
    /// At least one endpoint's owning process reports `state_severity >= 2`
    /// (warning/critical/failed).
    UnhealthyProcess,
    /// Live measurement window observed zero samples despite registered publishers — usually means
    /// the publisher hasn't sent yet, or the measurement subscriber didn't agree on a transport in
    /// time.
    NoSamplesObserved,
    /// Always emitted on `no_publishers_or_subscribers`, `no_publisher`, and `no_subscriber` to
    /// support typo recovery; `detail.candidates` may be `[]` and `detail.suggestion` may be `null`
    /// when no near-miss was found in the monitoring snapshot.
    DidYouMean,
    /// Two or more publishers registered for a topic that also has at least one subscriber — the
    /// "1:N producers on a 1:1 topic" anti-pattern. MCP-coined finding (no upstream prior art).
    MultiplePublishers,
    /// Live `wire_data_frequency_hz` is below `expected_hz` by more than `tolerance_pct`.
    /// MCP-coined; mirrors `ecal_topic_stats` semantics.
    RateBelowSpec,
    /// Live `wire_data_frequency_hz` is above `expected_hz` by more than `tolerance_pct`.
    RateAboveSpec,
}

/// Triage severity emitted on every `Finding` so the most actionable item sorts first. MCP-coined;
/// not an upstream concept.
#[derive(Debug, Clone, Copy, Serialize, schemars::JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum FindingSeverity {
    Info,
    Warning,
    Error,
}

fn severity_for(code: FindingCode) -> FindingSeverity {
    use FindingCode::*;
    use FindingSeverity::*;
    match code {
        NoPublisher
        | NoSubscriber
        | NoPublishersOrSubscribers
        | TypeMetadataMismatch
        | NoCommonTransport
        | UnhealthyProcess => Error,
        MessageDrops | MultiplePublishers | RateBelowSpec | RateAboveSpec | NoSamplesObserved
        | ShmDomainSplit => Warning,
        DidYouMean => Info,
    }
}

/// Sort key: `error > warning > info`, used as a tiebreaker only.
fn severity_rank(s: FindingSeverity) -> u8 {
    match s {
        FindingSeverity::Error => 2,
        FindingSeverity::Warning => 1,
        FindingSeverity::Info => 0,
    }
}

/// Likely-root-cause ordering, lowest = "look here first". Drives `findings[]` sort with
/// `severity_rank` as the tiebreaker, so a load-bearing `warning` (e.g. `multiple_publishers`)
/// sorts above a lower-priority advisory regardless of severity. Stable across releases — switch
/// on `code`, sort for UX.
fn code_rank(code: FindingCode) -> u8 {
    use FindingCode::*;
    match code {
        NoPublishersOrSubscribers => 0,
        NoPublisher => 1,
        NoSubscriber => 2,
        TypeMetadataMismatch => 3,
        NoCommonTransport => 4,
        UnhealthyProcess => 5,
        MultiplePublishers => 6,
        MessageDrops => 7,
        RateBelowSpec => 8,
        RateAboveSpec => 9,
        NoSamplesObserved => 10,
        ShmDomainSplit => 11,
        DidYouMean => 12,
    }
}

/// One row in `DiagnoseTopicResult.findings`. Branch on `code`; render `message` and `detail` for
/// humans. The list is sorted by `code_rank` (likely root cause first) with `severity_rank` as a
/// tiebreaker — see `code_rank` / `severity_rank` for the order.
#[derive(Debug, Serialize, schemars::JsonSchema)]
struct Finding {
    /// Stable machine-readable code; switch on this.
    code: FindingCode,
    /// Triage severity: `error` / `warning` / `info`. MCP-coined; not an upstream concept. See
    /// struct-level doc for the (non-severity- first) sort order.
    severity: FindingSeverity,
    /// Human-readable description of the same finding. Subject to rewording — do NOT pattern-match
    /// this.
    message: String,
    /// Optional structured supporting data (topic-name suggestion, drop count, severity numbers,
    /// etc.). **The key set is code-specific — branch on `code` before reading keys.** See the
    /// finding catalog and "Field-level gotchas" sections in `AGENT_SKILL.md`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    detail: Option<serde_json::Value>,
}

/// Aggregated `ecal_diagnose_topic` view of one topic name: the matching pub/sub `TopicEntry` rows,
/// the `(name, encoding)` signatures across them, the cross-endpoint transport intersections, the
/// `shm_transport_domain` set, the sorted `findings`, and an optional `live_stats` window from
/// `measure_topic`.
#[derive(Debug, Serialize, schemars::JsonSchema)]
struct DiagnoseTopicResult {
    /// Echo of the caller's requested topic.
    topic_name: String,
    /// Every publisher registration whose `topic_name` equals `topic_name` above. Empty list ⇒ no
    /// publisher is registered (see `findings[no_publisher]`).
    publishers: Vec<TopicEntry>,
    /// Mirror of `publishers` for the subscriber direction.
    subscribers: Vec<TopicEntry>,
    /// Distinct `(name, encoding)` tuples seen across publishers AND subscribers. If this has more
    /// than one entry, you have a type mismatch — the canonical reason for "subscriber sees
    /// nothing".
    type_signatures: Vec<TypeSignature>,
    /// Active transport layer kinds shared by *all* publishers and *all* subscribers on this topic.
    /// Empty while both sides exist with active layers ⇒ no layer is mutually agreed and data won't
    /// flow.
    common_active_transport_layers: Vec<String>,
    /// Distinct `shm_transport_domain` values seen across pubs+subs. More than one means at least
    /// some endpoints can't share SHM.
    shm_transport_domains: Vec<String>,
    /// Detected anomalies. Branch on `code`; sort order is documented on `Finding`. Empty list ⇒ no
    /// obvious issues.
    findings: Vec<Finding>,
    /// Live measurement window. **Literal JSON `null`** when no endpoints are registered for this
    /// topic (skipped to avoid a wasted subscribe), or when the caller passed `duration_ms=0`. The
    /// key is always present so the response stays self-describing — agents can tell "auto-skipped"
    /// from "missing field" at a glance. When samples were observed, only the populated measurement
    /// fields are present (no wall of nulls).
    live_stats: Option<TopicStatsResult>,
}

/// One distinct `(name, encoding)` signature across endpoints on one topic, with
/// publisher/subscriber occurrence counts. More than one row in
/// `DiagnoseTopicResult.type_signatures` ⇒ `type_metadata_mismatch`.
#[derive(Debug, Serialize, schemars::JsonSchema)]
struct TypeSignature {
    name: String,
    encoding: String,
    /// Number of **publishers** that advertised this signature.
    publisher_occurrences: usize,
    /// Number of **subscribers** that advertised this signature. A signature with
    /// `publisher_occurrences > 0 && subscriber_occurrences > 0` is one that both sides agree on;
    /// mismatch ⇒ data won't bind.
    subscriber_occurrences: usize,
    /// `topic_id` of every publisher that advertised this signature, sorted ascending for stable
    /// output. Lets a consumer attribute a row in `type_signatures[]` back to a specific entity in
    /// `publishers[]` without recomputing the join.
    publisher_topic_ids: Vec<i64>,
    /// `topic_id` of every subscriber that advertised this signature, sorted ascending.
    subscriber_topic_ids: Vec<i64>,
}

/// Response of `ecal_list_publishers`: filtered publisher rows from one monitoring snapshot.
#[derive(Debug, Serialize, schemars::JsonSchema)]
struct PublishersResult {
    /// Publisher registrations matching the caller's filter (or all of them when no filter was
    /// supplied). One row per `(topic, host, process_id, topic_id)` tuple.
    publishers: Vec<TopicEntry>,
}

/// Response of `ecal_list_subscribers`: filtered subscriber rows from one monitoring snapshot.
#[derive(Debug, Serialize, schemars::JsonSchema)]
struct SubscribersResult {
    /// Subscriber registrations; same shape as `PublishersResult.publishers`, one row per
    /// registered subscriber. `direction == "subscriber"`.
    subscribers: Vec<TopicEntry>,
}

/// Response of `ecal_list_services`: filtered service-server rows from one monitoring snapshot,
/// including each server's `methods[]`.
#[derive(Debug, Serialize, schemars::JsonSchema)]
struct ServicesResult {
    /// Server registrations; one row per `(service_name, host, process_id, service_id)`.
    /// `methods[]` carries each method on the server.
    services: Vec<ServiceEntry>,
}

/// Response of `ecal_list_processes`: process rows from one monitoring snapshot (mirrors
/// `SMonitoring.processes`).
#[derive(Debug, Serialize, schemars::JsonSchema)]
struct ProcessesResult {
    /// One row per eCAL process (`SMonitoring.processes`). Includes processes that don't currently
    /// have any pubs/subs/services.
    processes: Vec<ProcessEntry>,
}

/// One eCAL process row. Mirrors `eCAL::Monitoring::SProcess`
/// (`ecal/core/include/ecal/types/monitoring.h`) / rustecal `ProcessInfo`.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
struct ProcessEntry {
    host_name: String,
    process_id: i32,
    process_name: String,
    unit_name: String,
    /// eCAL `eProcessSeverity` (the **health** enum): `0=unknown, 1=healthy, 2=warning, 3=critical,
    /// 4=failed`. Treat `>= 2` (NOT `> 0` — `1` is *healthy*) as non-healthy. `0` means the process
    /// never called `Process::SetState` (common right after startup; default registration is
    /// `(severity=0, level=1)`).
    state_severity: i32,
    /// eCAL `eProcessSeverityLevel` (a **separate** finer-grained level, `0=unknown,
    /// 1..=5=level1..level5`). Independent of `state_severity` — they are distinct enums in
    /// `process.proto`, written together by `Process::SetState(severity, level, info)`. The default
    /// registration for a process that never calls `SetState` is `level=1` *with* `severity=0` ⇒
    /// `level=1` does NOT imply healthy; only `severity` does.
    state_severity_level: i32,
    state_info: String,
    runtime_version: String,
    /// Path to the `ecal.yaml` this process loaded (per its registration). Empty when the process
    /// started with the built-in defaults.
    config_file_path: String,
}

// ---------------------------------------------------------------------------
// Hosts & aggregated monitoring
// ---------------------------------------------------------------------------

/// Per-host rollup derived from a monitoring snapshot. **MCP-only — not a field of upstream
/// `SMonitoring`.** Aggregates `shm_transport_domain` values, `runtime_version` set, and
/// pub/sub/server/client/process counts per `host_name` so agents can diff hosts at a glance.
#[derive(Debug, Serialize, schemars::JsonSchema)]
struct HostEntry {
    host_name: String,
    /// Distinct `shm_transport_domain` values seen on this host across
    /// publishers/subscribers/processes. Two domains here ⇒ this host has processes that can't
    /// share SHM with each other; agents debugging "why doesn't SHM work between these two
    /// containers on the same box?" should look here first.
    shm_transport_domains: Vec<String>,
    /// Set-union of `runtime_version` values from `processes` on this host. More than one entry ⇒
    /// heterogeneous eCAL builds on the same box, usually intentional but a useful sanity check.
    runtime_versions: Vec<String>,
    process_count: usize,
    publisher_count: usize,
    subscriber_count: usize,
    server_count: usize,
    client_count: usize,
}

/// Response of `ecal_list_hosts`: MCP-derived per-host rollup. See `HostEntry`.
#[derive(Debug, Serialize, schemars::JsonSchema)]
struct HostsResult {
    /// One row per `host_name` observed across the full monitoring snapshot, with per-direction
    /// counts and SHM-domain rollup.
    hosts: Vec<HostEntry>,
}

/// Response of `ecal_get_monitoring`: one coherent monitoring snapshot shaped after
/// `eCAL::Monitoring::SMonitoring` (publishers, subscribers, servers, clients, processes — keys
/// match upstream) plus the MCP-derived `hosts[]` rollup.
#[derive(Debug, Serialize, schemars::JsonSchema)]
struct MonitoringResult {
    /// Active publisher registrations; same row shape as `ecal_list_publishers`.
    publishers: Vec<TopicEntry>,
    /// Active subscriber registrations; same row shape as `ecal_list_subscribers`.
    subscribers: Vec<TopicEntry>,
    /// Service *servers* on the bus, named to match upstream `SMonitoring.servers`. Note the tool
    /// `ecal_list_services` keeps `services` on its own response (it lists service instances,
    /// plural OK there); only this aggregated snapshot uses the upstream-faithful `servers` key.
    servers: Vec<ServiceEntry>,
    /// Active service-client registrations; same row shape as `ecal_list_service_clients`.
    clients: Vec<ClientEntry>,
    /// All eCAL processes (`SMonitoring.processes`); same row shape as `ecal_list_processes`.
    processes: Vec<ProcessEntry>,
    /// MCP-derived per-host rollup (not a field of upstream `SMonitoring`). Same row shape as
    /// `ecal_list_hosts`.
    hosts: Vec<HostEntry>,
}

/// Arguments for `ecal_get_monitoring`.
#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
struct GetMonitoringArgs {
    /// When true, include base-64 type descriptor blobs in `datatype_information` on
    /// publishers/subscribers AND in each `methods[*]` request/response datatype info on
    /// servers/clients (same scope as the `include_descriptors` flag on the per-tool `ecal_list_*`
    /// listings; ignored on `processes`/`hosts` since neither carries types). Off by default —
    /// descriptors can be tens of KB per topic.
    #[serde(default)]
    include_descriptors: Option<bool>,
}

// ---------------------------------------------------------------------------
// Publish / subscribe / service-call args & results
// ---------------------------------------------------------------------------

/// Arguments for `ecal_publish`. Wire mode (`StringMessage` vs `BytesMessage`) is selected by the
/// payload field set: `text` ⇒ string topic, `payload_base64` ⇒ bytes topic; the two are mutually
/// exclusive.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct PublishArgs {
    topic_name: String,
    /// UTF-8 text payload. Mutually exclusive with `payload_base64`.
    #[serde(default)]
    text: Option<String>,
    /// Base-64 encoded raw bytes payload. Mutually exclusive with `text`.
    #[serde(default)]
    payload_base64: Option<String>,
    /// Milliseconds to wait *after* creating the publisher for at least one subscriber to be
    /// **actively connected** (`get_subscriber_count > 0`, polled every 20 ms) before the first
    /// send. Defaults to 500. Returns as soon as a subscriber is seen. This polls active
    /// connections, not bare registration — a subscriber that has registered but not yet completed
    /// handshake will not satisfy the wait.
    #[serde(default)]
    discovery_wait_ms: Option<u64>,
    /// Number of times to send the message (useful when subscribers may be slow to discover the
    /// publisher). Defaults to 1.
    #[serde(default)]
    repeat: Option<u32>,
    /// Delay in milliseconds between repeated sends. Defaults to 100.
    #[serde(default)]
    repeat_interval_ms: Option<u64>,
}

/// Result of `ecal_publish`: topic echo, total bytes sent, and per-attempt `Send` accounting.
#[derive(Debug, Serialize, schemars::JsonSchema)]
struct PublishResult {
    /// Echo of the published topic.
    topic_name: String,
    /// Size of the **single** payload in bytes (UTF-8 length for `text`, raw byte length for
    /// `payload_base64`). Each `Send` writes this same payload, so total wire bytes are `bytes_sent
    /// * sends_succeeded` (excluding eCAL framing).
    bytes_sent: usize,
    /// Number of `Publisher::Send` calls that returned `true` (C return `0`, i.e. eCAL actually
    /// wrote to at least one transport). Note that `Send` can still return `false` **even with
    /// `subscriber_count > 0`** — for example when every transport layer's writer reports a failed
    /// write (`CPublisherImpl::Write` returns `false`). The no-subscribers-yet path returns `false`
    /// as well but only calls `RefreshSendCounter()` (no bytes on the wire).
    sends_succeeded: u32,
    /// Number of `Publisher::Send` calls issued (= `repeat`, default 1).
    sends_attempted: u32,
}

/// Arguments for `ecal_subscribe`. The handler always sleeps the full `duration_ms` (it does NOT
/// short-circuit when `samples_collected == max_samples`); arrivals beyond the cap stop being added
/// to `samples[]` and bump `samples_truncated_at_cap` instead.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct SubscribeArgs {
    topic_name: String,
    /// How long to listen, in milliseconds. Defaults to 1000. Bounded only by what the caller
    /// supplies; the listen window always runs to completion regardless of `max_samples`.
    #[serde(default)]
    duration_ms: Option<u64>,
    /// Maximum samples retained in `samples[]`. Defaults to 10. Excess arrivals during the window
    /// are counted via `SubscribeResult.samples_truncated_at_cap`, NOT dropped silently.
    #[serde(default)]
    max_samples: Option<usize>,
}

/// One received sample inside `SubscribeResult.samples`. Combines the raw payload (decoded as text
/// when valid UTF-8, otherwise base-64) with eCAL wire metadata: send timestamp, send clock, and
/// the publisher's advertised type/encoding.
#[derive(Debug, Serialize, schemars::JsonSchema)]
struct Sample {
    /// Topic name carried on the wire sample (`eCAL_STopicId.topic_name`). Under normal usage this
    /// is identical to the subscription topic; it is **not** a per-publisher discriminator —
    /// multiple publishers on the same topic emit the same string, and you'd disambiguate them via
    /// the publisher's `eCAL_SEntityId` (not surfaced on `Sample`).
    topic_name: String,
    /// Publisher's send timestamp in **microseconds since epoch**, as stamped by the publisher
    /// process at `Send` time (auto-stamped via `eCAL::Time::GetMicroSeconds()` unless the caller
    /// passed an explicit time). This is *not* a subscriber receive time.
    timestamp_us: i64,
    /// `eCAL_SReceiveCallbackData.send_clock` — the publisher's per-message counter at send time,
    /// increased by exactly one on each successful `Send`. Subscribers use it for drop detection
    /// (`message_drops`). Not the same as the subscriber's internal `m_clock`.
    clock: i64,
    /// Payload size in bytes.
    size: usize,
    /// UTF-8 decoded payload, present only when the bytes are valid UTF-8.
    #[serde(skip_serializing_if = "Option::is_none")]
    text: Option<String>,
    /// Base-64 encoded payload bytes. Suppressed when `text` was set and the payload is short and
    /// printable (≤ `INLINE_TEXT_BYTES`); set otherwise so binary payloads are always recoverable.
    #[serde(skip_serializing_if = "Option::is_none")]
    payload_base64: Option<String>,
    /// Wire-side `SDataTypeInformation.encoding` (e.g. `"proto"`, `"utf-8"`, `""`).
    encoding: String,
    /// Wire-side `SDataTypeInformation.name`.
    name: String,
}

/// Result of `ecal_subscribe`: listen-window echo, sample-cap accounting, and the collected
/// `Sample` rows.
#[derive(Debug, Serialize, schemars::JsonSchema)]
struct SubscribeResult {
    /// Echo of the subscribed topic.
    topic_name: String,
    /// Actual time the subscriber listened, in milliseconds.
    duration_ms: u64,
    /// Number of samples in `samples[]` (≤ `max_samples`).
    samples_collected: usize,
    /// Count of samples that arrived during the window but were truncated because
    /// `samples_collected` had already reached `max_samples`. Distinct from `STopic.message_drops`:
    /// this is an MCP-layer cap on the response payload, not a transport- or callback-level loss.
    /// Treat as "raise `max_samples` if you need more headroom", *not* as a network problem.
    samples_truncated_at_cap: usize,
    samples: Vec<Sample>,
}

/// Arguments for `ecal_call_service`. Optional discovery wait, per-call timeout, and an optional
/// single-replica filter via `target_service_id`.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct CallServiceArgs {
    service: String,
    method: String,
    /// UTF-8 text payload. Mutually exclusive with `payload_base64`.
    #[serde(default)]
    text: Option<String>,
    /// Base-64 encoded raw bytes payload. Mutually exclusive with `text`.
    #[serde(default)]
    payload_base64: Option<String>,
    /// Per-call timeout in milliseconds. Defaults to 2000.
    #[serde(default)]
    timeout_ms: Option<i32>,
    /// Milliseconds to wait for at least one server instance to be discovered before issuing the
    /// call. Defaults to 1000.
    #[serde(default)]
    discovery_wait_ms: Option<u64>,
    /// When set, only call the server instance whose
    /// `SServer.service_id` (`eCAL_SEntityId.entity_id`) matches this
    /// value. Useful for driving exactly one of N replicas. If no
    /// instance matches the filter, the call returns no responses
    /// rather than an error; the requested id is echoed back as
    /// `requested_target_service_id` **only when at least one replica
    /// was discovered** (so you can distinguish "filter rejected every
    /// replica" from "service is gone" — the latter returns
    /// `requested_target_service_id: null` and `discovered_instances == 0`).
    ///
    /// **Do not persist this value.** eCAL service ids are per-process
    /// (a server restart produces a fresh id). Re-discover via
    /// `ecal_list_services` after every restart.
    #[serde(default)]
    target_service_id: Option<u64>,
}

/// One row in `CallServiceResult.responses`: a single server replica's answer (or failure) for one
/// `ecal_call_service` attempt. `success` is set iff rustecal reported `CallState::Executed`;
/// failed and timed-out replies arrive here with `success=false` and (usually) a populated `error`
/// from the C error buffer.
#[derive(Debug, Serialize, schemars::JsonSchema)]
struct ServiceCallResponse {
    /// Index into the parent `responses[]`, **not** into the discovered fleet. With
    /// `target_service_id` set this will always be `0`.
    instance_index: usize,
    /// `true` iff the rustecal-side `CallState` is `Executed` (`ServiceResponse::success`). `false`
    /// with a non-empty `error` is the most common "method exists but the server returned a failure
    /// status" case; `false` with empty `error` typically means timeout or transport failure.
    success: bool,
    /// eCAL-side failure reason (timeout, non-success status, transport error). Omitted on the
    /// happy path. Always paired with `success: false`.
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    /// Identity of the server instance that produced this response. Populated from
    /// `eCAL_SServiceResponse::server_id` (= the server's `SServer.service_id`); same value you
    /// pass as `target_service_id`.
    service_id: u64,
    service_process_id: i32,
    service_host_name: String,
    response_size: usize,
    /// UTF-8 text decode of the response payload. Always preferred when the bytes are valid UTF-8.
    #[serde(skip_serializing_if = "Option::is_none")]
    response_text: Option<String>,
    /// Base-64 encoded response bytes. Suppressed when `response_text` is set and the payload is
    /// short (≤ `INLINE_TEXT_BYTES`); always set for binary or large responses so callers never
    /// lose data.
    #[serde(skip_serializing_if = "Option::is_none")]
    response_base64: Option<String>,
}

/// Aggregate result of `ecal_call_service`: discovery counts, optional echo of the caller's
/// `target_service_id`, and one row per replica that answered (or failed).
#[derive(Debug, Serialize, schemars::JsonSchema)]
struct CallServiceResult {
    /// Echo of the request `service`.
    service: String,
    /// Echo of the request `method`.
    method: String,
    /// Total number of server instances eCAL discovered for this service before any
    /// `target_service_id` filtering. `len(responses)` is the post-filter count; the difference
    /// between the two is how many replicas were rejected by the target filter.
    discovered_instances: usize,
    /// Echo of the caller's `target_service_id`. Present only when the caller supplied one **and**
    /// `discovered_instances > 0`; otherwise the key is **omitted from JSON** (deserializes as
    /// `None` / `null` in typical clients). This lets a reader distinguish `len(responses) == 0`
    /// due to "service is gone" (key absent, `discovered_instances == 0`) from `len(responses) ==
    /// 0` due to "filter rejected every replica" (key echoes the requested id,
    /// `discovered_instances >= 1`).
    #[serde(skip_serializing_if = "Option::is_none")]
    requested_target_service_id: Option<u64>,
    responses: Vec<ServiceCallResponse>,
}

// ---------------------------------------------------------------------------
// MCP server
// ---------------------------------------------------------------------------

/// Per-(topic_id, subscriber_topic_id) record of the previous `(message_drops, observed_at)`
/// snapshot, used to compute drop *rates* in `findings[message_drops]`. Pruned on every diagnose
/// call: any entry whose `observed_at` is older than `DROP_CACHE_TTL` is dropped before the new
/// snapshot is recorded.
type DropsCache = Arc<Mutex<HashMap<(i64, i64), (i32, Instant)>>>;
const DROP_CACHE_TTL: Duration = Duration::from_secs(60);

#[derive(Clone, Default)]
struct EcalServer {
    drops_cache: DropsCache,
}

fn snapshot() -> Result<rustecal_core::core_types::monitoring::MonitoringSnapshot, McpError> {
    rustecal_core::monitoring::Monitoring::get_snapshot().map_err(|e| {
        McpError::internal_error(format!("eCAL monitoring snapshot failed: {e:?}"), None)
    })
}

fn topic_entry(
    t: rustecal_core::core_types::monitoring::TopicInfo,
    direction: Direction,
    include_descriptors: bool,
) -> TopicEntry {
    let transport_layer: Vec<TransportLayerEntry> = t
        .transport_layers
        .iter()
        .map(transport_layer_entry)
        .collect();
    TopicEntry {
        topic_name: t.topic_name,
        direction,
        datatype_information: datatype_information(t.data_type, include_descriptors),
        host_name: t.host_name,
        shm_transport_domain: t.shm_transport_domain,
        process_id: t.process_id,
        process_name: t.process_name,
        unit_name: t.unit_name,
        topic_id: t.topic_id,
        connections_local: t.connections_local,
        connections_external: t.connections_external,
        message_drops: t.message_drops,
        registered_data_frequency_hz: f64::from(t.data_frequency) / 1000.0,
        data_clock: t.data_clock,
        registration_clock: t.registration_clock,
        transport_layer,
    }
}

/// Default fractional tolerance for `meets_spec` when the caller does not pass `tolerance_pct`.
/// ±10% is a reasonable starting point: tight enough to flag genuine drift on steady heartbeats,
/// loose enough not to false-positive on jittery streams.
const DEFAULT_TOLERANCE_PCT: f64 = 0.10;

/// Maximum response payload size (in bytes) for which we'll *suppress* the base-64 form when a
/// UTF-8 text decode is present. Above this we emit both forms so the caller can always recover the
/// exact bytes.
const INLINE_TEXT_BYTES: usize = 256;

fn measure_topic(
    topic: &str,
    duration_ms: u64,
    expected_hz: Option<f64>,
    tolerance_pct: Option<f64>,
) -> Result<TopicStatsResult, String> {
    #[derive(Default)]
    struct Acc {
        timestamps: Vec<i64>,
        sizes: Vec<usize>,
        type_name: Option<String>,
        encoding: Option<String>,
    }
    let acc: Arc<Mutex<Acc>> = Arc::new(Mutex::new(Acc::default()));

    let mut sub = TypedSubscriber::<BytesMessage>::new(topic)
        .map_err(|e| format!("create subscriber: {e}"))?;
    {
        let acc = acc.clone();
        sub.set_callback(move |received| {
            let mut g = acc.lock();
            g.timestamps.push(received.timestamp);
            g.sizes.push(received.payload.data.as_ref().len());
            if g.type_name.is_none() {
                g.type_name = Some(received.type_name);
                g.encoding = Some(received.encoding);
            }
        });
    }
    std::thread::sleep(Duration::from_millis(duration_ms));
    drop(sub);

    let g = std::mem::take(&mut *acc.lock());
    let n = g.timestamps.len() as u64;
    let total_bytes: u64 = g.sizes.iter().map(|s| *s as u64).sum();

    let (min_size_bytes, mean_size_bytes, max_size_bytes) = if g.sizes.is_empty() {
        (None, None, None)
    } else {
        let min = *g.sizes.iter().min().unwrap();
        let max = *g.sizes.iter().max().unwrap();
        let mean = total_bytes as f64 / n as f64;
        (Some(min), Some(mean), Some(max))
    };

    let (min_gap_us, mean_gap_us, max_gap_us, gap_stddev_us) = if g.timestamps.len() < 2 {
        (None, None, None, None)
    } else {
        // Sort by send-timestamp first so multi-publisher reordering
        // (samples from different publishers can arrive out of order in
        // wall-clock terms) doesn't produce negative gaps.
        let mut sorted_ts = g.timestamps.clone();
        sorted_ts.sort_unstable();
        let mut gaps: Vec<i64> = sorted_ts.windows(2).map(|w| w[1] - w[0]).collect();
        gaps.sort();
        let min = *gaps.first().unwrap();
        let max = *gaps.last().unwrap();
        let mean = gaps.iter().map(|g| *g as f64).sum::<f64>() / gaps.len() as f64;
        let var = gaps
            .iter()
            .map(|g| {
                let d = *g as f64 - mean;
                d * d
            })
            .sum::<f64>()
            / gaps.len() as f64;
        (Some(min), Some(mean), Some(max), Some(var.sqrt()))
    };

    let wire_data_frequency_hz = mean_gap_us.and_then(|m| if m > 0.0 { Some(1.0e6 / m) } else { None });
    let jitter_pct = match (gap_stddev_us, mean_gap_us) {
        (Some(sd), Some(m)) if m > 0.0 => Some(sd / m),
        _ => None,
    };

    let resolved_tolerance = expected_hz.map(|_| tolerance_pct.unwrap_or(DEFAULT_TOLERANCE_PCT));
    let (delta_hz, ratio_to_target, meets_spec, delta_direction) =
        compute_spec_check(expected_hz, wire_data_frequency_hz, tolerance_pct);

    Ok(TopicStatsResult {
        topic_name: topic.to_string(),
        duration_ms,
        samples_observed: n,
        wire_data_frequency_hz,
        total_bytes,
        min_size_bytes,
        mean_size_bytes,
        max_size_bytes,
        min_gap_us,
        mean_gap_us,
        max_gap_us,
        gap_stddev_us,
        jitter_pct,
        first_timestamp_us: g.timestamps.iter().min().copied(),
        last_timestamp_us: g.timestamps.iter().max().copied(),
        type_name: g.type_name,
        encoding: g.encoding,
        expected_hz,
        delta_hz,
        delta_direction,
        ratio_to_target,
        meets_spec,
        tolerance_pct: resolved_tolerance,
        publishers: None,
    })
}

fn compute_spec_check(
    expected_hz: Option<f64>,
    wire_data_frequency_hz: Option<f64>,
    tolerance_pct: Option<f64>,
) -> (
    Option<f64>,
    Option<f64>,
    Option<bool>,
    Option<DeltaDirection>,
) {
    match (expected_hz, wire_data_frequency_hz) {
        (Some(target), Some(actual)) if target > 0.0 => {
            let tol = tolerance_pct.unwrap_or(DEFAULT_TOLERANCE_PCT);
            let delta = actual - target;
            let ratio = actual / target;
            let meets = (delta.abs() / target) <= tol;
            let dir = if meets {
                DeltaDirection::OnSpec
            } else if delta < 0.0 {
                DeltaDirection::BelowSpec
            } else {
                DeltaDirection::AboveSpec
            };
            (Some(delta), Some(ratio), Some(meets), Some(dir))
        }
        _ => (None, None, None, None),
    }
}

/// Set-intersection of *active* transport layer kinds across all entries
/// on each side. Returns `(intersection, both_sides_advertised)`.
///
/// `both_sides_advertised` is `true` only when at least one publisher
/// **and** at least one subscriber advertised a non-empty active layer
/// set; eCAL sometimes leaves subscriber `transport_layer` all
/// `active=false` in monitoring snapshots (no sample received yet), in
/// which case "no shared transport" is not a real finding.
fn intersect_active_transports(pubs: &[TopicEntry], subs: &[TopicEntry]) -> (Vec<String>, bool) {
    fn merge(side: &[TopicEntry]) -> std::collections::BTreeSet<String> {
        side.iter()
            .flat_map(|e| {
                e.transport_layer
                    .iter()
                    .filter(|l| l.active)
                    .map(|l| l.layer_type.clone())
            })
            .collect()
    }
    let p = merge(pubs);
    let s = merge(subs);
    let advertised = !p.is_empty() && !s.is_empty();
    let common: Vec<String> = p.intersection(&s).cloned().collect();
    (common, advertised)
}

fn process_entry(p: rustecal_core::core_types::monitoring::ProcessInfo) -> ProcessEntry {
    ProcessEntry {
        host_name: p.host_name,
        process_id: p.process_id,
        process_name: p.process_name,
        unit_name: p.unit_name,
        state_severity: p.state_severity,
        state_severity_level: p.state_severity_level,
        state_info: p.state_info,
        runtime_version: p.runtime_version,
        config_file_path: p.config_file_path,
    }
}

fn service_entry(
    s: rustecal_core::core_types::monitoring::ServerInfo,
    include_descriptors: bool,
) -> ServiceEntry {
    ServiceEntry {
        service_name: s.service_name,
        service_id: s.service_id,
        host_name: s.host_name,
        process_id: s.process_id,
        process_name: s.process_name,
        unit_name: s.unit_name,
        version: s.version,
        tcp_port_v0: s.tcp_port_v0,
        tcp_port_v1: s.tcp_port_v1,
        methods: s
            .methods
            .into_iter()
            .map(|m| method_entry(m, include_descriptors))
            .collect(),
    }
}

fn client_entry(
    c: rustecal_core::core_types::monitoring::ClientInfo,
    include_descriptors: bool,
) -> ClientEntry {
    ClientEntry {
        service_name: c.service_name,
        service_id: c.service_id,
        host_name: c.host_name,
        process_id: c.process_id,
        process_name: c.process_name,
        unit_name: c.unit_name,
        version: c.version,
        methods: c
            .methods
            .into_iter()
            .map(|m| method_entry(m, include_descriptors))
            .collect(),
    }
}

fn method_entry(
    m: rustecal_core::core_types::monitoring::MethodInfo,
    include_descriptors: bool,
) -> MethodEntry {
    MethodEntry {
        method_name: m.method_name,
        request_datatype_information: datatype_information(m.request_type, include_descriptors),
        response_datatype_information: datatype_information(m.response_type, include_descriptors),
        call_count: m.call_count,
    }
}

/// Aggregate per-host counts and metadata over the full monitoring snapshot. Mirrors the way
/// upstream's GUI Monitor surfaces a "Hosts" view (`doc/rst/getting_started/monitor.rst`).
fn build_hosts(
    pubs: &[TopicEntry],
    subs: &[TopicEntry],
    services: &[ServiceEntry],
    clients: &[ClientEntry],
    processes: &[ProcessEntry],
    raw_processes: &[rustecal_core::core_types::monitoring::ProcessInfo],
) -> Vec<HostEntry> {
    use std::collections::BTreeMap;
    let mut by_host: BTreeMap<String, HostEntry> = BTreeMap::new();
    fn touch<'a>(m: &'a mut BTreeMap<String, HostEntry>, host: &str) -> &'a mut HostEntry {
        m.entry(host.to_string()).or_insert_with(|| HostEntry {
            host_name: host.to_string(),
            shm_transport_domains: Vec::new(),
            runtime_versions: Vec::new(),
            process_count: 0,
            publisher_count: 0,
            subscriber_count: 0,
            server_count: 0,
            client_count: 0,
        })
    }

    for e in pubs {
        let h = touch(&mut by_host, &e.host_name);
        h.publisher_count += 1;
        if !h.shm_transport_domains.contains(&e.shm_transport_domain) {
            h.shm_transport_domains.push(e.shm_transport_domain.clone());
        }
    }
    for e in subs {
        let h = touch(&mut by_host, &e.host_name);
        h.subscriber_count += 1;
        if !h.shm_transport_domains.contains(&e.shm_transport_domain) {
            h.shm_transport_domains.push(e.shm_transport_domain.clone());
        }
    }
    for e in services {
        touch(&mut by_host, &e.host_name).server_count += 1;
    }
    for e in clients {
        touch(&mut by_host, &e.host_name).client_count += 1;
    }
    for p in processes {
        let h = touch(&mut by_host, &p.host_name);
        h.process_count += 1;
        if !p.runtime_version.is_empty() && !h.runtime_versions.contains(&p.runtime_version) {
            h.runtime_versions.push(p.runtime_version.clone());
        }
    }
    // Pull `shm_transport_domain` off the raw ProcessInfo too — it's a
    // per-process eCAL field even though our public `ProcessEntry` doesn't
    // surface it directly. This catches hosts that have processes with no
    // pubs/subs/services on them.
    for p in raw_processes {
        let h = touch(&mut by_host, &p.host_name);
        if !p.shm_transport_domain.is_empty()
            && !h.shm_transport_domains.contains(&p.shm_transport_domain)
        {
            h.shm_transport_domains.push(p.shm_transport_domain.clone());
        }
    }

    for h in by_host.values_mut() {
        h.shm_transport_domains.sort();
        h.runtime_versions.sort();
    }
    by_host.into_values().collect()
}

/// Return up to `limit` names from `candidates` that look most like `target`, ordered best-first.
/// Cheap heuristic: prefer names whose case-insensitive containment of any 3+ char substring of the
/// target scores highest, then prefer names with smaller absolute length difference. Good enough to
/// catch typos / wrong namespace prefixes.
#[cfg(test)]
fn similar_names<'a>(
    target: &str,
    candidates: impl Iterator<Item = &'a str>,
    limit: usize,
) -> Vec<String> {
    similar_names_with_scores(target, candidates, limit)
        .into_iter()
        .map(|(n, _)| n)
        .collect()
}

fn similar_names_with_scores<'a>(
    target: &str,
    candidates: impl Iterator<Item = &'a str>,
    limit: usize,
) -> Vec<(String, f64)> {
    let t_lower = target.to_ascii_lowercase();
    let t_grams: Vec<&str> = (0..t_lower.len().saturating_sub(2))
        .filter_map(|i| t_lower.get(i..i + 3))
        .collect();
    let mut scored: Vec<(i32, String)> = candidates
        .filter_map(|c| {
            if c == target {
                return None;
            }
            let lower = c.to_ascii_lowercase();
            let hits = t_grams.iter().filter(|g| lower.contains(*g)).count() as i32;
            let len_diff = (lower.len() as i32 - t_lower.len() as i32).abs();
            let score = hits * 100 - len_diff;
            if hits == 0 {
                None
            } else {
                Some((score, c.to_string()))
            }
        })
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0));
    scored
        .into_iter()
        .take(limit)
        .map(|(s, n)| (n, s as f64))
        .collect()
}

/// Returns `Some(publishers)` only when ≥2 publishers are registered for `topic`, so callers can
/// echo the rogue-publisher set without a follow-up `ecal_list_publishers` call. Empty /
/// single-publisher cases collapse to `None` so the response stays clean in the common case.
fn collect_echoed_publishers(
    snap: &rustecal_core::core_types::monitoring::MonitoringSnapshot,
    topic: &str,
) -> Option<Vec<EchoedPublisher>> {
    let pubs: Vec<EchoedPublisher> = snap
        .publishers
        .iter()
        .filter(|t| t.topic_name == topic)
        .map(|t| EchoedPublisher {
            host_name: t.host_name.clone(),
            unit_name: t.unit_name.clone(),
            process_name: t.process_name.clone(),
            topic_id: t.topic_id,
            registered_data_frequency_hz: f64::from(t.data_frequency) / 1000.0,
        })
        .collect();
    if pubs.len() >= 2 {
        Some(pubs)
    } else {
        None
    }
}

/// Build the `unhealthy_process` finding for one (side, entry, process) triple, or `None` when
/// `state_severity < 2` (healthy / unknown). Factored out of the diagnose loop so it can be
/// unit-tested without running the full eCAL monitoring pipeline.
fn build_unhealthy_process_finding(
    side: &str,
    e: &TopicEntry,
    proc_state: &rustecal_core::core_types::monitoring::ProcessInfo,
) -> Option<(FindingCode, String, Option<serde_json::Value>)> {
    if proc_state.state_severity < 2 {
        return None;
    }
    let msg = format!(
        "{side} '{}' (pid {}) on {} is in non-healthy state \
         (severity={}, level={}): {}",
        e.unit_name,
        e.process_id,
        e.host_name,
        proc_state.state_severity,
        proc_state.state_severity_level,
        if proc_state.state_info.is_empty() {
            "<no state_info>"
        } else {
            &proc_state.state_info
        },
    );
    let detail = serde_json::json!({
        "side": side,
        "host_name": e.host_name,
        "unit_name": e.unit_name,
        "process_id": e.process_id,
        "topic_id": e.topic_id,
        "state_severity": proc_state.state_severity,
        "state_severity_level": proc_state.state_severity_level,
        "state_info": proc_state.state_info,
    });
    Some((FindingCode::UnhealthyProcess, msg, Some(detail)))
}

fn name_matches(haystack: &str, pattern: &Option<String>) -> bool {
    match pattern.as_deref() {
        None | Some("") => true,
        Some(p) => haystack
            .to_ascii_lowercase()
            .contains(&p.to_ascii_lowercase()),
    }
}

/// Shared argument struct for `ecal_list_publishers`, `ecal_list_subscribers`,
/// `ecal_list_services`, and `ecal_list_service_clients`. Also accepted by `ecal_list_processes`
/// (only `name_pattern` applies — no types or descriptors) and `ecal_list_hosts` (filter is ignored
/// entirely).
#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
struct ListFilter {
    /// Optional case-insensitive substring filter on the entity name (`topic_name` for pub/sub,
    /// `service_name` for services/clients, `unit_name` or `process_name` for processes). Omit or
    /// pass `""` for "no filter".
    #[serde(default)]
    name_pattern: Option<String>,
    /// Optional case-insensitive **substring** filter on `datatype_information.name`. Looser than
    /// `ecal_mon_cli --find`, which requires an **exact** case-sensitive match on the type name —
    /// substring + case-fold catches more typos and namespace variants. Has no effect on
    /// service/process listings (services use method-level types).
    #[serde(default)]
    type_name_pattern: Option<String>,
    /// When true, include the binary type descriptor blob (base-64) in each `datatype_information`
    /// entry. Off by default because descriptors can be tens of KB per topic and you usually don't
    /// want them on a "give me everything" scan. Honored by `ecal_list_publishers`,
    /// `ecal_list_subscribers`, `ecal_list_services`, `ecal_list_service_clients`; ignored by
    /// `ecal_list_processes` and `ecal_list_hosts`.
    #[serde(default)]
    include_descriptors: Option<bool>,
}

#[tool_router]
impl EcalServer {
    pub fn new() -> Self {
        Self::default()
    }

    #[tool(
        description = "List eCAL publishers currently visible on the network. \
        Returns topic name, datatype_information (name/encoding/descriptor_len), \
        transport_layer (one entry per layer with type/version/active), \
        host, process, and traffic stats. Pass `name_pattern` to substring-filter \
        by topic, `type_name_pattern` to filter by type, and \
        `include_descriptors=true` to attach base-64 type descriptor blobs (off \
        by default; can be tens of KB each)."
    )]
    async fn ecal_list_publishers(
        &self,
        Parameters(args): Parameters<ListFilter>,
    ) -> Result<Json<PublishersResult>, McpError> {
        let snap = snapshot()?;
        let include = args.include_descriptors.unwrap_or(false);
        let publishers = snap
            .publishers
            .into_iter()
            .filter(|t| name_matches(&t.topic_name, &args.name_pattern))
            .filter(|t| name_matches(&t.data_type.type_name, &args.type_name_pattern))
            .map(|t| topic_entry(t, Direction::Publisher, include))
            .collect();
        Ok(Json(PublishersResult { publishers }))
    }

    #[tool(
        description = "List eCAL subscribers currently visible on the network. \
        Same shape and filtering as `ecal_list_publishers`. Subscriber-side \
        `datatype_information` is the **expected** type declared at construction, \
        not a peer-derived value."
    )]
    async fn ecal_list_subscribers(
        &self,
        Parameters(args): Parameters<ListFilter>,
    ) -> Result<Json<SubscribersResult>, McpError> {
        let snap = snapshot()?;
        let include = args.include_descriptors.unwrap_or(false);
        let subscribers = snap
            .subscribers
            .into_iter()
            .filter(|t| name_matches(&t.topic_name, &args.name_pattern))
            .filter(|t| name_matches(&t.data_type.type_name, &args.type_name_pattern))
            .map(|t| topic_entry(t, Direction::Subscriber, include))
            .collect();
        Ok(Json(SubscribersResult { subscribers }))
    }

    #[tool(
        description = "List eCAL service servers visible on the network. Each entry \
        has one `service_id` per server (matches upstream `SServer.service_id` — \
        per-server, NOT per-method) plus a `methods[]` array. Each method carries \
        full `request_datatype_information` and `response_datatype_information` \
        (`{name, encoding, descriptor_len, descriptor_fingerprint, descriptor_base64?}`) \
        — the same shape as `TopicEntry.datatype_information`. Empty `name`/`encoding` \
        with `descriptor_len == 0` means the server didn't advertise a type \
        (e.g. raw `add_method` registrations). Pass `name_pattern` to \
        substring-filter by service name; pass `include_descriptors=true` to \
        attach base-64 descriptor blobs (off by default)."
    )]
    async fn ecal_list_services(
        &self,
        Parameters(args): Parameters<ListFilter>,
    ) -> Result<Json<ServicesResult>, McpError> {
        let snap = snapshot()?;
        let include = args.include_descriptors.unwrap_or(false);
        let services = snap
            .servers
            .into_iter()
            .filter(|s| name_matches(&s.service_name, &args.name_pattern))
            .map(|s| service_entry(s, include))
            .collect();
        Ok(Json(ServicesResult { services }))
    }

    #[tool(
        description = "List eCAL service *clients* visible on the network. Useful for \
        discovering who is calling which service. `service_id` matches upstream \
        `SClient.service_id`. Pass `name_pattern` to substring-filter by service \
        name; pass `include_descriptors=true` to attach descriptors on \
        `methods[*].{request,response}_datatype_information`."
    )]
    async fn ecal_list_service_clients(
        &self,
        Parameters(args): Parameters<ListFilter>,
    ) -> Result<Json<ServiceClientsResult>, McpError> {
        let snap = snapshot()?;
        let include = args.include_descriptors.unwrap_or(false);
        let clients = snap
            .clients
            .into_iter()
            .filter(|c| name_matches(&c.service_name, &args.name_pattern))
            .map(|c| client_entry(c, include))
            .collect();
        Ok(Json(ServiceClientsResult { clients }))
    }

    #[tool(
        description = "Drain pending eCAL log messages emitted by all processes since the \
        last call. Calls `eCAL_Logging_GetLogging`, which empties the local log buffer; \
        repeated calls only return new entries. Optional filters: `min_level` (one of \
        debug4|debug3|debug2|debug1|info|warning|error|fatal, default info — the \
        ordering is severity-ascending) drops anything below the named severity; \
        `since_timestamp_us` drops anything older; `process_name_pattern` \
        substring-filters by emitting process."
    )]
    async fn ecal_get_logs(
        &self,
        Parameters(args): Parameters<GetLogsArgs>,
    ) -> Result<Json<LogsResult>, McpError> {
        let logs = tokio::task::spawn_blocking(|| rustecal_core::log::Log::get_logging())
            .await
            .map_err(|e| McpError::internal_error(format!("get_logs task join: {e}"), None))?
            .map_err(|e| McpError::internal_error(format!("eCAL log fetch failed: {e:?}"), None))?;

        let min_priority = args
            .min_level
            .as_deref()
            .map(parse_min_level)
            .unwrap_or_else(|| level_priority(LogLevel::Info));
        let since = args.since_timestamp_us.unwrap_or(i64::MIN);

        let logs = logs
            .into_iter()
            .filter(|m| level_priority(m.level) >= min_priority)
            .filter(|m| m.timestamp >= since)
            .filter(|m| name_matches(&m.process_name, &args.process_name_pattern))
            .map(|m| LogEntry {
                level: format!("{:?}", m.level),
                timestamp_us: m.timestamp,
                host_name: m.host_name,
                process_name: m.process_name,
                process_id: m.process_id,
                unit_name: m.thread_name,
                content: m.content,
            })
            .collect();
        Ok(Json(LogsResult { logs }))
    }

    #[tool(
        description = "List eCAL processes visible on the network. Pass `name_pattern` \
        to substring-filter against `unit_name` or `process_name`. Note: \
        the underlying eCAL discovery may surface fewer processes per call than \
        actually exist (a known monitoring quirk on multi-host networks); the \
        canonical roster is the union of `ecal_list_publishers`, `_subscribers`, \
        and `_services`. Use `ecal_list_hosts` for a derived host roster."
    )]
    async fn ecal_list_processes(
        &self,
        Parameters(args): Parameters<ListFilter>,
    ) -> Result<Json<ProcessesResult>, McpError> {
        let snap = snapshot()?;
        let processes = snap
            .processes
            .into_iter()
            .filter(|p| {
                name_matches(&p.unit_name, &args.name_pattern)
                    || name_matches(&p.process_name, &args.name_pattern)
            })
            .map(process_entry)
            .collect();
        Ok(Json(ProcessesResult { processes }))
    }

    #[tool(
        description = "List eCAL **hosts** visible on the network — the same first-class \
        entity the upstream eCAL Monitor GUI surfaces in its Hosts view. Each \
        entry aggregates per-host counts (publishers/subscribers/servers/clients/processes), \
        the distinct `shm_transport_domains` seen on that host (>1 ⇒ same-host \
        SHM ineligible between some processes), and the set-union of \
        `runtime_version` strings (>1 ⇒ heterogeneous eCAL builds on one box). \
        Cheap to call: derived from the same single monitoring snapshot."
    )]
    async fn ecal_list_hosts(
        &self,
        Parameters(_): Parameters<ListFilter>,
    ) -> Result<Json<HostsResult>, McpError> {
        let snap = snapshot()?;
        let pubs: Vec<TopicEntry> = snap
            .publishers
            .iter()
            .cloned()
            .map(|t| topic_entry(t, Direction::Publisher, false))
            .collect();
        let subs: Vec<TopicEntry> = snap
            .subscribers
            .iter()
            .cloned()
            .map(|t| topic_entry(t, Direction::Subscriber, false))
            .collect();
        let services: Vec<ServiceEntry> = snap
            .servers
            .iter()
            .cloned()
            .map(|s| service_entry(s, false))
            .collect();
        let clients: Vec<ClientEntry> = snap
            .clients
            .iter()
            .cloned()
            .map(|c| client_entry(c, false))
            .collect();
        let processes: Vec<ProcessEntry> =
            snap.processes.iter().cloned().map(process_entry).collect();
        let hosts = build_hosts(
            &pubs,
            &subs,
            &services,
            &clients,
            &processes,
            &snap.processes,
        );
        Ok(Json(HostsResult { hosts }))
    }

    #[tool(
        description = "Single-call snapshot of everything on the eCAL network: \
        publishers, subscribers, services, clients, processes, and the derived \
        hosts roster. Wraps rustecal `Monitoring::get_snapshot()` (which calls \
        the C `eCAL_Monitoring_GetMonitoring` with all entities); the `hosts` \
        roster is derived by ecal-mcp, not part of upstream `SMonitoring`. \
        Prefer this over four sequential `ecal_list_*` calls when you need a \
        coherent network survey (single snapshot ⇒ no race between list calls)."
    )]
    async fn ecal_get_monitoring(
        &self,
        Parameters(args): Parameters<GetMonitoringArgs>,
    ) -> Result<Json<MonitoringResult>, McpError> {
        let snap = snapshot()?;
        let include = args.include_descriptors.unwrap_or(false);
        let publishers: Vec<TopicEntry> = snap
            .publishers
            .iter()
            .cloned()
            .map(|t| topic_entry(t, Direction::Publisher, include))
            .collect();
        let subscribers: Vec<TopicEntry> = snap
            .subscribers
            .iter()
            .cloned()
            .map(|t| topic_entry(t, Direction::Subscriber, include))
            .collect();
        let servers: Vec<ServiceEntry> = snap
            .servers
            .iter()
            .cloned()
            .map(|s| service_entry(s, include))
            .collect();
        let clients: Vec<ClientEntry> = snap
            .clients
            .iter()
            .cloned()
            .map(|c| client_entry(c, include))
            .collect();
        let processes: Vec<ProcessEntry> =
            snap.processes.iter().cloned().map(process_entry).collect();
        let hosts = build_hosts(
            &publishers,
            &subscribers,
            &servers,
            &clients,
            &processes,
            &snap.processes,
        );
        Ok(Json(MonitoringResult {
            publishers,
            subscribers,
            servers,
            clients,
            processes,
            hosts,
        }))
    }

    #[tool(description = "Publish a message to an eCAL topic. \
        Provide either `text` (UTF-8 string, sent as eCAL StringMessage) or `payload_base64` \
        (base-64 encoded bytes, sent as eCAL BytesMessage). \
        `discovery_wait_ms` (default 500) polls `get_subscriber_count` until at least \
        one subscriber is **actively connected** before the first send (not just \
        registered) — set higher on a fresh process to avoid the no-subscribers-yet \
        path that returns false from `Send`. \
        `repeat` (default 1) issues the same payload N times back-to-back, spaced by \
        `repeat_interval_ms` (default 100); useful for giving late-joining subscribers a \
        chance to receive it. `sends_attempted` echoes `repeat`; `sends_succeeded` counts \
        the subset where `Publisher::Send` returned true.")]
    async fn ecal_publish(
        &self,
        Parameters(args): Parameters<PublishArgs>,
    ) -> Result<Json<PublishResult>, McpError> {
        if args.text.is_some() == args.payload_base64.is_some() {
            return Err(McpError::invalid_params(
                "exactly one of `text` or `payload_base64` must be provided",
                None,
            ));
        }

        let discovery_wait = Duration::from_millis(args.discovery_wait_ms.unwrap_or(500));
        let repeat = args.repeat.unwrap_or(1).max(1);
        let interval = Duration::from_millis(args.repeat_interval_ms.unwrap_or(100));
        let topic = args.topic_name.clone();

        let result = tokio::task::spawn_blocking(move || -> Result<PublishResult, String> {
            let mut sends_ok: u32 = 0;
            let bytes_sent: usize;

            // Wait for at least one **active** subscriber to be discovered.
            // `GetSubscriberCount()` only counts connections whose `state ==
            // true` in `CPublisherImpl::m_connection_map`. The first
            // registration ping stores `state == false` and only the second
            // update flips it on, so this is a stronger signal than
            // "registration arrived". `CPublisher::Send` returns `false`
            // when this count is zero: it still calls
            // `RefreshSendCounter()` (so `data_clock` and frequency stats
            // advance) but does NOT put bytes on the wire. Polling here
            // makes the first send actually land.
            fn await_subscriber<T: PublisherMessage>(
                publisher: &TypedPublisher<T>,
                budget: Duration,
            ) {
                let deadline = Instant::now() + budget;
                while publisher.get_subscriber_count() == 0 && Instant::now() < deadline {
                    std::thread::sleep(Duration::from_millis(20));
                }
            }

            if let Some(text) = args.text {
                let publisher = TypedPublisher::<StringMessage>::new(&topic)
                    .map_err(|e| format!("create publisher: {e}"))?;
                await_subscriber(&publisher, discovery_wait);
                let msg = StringMessage {
                    data: text.clone().into(),
                };
                bytes_sent = text.as_bytes().len();
                for i in 0..repeat {
                    if publisher.send(&msg, Timestamp::Auto) {
                        sends_ok += 1;
                    }
                    if i + 1 < repeat {
                        std::thread::sleep(interval);
                    }
                }
            } else {
                let raw = B64
                    .decode(args.payload_base64.unwrap())
                    .map_err(|e| format!("invalid base64: {e}"))?;
                bytes_sent = raw.len();
                let publisher = TypedPublisher::<BytesMessage>::new(&topic)
                    .map_err(|e| format!("create publisher: {e}"))?;
                await_subscriber(&publisher, discovery_wait);
                let msg = BytesMessage {
                    data: Cow::Owned(raw),
                };
                for i in 0..repeat {
                    if publisher.send(&msg, Timestamp::Auto) {
                        sends_ok += 1;
                    }
                    if i + 1 < repeat {
                        std::thread::sleep(interval);
                    }
                }
            }

            Ok(PublishResult {
                topic_name: topic,
                bytes_sent,
                sends_succeeded: sends_ok,
                sends_attempted: repeat,
            })
        })
        .await
        .map_err(|e| McpError::internal_error(format!("publish task join: {e}"), None))?
        .map_err(|e| McpError::internal_error(e, None))?;

        Ok(Json(result))
    }

    #[tool(
        description = "Subscribe to an eCAL topic for a fixed duration and return any \
        samples seen. UTF-8 payloads are returned as `text`; the base-64 form is \
        suppressed for short printable text and always present for binary or large \
        payloads, so the exact bytes are always recoverable. \
        `samples_truncated_at_cap` counts samples that arrived but were dropped \
        because `max_samples` was hit — it's an MCP response-cap, NOT \
        `STopic.message_drops` (transport / callback drop). \
        Each `samples[*]` row echoes `topic_name` straight from the wire \
        (`eCAL_STopicId.topic_name`) — under normal usage this matches the \
        subscribed topic. It is **not** a per-publisher discriminator (multiple \
        publishers on the same topic emit the same string); to attribute a \
        sample to a specific publisher, cross-reference `ecal_list_publishers` \
        by `entity_id` out of band."
    )]
    async fn ecal_subscribe(
        &self,
        Parameters(args): Parameters<SubscribeArgs>,
    ) -> Result<Json<SubscribeResult>, McpError> {
        let duration_ms = args.duration_ms.unwrap_or(1000);
        let max_samples = args.max_samples.unwrap_or(10);
        let topic = args.topic_name.clone();

        let result = tokio::task::spawn_blocking(move || -> Result<SubscribeResult, String> {
            let collected: Arc<Mutex<Vec<Sample>>> = Arc::new(Mutex::new(Vec::new()));
            let truncated: Arc<Mutex<usize>> = Arc::new(Mutex::new(0));

            let mut sub = TypedSubscriber::<BytesMessage>::new(&topic)
                .map_err(|e| format!("create subscriber: {e}"))?;

            {
                let collected = collected.clone();
                let truncated = truncated.clone();
                sub.set_callback(move |received| {
                    let mut guard = collected.lock();
                    if guard.len() >= max_samples {
                        *truncated.lock() += 1;
                        return;
                    }
                    let bytes: &[u8] = received.payload.data.as_ref();
                    let text = std::str::from_utf8(bytes).ok().map(|s| s.to_string());
                    let payload_base64 = if text.is_some() && bytes.len() <= INLINE_TEXT_BYTES {
                        None
                    } else {
                        Some(B64.encode(bytes))
                    };
                    guard.push(Sample {
                        topic_name: received.topic_name,
                        timestamp_us: received.timestamp,
                        clock: received.clock,
                        size: bytes.len(),
                        text,
                        payload_base64,
                        encoding: received.encoding,
                        name: received.type_name,
                    });
                });
            }

            std::thread::sleep(Duration::from_millis(duration_ms));
            drop(sub);

            let samples = std::mem::take(&mut *collected.lock());
            let cap_overflow = *truncated.lock();
            Ok(SubscribeResult {
                topic_name: topic,
                duration_ms,
                samples_collected: samples.len(),
                samples_truncated_at_cap: cap_overflow,
                samples,
            })
        })
        .await
        .map_err(|e| McpError::internal_error(format!("subscribe task join: {e}"), None))?
        .map_err(|e| McpError::internal_error(e, None))?;

        Ok(Json(result))
    }

    #[tool(
        description = "Call an eCAL service method on every discovered server instance and \
        return one entry per instance. Provide either `text` (UTF-8) or \
        `payload_base64` as the request payload. Set `target_service_id` to scope \
        the call to a single replica — when used, `responses` may be shorter \
        than `discovered_instances`, and the caller's id is echoed back as \
        `requested_target_service_id` so you can distinguish \"service is gone\" \
        (`requested_target_service_id: null`, `discovered_instances: 0`) from \
        \"my filter rejected every replica\" (`requested_target_service_id: <id>`, \
        `discovered_instances > 0`, `len(responses) == 0`). \
        \
        Each `responses[].service_id` matches the upstream `SServer.service_id` \
        (= the `service_id` field on `ecal_list_services` rows). \
        `instance_index` is an index into `responses[]`, not into the discovered \
        fleet (so it's always `0` when a target filter is in effect). Short \
        printable UTF-8 responses are returned as `response_text` only; binary or \
        large responses also include `response_base64`."
    )]
    async fn ecal_call_service(
        &self,
        Parameters(args): Parameters<CallServiceArgs>,
    ) -> Result<Json<CallServiceResult>, McpError> {
        if args.text.is_some() == args.payload_base64.is_some() {
            return Err(McpError::invalid_params(
                "exactly one of `text` or `payload_base64` must be provided",
                None,
            ));
        }

        let payload: Vec<u8> = if let Some(t) = args.text.as_ref() {
            t.as_bytes().to_vec()
        } else {
            B64.decode(args.payload_base64.as_ref().unwrap())
                .map_err(|e| McpError::invalid_params(format!("invalid base64: {e}"), None))?
        };

        let timeout_ms = args.timeout_ms.unwrap_or(2000);
        let discovery_wait_ms = args.discovery_wait_ms.unwrap_or(1000);
        let service_name = args.service.clone();
        let method_name = args.method.clone();
        let target = args.target_service_id;

        let result = tokio::task::spawn_blocking(move || -> Result<CallServiceResult, String> {
            let client = ServiceClient::new(&service_name)
                .map_err(|e| format!("create service client: {e}"))?;

            let deadline = std::time::Instant::now() + Duration::from_millis(discovery_wait_ms);
            while client.get_client_instances().is_empty() && std::time::Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(50));
            }

            let instances = client.get_client_instances();
            let discovered = instances.len();
            if instances.is_empty() {
                return Ok(CallServiceResult {
                    service: service_name,
                    method: method_name,
                    discovered_instances: 0,
                    requested_target_service_id: None,
                    responses: Vec::new(),
                });
            }

            let mut responses = Vec::with_capacity(instances.len());
            for instance in instances.into_iter() {
                let req = ServiceRequest {
                    payload: payload.clone(),
                };
                match instance.call(&method_name, req, Some(timeout_ms)) {
                    Some(res) => {
                        let raw_id = res.server_id.service_id;
                        if let Some(t) = target {
                            if raw_id.entity_id != t {
                                continue;
                            }
                        }
                        let bytes = res.payload;
                        let text = std::str::from_utf8(&bytes).ok().map(|s| s.to_string());
                        let response_base64 = if text.is_some() && bytes.len() <= INLINE_TEXT_BYTES
                        {
                            None
                        } else {
                            Some(B64.encode(&bytes))
                        };
                        let host_name = if raw_id.host_name.is_null() {
                            String::new()
                        } else {
                            unsafe { CStr::from_ptr(raw_id.host_name) }
                                .to_string_lossy()
                                .into_owned()
                        };
                        responses.push(ServiceCallResponse {
                            instance_index: responses.len(),
                            success: res.success,
                            error: res.error_msg,
                            service_id: raw_id.entity_id,
                            service_process_id: raw_id.process_id,
                            service_host_name: host_name,
                            response_size: bytes.len(),
                            response_text: text,
                            response_base64,
                        });
                    }
                    None => {
                        if target.is_some() {
                            // Without an instance id lookup we can't tell
                            // whether the silent instance is the one the
                            // caller targeted; safer to drop than to
                            // pollute the result with a phantom row.
                            continue;
                        }
                        responses.push(ServiceCallResponse {
                            instance_index: responses.len(),
                            success: false,
                            // In rustecal, `ClientInstance::call` only
                            // returns `None` when `CString::new(method)`
                            // fails (i.e. the method name has an interior
                            // NUL byte). Real timeouts / server crashes /
                            // transport failures come back as
                            // `Some(ServiceResponse { success: false, .. })`
                            // with `error_msg` populated by the C layer.
                            // So this branch is unusual and indicates a
                            // bad method string.
                            error: Some(format!(
                                "rustecal returned None for method {method_name:?} \
                                 (likely an interior NUL in the method name)"
                            )),
                            service_id: 0,
                            service_process_id: 0,
                            service_host_name: String::new(),
                            response_size: 0,
                            response_text: None,
                            response_base64: None,
                        });
                    }
                }
            }

            Ok(CallServiceResult {
                service: service_name,
                method: method_name,
                discovered_instances: discovered,
                requested_target_service_id: target,
                responses,
            })
        })
        .await
        .map_err(|e| McpError::internal_error(format!("call_service task join: {e}"), None))?
        .map_err(|e| McpError::internal_error(e, None))?;

        Ok(Json(result))
    }

    #[tool(
        description = "Measure the *live* throughput of an eCAL topic over a window. \
        Subscribes for `duration_ms` (default 2000, hard-capped at 60000) and returns \
        payload-size stats, inter-arrival jitter, and a derived `wire_data_frequency_hz` \
        (= 1e6 / mean_gap_us). Unlike the cached `registered_data_frequency_hz` field \
        on `ecal_list_publishers` (which reflects the registration snapshot — \
        `data_frequency` mHz upstream — not the wire), `wire_data_frequency_hz` is what is \
        actually flowing right now. \
        Pass `expected_hz` to get `meets_spec` / `delta_hz` / `delta_direction` / \
        `ratio_to_target` pre-computed; pass `tolerance_pct` (e.g. 0.02 for ±2%) to \
        override the 10% default. The response echoes the resolved tolerance back \
        as `tolerance_pct`. `jitter_pct` = `gap_stddev_us / mean_gap_us` is also \
        included so reports don't have to re-compute it. When two or more publishers \
        are registered for the topic, the response also echoes `publishers[]` so the \
        rogue-publisher case is visible from the rate tool alone."
    )]
    async fn ecal_topic_stats(
        &self,
        Parameters(args): Parameters<TopicStatsArgs>,
    ) -> Result<Json<TopicStatsResult>, McpError> {
        let duration_ms = args.duration_ms.unwrap_or(2000).min(60_000);
        let expected_hz = args.expected_hz;
        let tolerance_pct = args.tolerance_pct;
        let topic = args.topic_name.clone();

        let snap = snapshot()?;
        let echoed_publishers = collect_echoed_publishers(&snap, &topic);

        let result = tokio::task::spawn_blocking(move || -> Result<TopicStatsResult, String> {
            measure_topic(&topic, duration_ms, expected_hz, tolerance_pct)
        })
        .await
        .map_err(|e| McpError::internal_error(format!("topic_stats task join: {e}"), None))?
        .map_err(|e| McpError::internal_error(e, None))?;

        let mut result = result;
        result.publishers = echoed_publishers;
        Ok(Json(result))
    }

    #[tool(
        description = "First tool to reach for when answering 'why am I not getting data?'. \
        Combines a monitoring snapshot (all matching publishers + subscribers, type \
        signatures, transport_layer, SHM domains) with an optional live measurement \
        window. \
        \
        Response shape: `findings[]` is the actionable part — each entry has a stable \
        machine-readable `code` (switch on this; codes include \
        `no_publishers_or_subscribers`, `no_publisher`, `no_subscriber`, \
        `type_metadata_mismatch`, `no_common_transport`, `shm_domain_split`, \
        `message_drops`, `unhealthy_process`, `did_you_mean`, `no_samples_observed`, \
        `multiple_publishers`, `rate_below_spec`, `rate_above_spec`), a `severity` \
        (`error`/`warning`/`info`), a human `message` (don't pattern-match), and an \
        optional structured `detail` payload. Findings are sorted by likely-root-cause \
        (with `severity` as a tiebreaker) so the load-bearing item is index 0 even when \
        a lower-priority advisory is also firing. \
        \
        `type_signatures[]` splits occurrences per side (`publisher_occurrences` / \
        `subscriber_occurrences`) and attributes each row back to the responsible \
        entities via `publisher_topic_ids` / `subscriber_topic_ids`. \
        `live_stats` is the literal JSON `null` when no endpoints are registered or \
        when caller passed `duration_ms=0`. Pass `expected_hz` (and optionally \
        `tolerance_pct`) to get `rate_below_spec` / `rate_above_spec` findings \
        without a separate `ecal_topic_stats` call. Subscriber-side \
        `datatype_information` is the subscriber's *expected* type (declared at \
        construction), not a peer-derived value — useful when reporting `no_publisher`."
    )]
    async fn ecal_diagnose_topic(
        &self,
        Parameters(args): Parameters<DiagnoseTopicArgs>,
    ) -> Result<Json<DiagnoseTopicResult>, McpError> {
        let duration_ms = args.duration_ms.unwrap_or(1500).min(60_000);
        let expected_hz = args.expected_hz;
        let tolerance_pct = args.tolerance_pct;
        let topic = args.topic_name.clone();

        let snap = snapshot()?;
        let all_pub_names: std::collections::BTreeSet<String> = snap
            .publishers
            .iter()
            .map(|t| t.topic_name.clone())
            .collect();
        let all_sub_names: std::collections::BTreeSet<String> = snap
            .subscribers
            .iter()
            .map(|t| t.topic_name.clone())
            .collect();
        let pubs: Vec<TopicEntry> = snap
            .publishers
            .iter()
            .filter(|t| t.topic_name == topic)
            .cloned()
            .map(|t| topic_entry(t, Direction::Publisher, false))
            .collect();
        let subs: Vec<TopicEntry> = snap
            .subscribers
            .iter()
            .filter(|t| t.topic_name == topic)
            .cloned()
            .map(|t| topic_entry(t, Direction::Subscriber, false))
            .collect();

        let mut sigs: std::collections::BTreeMap<(String, String), TypeSignatureAcc> =
            Default::default();
        for e in &pubs {
            let acc = sigs
                .entry((
                    e.datatype_information.name.clone(),
                    e.datatype_information.encoding.clone(),
                ))
                .or_default();
            acc.publisher_occurrences += 1;
            acc.publisher_topic_ids.push(e.topic_id);
        }
        for e in &subs {
            let acc = sigs
                .entry((
                    e.datatype_information.name.clone(),
                    e.datatype_information.encoding.clone(),
                ))
                .or_default();
            acc.subscriber_occurrences += 1;
            acc.subscriber_topic_ids.push(e.topic_id);
        }
        let type_signatures: Vec<TypeSignature> = sigs
            .into_iter()
            .map(|((name, encoding), mut acc)| {
                acc.publisher_topic_ids.sort_unstable();
                acc.subscriber_topic_ids.sort_unstable();
                TypeSignature {
                    name,
                    encoding,
                    publisher_occurrences: acc.publisher_occurrences,
                    subscriber_occurrences: acc.subscriber_occurrences,
                    publisher_topic_ids: acc.publisher_topic_ids,
                    subscriber_topic_ids: acc.subscriber_topic_ids,
                }
            })
            .collect();

        let (common_active_transport_layers, both_sides_advertised) =
            intersect_active_transports(&pubs, &subs);

        let mut domains: std::collections::BTreeSet<String> = Default::default();
        for e in pubs.iter().chain(subs.iter()) {
            domains.insert(e.shm_transport_domain.clone());
        }
        let shm_transport_domains: Vec<String> = domains.into_iter().collect();

        let no_endpoints = pubs.is_empty() && subs.is_empty();
        let mut raw_findings: Vec<(FindingCode, String, Option<serde_json::Value>)> = Vec::new();

        if no_endpoints {
            raw_findings.push((
                FindingCode::NoPublishersOrSubscribers,
                "no publishers or subscribers registered for this topic".into(),
                None,
            ));
        } else if pubs.is_empty() {
            raw_findings.push((
                FindingCode::NoPublisher,
                "subscribers exist but no publisher is registered".into(),
                None,
            ));
        } else if subs.is_empty() {
            raw_findings.push((
                FindingCode::NoSubscriber,
                "publishers exist but no subscriber is registered".into(),
                None,
            ));
        }
        if type_signatures.len() > 1 {
            // Enrich the detail with one row per signature so the consumer doesn't have
            // to pivot back to `type_signatures[]` + `publishers[]/subscribers[]`. Each
            // row carries the descriptor metadata of the first endpoint we can find for
            // that (name, encoding) — pubs first (more authoritative), subs as fallback.
            let descriptor_for = |name: &str, encoding: &str| -> (Option<String>, usize) {
                pubs.iter()
                    .chain(subs.iter())
                    .find(|e| {
                        e.datatype_information.name == name
                            && e.datatype_information.encoding == encoding
                    })
                    .map(|e| {
                        (
                            e.datatype_information.descriptor_fingerprint.clone(),
                            e.datatype_information.descriptor_len,
                        )
                    })
                    .unwrap_or((None, 0))
            };
            let signatures: Vec<serde_json::Value> = type_signatures
                .iter()
                .map(|s| {
                    let (fingerprint, descriptor_len) = descriptor_for(&s.name, &s.encoding);
                    serde_json::json!({
                        "name": s.name,
                        "encoding": s.encoding,
                        "descriptor_fingerprint": fingerprint,
                        "descriptor_len": descriptor_len,
                        "publisher_count": s.publisher_occurrences,
                        "subscriber_count": s.subscriber_occurrences,
                        "publisher_topic_ids": s.publisher_topic_ids,
                        "subscriber_topic_ids": s.subscriber_topic_ids,
                    })
                })
                .collect();
            raw_findings.push((
                FindingCode::TypeMetadataMismatch,
                format!(
                    "metadata mismatch: {} distinct (name, encoding) signatures across \
                     endpoints — raw bytes still flow, but typed deserialization will fail on \
                     the side(s) that disagree",
                    type_signatures.len()
                ),
                Some(serde_json::json!({
                    "signature_count": type_signatures.len(),
                    "signatures": signatures,
                })),
            ));
        }
        if !pubs.is_empty()
            && !subs.is_empty()
            && both_sides_advertised
            && common_active_transport_layers.is_empty()
        {
            // NOTE: this branch can only fire when both sides advertised
            // *non-empty* active layer sets but their intersection is
            // empty. We can't faithfully fire it from configured-but-not-
            // yet-active layer state because rustecal doesn't expose
            // `Configuration::TransportLayer.{shm,udp,tcp}.enable` today
            // (out of scope for this round; tracked as carryover).
            raw_findings.push((
                FindingCode::NoCommonTransport,
                "publishers and subscribers share no active transport layer; data cannot flow"
                    .into(),
                None,
            ));
        }
        // SHM-domain mismatch only matters across endpoints with *different
        // host_names*. Same-host endpoints are eligible for SHM regardless
        // of the configured domain (eCAL short-circuits on host equality).
        let mut hosts: std::collections::BTreeSet<&str> = Default::default();
        for e in pubs.iter().chain(subs.iter()) {
            hosts.insert(&e.host_name);
        }
        // Suppress `shm_domain_split` entirely when a real transport is
        // already negotiated OR when every host's default domain equals
        // its hostname (the "no real config divergence" inter-host
        // topology) — in those cases the split is pure noise and was
        // overranking real findings on every cross-host scenario.
        let shm_split_is_noise = !common_active_transport_layers.is_empty()
            || pubs
                .iter()
                .chain(subs.iter())
                .all(|e| e.shm_transport_domain == e.host_name);
        if hosts.len() > 1 && shm_transport_domains.len() > 1 && !shm_split_is_noise {
            raw_findings.push((
                FindingCode::ShmDomainSplit,
                format!(
                    "{} hosts with {} distinct shm_transport_domain values: cross-host SHM is \
                     only eligible when all sides agree on the domain string",
                    hosts.len(),
                    shm_transport_domains.len()
                ),
                Some(serde_json::json!({
                    "host_count": hosts.len(),
                    "shm_transport_domains": shm_transport_domains.clone(),
                })),
            ));
        }
        // message_drops + drop-rate cache. Compute deltas per
        // (publisher topic_id, subscriber topic_id) — we don't know which
        // publisher caused the drop, so key on every visible publisher
        // topic_id × this subscriber. Falls back to (0, sub.topic_id) when
        // no publisher is visible (rare but possible while a publisher is
        // mid-restart).
        let now = Instant::now();
        let pub_topic_ids: Vec<i64> = pubs.iter().map(|p| p.topic_id).collect();
        {
            let mut cache_guard = self.drops_cache.lock();
            cache_guard.retain(|_, (_, t)| now.duration_since(*t) <= DROP_CACHE_TTL);
            for s in &subs {
                if s.message_drops <= 0 {
                    continue;
                }
                let pubs_iter: Vec<i64> = if pub_topic_ids.is_empty() {
                    vec![0]
                } else {
                    pub_topic_ids.clone()
                };
                let mut prior: Option<(i32, Instant)> = None;
                for pid in &pubs_iter {
                    if let Some((d, t)) = cache_guard.get(&(*pid, s.topic_id)).copied() {
                        prior = Some((d, t));
                        break;
                    }
                }
                for pid in &pubs_iter {
                    cache_guard.insert((*pid, s.topic_id), (s.message_drops, now));
                }
                let (drop_rate_per_sec, drops_since_last_snapshot, window_seconds) = match prior {
                    Some((prev_drops, prev_t)) => {
                        let window = now.duration_since(prev_t).as_secs_f64();
                        let delta = s.message_drops - prev_drops;
                        let rate = if window > 0.0 {
                            Some(delta as f64 / window)
                        } else {
                            None
                        };
                        (rate, Some(delta), Some(window))
                    }
                    None => (None, None, None),
                };
                raw_findings.push((
                    FindingCode::MessageDrops,
                    format!(
                        "subscriber {} (pid {}) on {} reports {} message drop(s) since registration",
                        s.unit_name, s.process_id, s.host_name, s.message_drops
                    ),
                    Some(serde_json::json!({
                        "host_name": s.host_name,
                        "unit_name": s.unit_name,
                        "process_id": s.process_id,
                        "topic_id": s.topic_id,
                        "message_drops": s.message_drops,
                        "drop_rate_per_sec": drop_rate_per_sec,
                        "drops_since_last_snapshot": drops_since_last_snapshot,
                        "window_seconds": window_seconds,
                    })),
                ));
            }
        }
        for (side, entries) in [("publisher", &pubs), ("subscriber", &subs)] {
            for e in entries.iter() {
                let Some(proc_state) = snap
                    .processes
                    .iter()
                    .find(|p| p.process_id == e.process_id && p.host_name == e.host_name)
                else {
                    continue;
                };
                if let Some(f) = build_unhealthy_process_finding(side, e, proc_state) {
                    raw_findings.push(f);
                }
            }
        }
        // multiple_publishers: 1:N producer anti-pattern. Surface only
        // when the topic has at least one matched subscriber — a
        // single-publisher / no-subscriber graph is normal.
        if pubs.len() >= 2 && !subs.is_empty() {
            let rates: Vec<f64> = pubs
                .iter()
                .map(|p| p.registered_data_frequency_hz)
                .collect();
            let nonzero: Vec<f64> = rates.iter().copied().filter(|h| *h > 0.0).collect();
            let rate_divergence_ratio = match (
                nonzero.iter().cloned().reduce(f64::max),
                nonzero.iter().cloned().reduce(f64::min),
            ) {
                (Some(mx), Some(mn)) if mn > 0.0 => mx / mn,
                _ => 1.0,
            };
            let pub_hosts: std::collections::BTreeSet<&str> =
                pubs.iter().map(|p| p.host_name.as_str()).collect();
            let host_divergence = pub_hosts.len() >= 2;
            let detail_pubs: Vec<serde_json::Value> = pubs
                .iter()
                .map(|p| {
                    serde_json::json!({
                        "host_name": p.host_name,
                        "unit_name": p.unit_name,
                        "process_id": p.process_id,
                        "topic_id": p.topic_id,
                        "registered_data_frequency_hz": p.registered_data_frequency_hz,
                        "data_clock": p.data_clock,
                    })
                })
                .collect();
            raw_findings.push((
                FindingCode::MultiplePublishers,
                format!(
                    "{} publishers registered for a topic with {} subscriber(s) — \
                     1:N producer anti-pattern; rate-divergence ratio {:.2}",
                    pubs.len(),
                    subs.len(),
                    rate_divergence_ratio
                ),
                Some(serde_json::json!({
                    "publisher_count": pubs.len() as i32,
                    "publishers": detail_pubs,
                    "rate_divergence_ratio": rate_divergence_ratio,
                    "host_divergence": host_divergence,
                })),
            ));
        }
        if no_endpoints {
            let all_topic_names: std::collections::BTreeSet<String> = all_pub_names
                .iter()
                .chain(all_sub_names.iter())
                .cloned()
                .collect();
            if let Some(detail) =
                build_did_you_mean(&topic, &all_topic_names, &snap, all_topic_names.iter())
            {
                let msg = match detail.get("suggestion").and_then(|v| v.as_str()) {
                    Some(s) => format!("topic {topic:?} not found; closest known topic is {s:?}"),
                    None => format!(
                        "topic {topic:?} not found; no near-miss in the registration snapshot"
                    ),
                };
                raw_findings.push((FindingCode::DidYouMean, msg, Some(detail)));
            }
        } else if pubs.is_empty() {
            // Producer side is missing → suggest from publisher roster
            // ("opposite side" of what's missing here).
            if let Some(detail) =
                build_did_you_mean(&topic, &all_pub_names, &snap, all_pub_names.iter())
            {
                let msg = match detail.get("suggestion").and_then(|v| v.as_str()) {
                    Some(s) => format!(
                        "no publisher for {topic:?}; closest topic with a publisher is {s:?}"
                    ),
                    None => format!(
                        "no publisher for {topic:?}; no near-miss publisher in the snapshot"
                    ),
                };
                raw_findings.push((FindingCode::DidYouMean, msg, Some(detail)));
            }
        } else if subs.is_empty() {
            if let Some(detail) =
                build_did_you_mean(&topic, &all_sub_names, &snap, all_sub_names.iter())
            {
                let msg = match detail.get("suggestion").and_then(|v| v.as_str()) {
                    Some(s) => format!(
                        "no subscriber for {topic:?}; closest topic with a subscriber is {s:?}"
                    ),
                    None => format!(
                        "no subscriber for {topic:?}; no near-miss subscriber in the snapshot"
                    ),
                };
                raw_findings.push((FindingCode::DidYouMean, msg, Some(detail)));
            }
        }

        let mut live_stats: Option<TopicStatsResult> = if duration_ms == 0 || no_endpoints {
            None
        } else {
            let topic_clone = topic.clone();
            let measured = tokio::task::spawn_blocking(move || {
                measure_topic(&topic_clone, duration_ms, expected_hz, tolerance_pct)
            })
            .await
            .map_err(|e| McpError::internal_error(format!("diagnose task join: {e}"), None))?
            .map_err(|e| McpError::internal_error(e, None))?;
            if measured.samples_observed == 0 && !pubs.is_empty() {
                raw_findings.push((
                    FindingCode::NoSamplesObserved,
                    format!(
                        "no samples observed in {duration_ms}ms despite {} registered publisher(s)",
                        pubs.len()
                    ),
                    Some(serde_json::json!({
                        "duration_ms": duration_ms,
                        "publisher_count": pubs.len(),
                    })),
                ));
            }
            Some(measured)
        };

        // rate_below_spec / rate_above_spec from live_stats
        if let (Some(stats), Some(target)) = (live_stats.as_ref(), expected_hz) {
            if let (Some(meets), Some(dir)) = (stats.meets_spec, stats.delta_direction) {
                if !meets {
                    let detail = serde_json::json!({
                        "wire_data_frequency_hz": stats.wire_data_frequency_hz,
                        "expected_hz": target,
                        "tolerance_pct": stats.tolerance_pct,
                        "ratio_to_target": stats.ratio_to_target,
                        "delta_hz": stats.delta_hz,
                        "delta_direction": dir,
                    });
                    let (code, msg) = match dir {
                        DeltaDirection::BelowSpec => (
                            FindingCode::RateBelowSpec,
                            format!(
                                "live rate {:.3} Hz is below expected {:.3} Hz (tolerance {:.2}%)",
                                stats.wire_data_frequency_hz.unwrap_or(0.0),
                                target,
                                stats.tolerance_pct.unwrap_or(0.0) * 100.0
                            ),
                        ),
                        DeltaDirection::AboveSpec => (
                            FindingCode::RateAboveSpec,
                            format!(
                                "live rate {:.3} Hz is above expected {:.3} Hz (tolerance {:.2}%)",
                                stats.wire_data_frequency_hz.unwrap_or(0.0),
                                target,
                                stats.tolerance_pct.unwrap_or(0.0) * 100.0
                            ),
                        ),
                        DeltaDirection::OnSpec => unreachable!("meets_spec=false ⇒ not OnSpec"),
                    };
                    raw_findings.push((code, msg, Some(detail)));
                }
            }
        }

        // Sort by likely-root-cause (`code_rank`) primary, with
        // `severity` as the tiebreaker. This puts the load-bearing
        // finding at index 0 even when a lower-priority advisory
        // (e.g. `did_you_mean`) is also firing.
        let mut findings: Vec<Finding> = raw_findings
            .into_iter()
            .map(|(code, message, detail)| Finding {
                severity: severity_for(code),
                code,
                message,
                detail,
            })
            .collect();
        findings.sort_by(|a, b| {
            code_rank(a.code)
                .cmp(&code_rank(b.code))
                .then(severity_rank(b.severity).cmp(&severity_rank(a.severity)))
        });

        // When a metadata mismatch is in effect, the live receive callback
        // binds to whichever signature TCP delivered first; the resulting
        // type_name / encoding scalars are unreliable. Null them out so
        // consumers don't quote them as the network's "real" type.
        let has_metadata_mismatch = findings
            .iter()
            .any(|f| matches!(f.code, FindingCode::TypeMetadataMismatch));
        if has_metadata_mismatch {
            if let Some(stats) = live_stats.as_mut() {
                stats.type_name = None;
                stats.encoding = None;
            }
        }

        Ok(Json(DiagnoseTopicResult {
            topic_name: topic,
            publishers: pubs,
            subscribers: subs,
            type_signatures,
            common_active_transport_layers,
            shm_transport_domains,
            findings,
            live_stats,
        }))
    }
}

#[derive(Default)]
struct TypeSignatureAcc {
    publisher_occurrences: usize,
    subscriber_occurrences: usize,
    publisher_topic_ids: Vec<i64>,
    subscriber_topic_ids: Vec<i64>,
}

/// Build the structured `did_you_mean.detail` object, given a target, the candidate name pool to
/// draw from, and the full snapshot for per-candidate enrichment (direction / occurrences / hosts /
/// rates).
fn build_did_you_mean<'a, I>(
    target: &str,
    pool: &std::collections::BTreeSet<String>,
    snap: &rustecal_core::core_types::monitoring::MonitoringSnapshot,
    _names: I,
) -> Option<serde_json::Value>
where
    I: Iterator<Item = &'a String>,
{
    use std::collections::BTreeSet;
    // Always emit a `did_you_mean` finding on negative-existence —
    // an empty `candidates[]` is the documented "we looked, nothing
    // close" signal. The skill promises this; silent absence used to
    // confuse fresh agents into a host-fs escape.
    let ranked = similar_names_with_scores(target, pool.iter().map(String::as_str), 10);
    let candidates: Vec<serde_json::Value> = ranked
        .iter()
        .map(|(name, score)| {
            let mut occurrences = 0i32;
            let mut hosts: BTreeSet<String> = BTreeSet::new();
            let mut max_hz: f64 = 0.0;
            let mut in_pubs = false;
            let mut in_subs = false;
            for t in snap.publishers.iter().filter(|t| t.topic_name == *name) {
                in_pubs = true;
                occurrences += 1;
                hosts.insert(t.host_name.clone());
                let hz = f64::from(t.data_frequency) / 1000.0;
                if hz > max_hz {
                    max_hz = hz;
                }
            }
            for t in snap.subscribers.iter().filter(|t| t.topic_name == *name) {
                in_subs = true;
                occurrences += 1;
                hosts.insert(t.host_name.clone());
                let hz = f64::from(t.data_frequency) / 1000.0;
                if hz > max_hz {
                    max_hz = hz;
                }
            }
            let direction = match (in_pubs, in_subs) {
                (true, true) => "both",
                (true, false) => "publisher",
                (false, true) => "subscriber",
                (false, false) => "unknown",
            };
            serde_json::json!({
                "topic_name": name,
                "score": *score,
                "direction": direction,
                "occurrences": occurrences,
                "host_names": hosts.into_iter().collect::<Vec<_>>(),
                "registered_data_frequency_hz": max_hz,
            })
        })
        .collect();
    let suggestion = ranked.first().map(|(name, _)| name.clone());
    Some(serde_json::json!({
        "suggestion": suggestion,
        "candidates": candidates,
    }))
}

#[tool_handler]
impl ServerHandler for EcalServer {
    fn get_info(&self) -> RmcpServerInfo {
        RmcpServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("ecal-mcp", env!("CARGO_PKG_VERSION")))
            .with_instructions(
                "Inspect and interact with an Eclipse eCAL pub/sub network. \
                 When debugging a topic that 'isn't working', start with \
                 `ecal_diagnose_topic` — it consolidates publishers, subscribers, \
                 type signatures, transport layers, process health, and a live \
                 measurement, then emits a `findings` list. \
                 For a single-call network survey use `ecal_get_monitoring`. \
                 Field names follow upstream eCAL: `datatype_information.name`, \
                 `transport_layer[*].active`, `service_id` per server, \
                 `direction` per topic entry, etc. Tools: ecal_list_publishers, \
                 ecal_list_subscribers, ecal_list_services, \
                 ecal_list_service_clients, ecal_list_processes, ecal_list_hosts, \
                 ecal_get_monitoring, ecal_get_logs, ecal_publish, ecal_subscribe, \
                 ecal_topic_stats, ecal_diagnose_topic, ecal_call_service. \
                 Note: monitoring data refreshes on the eCAL `registration_refresh` \
                 cadence (default 1000ms), so a process that started <1s ago may \
                 not be visible yet."
                    .to_string(),
            )
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info,rmcp=warn")),
        )
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();

    let parsed = cli::Cli::parse();
    let cmd = parsed.command.unwrap_or(cli::Cmd::Serve);

    // `tools` is a pure-reflection over the static `#[tool]` registry —
    // skipping eCAL init keeps stdout clean (eCAL prints its config banner
    // during init/finalize) and makes the listing instant.
    let needs_ecal = !matches!(cmd, cli::Cmd::Tools { .. });

    if needs_ecal {
        Ecal::initialize(Some("ecal_mcp"), EcalComponents::ALL, None)
            .map_err(|e| anyhow::anyhow!("eCAL initialize failed: {e:?}"))?;
    }

    let outcome: Result<()> = match cmd {
        cli::Cmd::Serve => {
            tracing::info!("eCAL initialized; serving MCP over stdio");
            async {
                let service = EcalServer::new()
                    .serve(rmcp::transport::stdio())
                    .await
                    .inspect_err(|e| tracing::error!(error = ?e, "MCP serve error"))?;
                service.waiting().await?;
                Ok(())
            }
            .await
        }
        other => {
            let pretty = match &other {
                cli::Cmd::Call { pretty, .. } => *pretty,
                cli::Cmd::Tools { compact } => !*compact,
                cli::Cmd::Serve => false,
            };
            let server = EcalServer::new();
            match cli::run(&server, other).await {
                Ok(v) => {
                    cli::print_value(&v, pretty);
                    Ok(())
                }
                Err(msg) => {
                    eprintln!("ecal-mcp: {msg}");
                    Err(anyhow::anyhow!(msg))
                }
            }
        }
    };

    if needs_ecal {
        Ecal::finalize();
    }

    outcome
}

#[cfg(test)]
mod tests {
    use super::*;

    fn te(layers: &[(&str, bool)], direction: Direction) -> TopicEntry {
        let transport_layer = layers
            .iter()
            .map(|(k, a)| TransportLayerEntry {
                layer_type: (*k).to_string(),
                version: 1,
                active: *a,
            })
            .collect();
        TopicEntry {
            topic_name: String::new(),
            direction,
            datatype_information: DataTypeInformation {
                name: String::new(),
                encoding: String::new(),
                descriptor_len: 0,
                descriptor_fingerprint: None,
                descriptor_base64: None,
            },
            host_name: String::new(),
            shm_transport_domain: String::new(),
            process_id: 0,
            process_name: String::new(),
            unit_name: String::new(),
            topic_id: 0,
            connections_local: 0,
            connections_external: 0,
            message_drops: 0,
            registered_data_frequency_hz: 0.0,
            data_clock: 0,
            registration_clock: 0,
            transport_layer,
        }
    }

    #[test]
    fn parse_min_level_aliases_and_unknown_label() {
        assert_eq!(parse_min_level("warn"), parse_min_level("warning"));
        assert_eq!(parse_min_level("err"), parse_min_level("error"));
        assert_eq!(parse_min_level("INFO"), parse_min_level("info"));
        assert_eq!(parse_min_level("debug4"), 1);
        assert_eq!(parse_min_level("fatal"), 8);
        assert_eq!(parse_min_level(""), 0);
        assert_eq!(parse_min_level("nonsense"), 0);
        let order = [
            "debug4", "debug3", "debug2", "debug1", "info", "warning", "error", "fatal",
        ];
        for w in order.windows(2) {
            assert!(
                parse_min_level(w[0]) < parse_min_level(w[1]),
                "ordering broken between {:?} and {:?}",
                w[0],
                w[1],
            );
        }
    }

    #[test]
    fn level_priority_matches_parse_min_level() {
        let pairs = [
            (LogLevel::Debug4, "debug4"),
            (LogLevel::Debug3, "debug3"),
            (LogLevel::Debug2, "debug2"),
            (LogLevel::Debug1, "debug1"),
            (LogLevel::Info, "info"),
            (LogLevel::Warning, "warning"),
            (LogLevel::Error, "error"),
            (LogLevel::Fatal, "fatal"),
        ];
        for (lvl, label) in pairs {
            assert_eq!(
                level_priority(lvl),
                parse_min_level(label),
                "mismatch for {label}"
            );
        }
        assert_eq!(level_priority(LogLevel::None), 0);
        assert_eq!(level_priority(LogLevel::All), 0);
    }

    #[test]
    fn intersect_active_transports_semantics() {
        let pubs = vec![te(
            &[("shm", true), ("udp_mc", false)],
            Direction::Publisher,
        )];
        let subs = vec![te(&[("shm", true), ("tcp", true)], Direction::Subscriber)];
        let (common, advertised) = intersect_active_transports(&pubs, &subs);
        assert_eq!(common, vec!["shm"]);
        assert!(advertised);

        // Disjoint active layers → empty intersection but both advertised
        // (this is the "data won't flow" case the diagnostic flags).
        let pubs2 = vec![te(&[("udp_mc", true)], Direction::Publisher)];
        let subs2 = vec![te(&[("shm", true)], Direction::Subscriber)];
        let (common2, advertised2) = intersect_active_transports(&pubs2, &subs2);
        assert!(common2.is_empty());
        assert!(advertised2);

        // Subscriber side hasn't reported any active layer yet (common
        // during the first registration cycle): we must NOT claim "no shared
        // layer" — that's the false-positive we explicitly fixed.
        let pubs3 = vec![te(&[("shm", true)], Direction::Publisher)];
        let subs3 = vec![te(
            &[("shm", false), ("udp_mc", false)],
            Direction::Subscriber,
        )];
        let (common3, advertised3) = intersect_active_transports(&pubs3, &subs3);
        assert!(common3.is_empty());
        assert!(
            !advertised3,
            "subscriber with no active layers must clear `advertised` \
                 so the diagnostic stays silent"
        );

        // Inactive layers on both sides never count, even if they match.
        let pubs4 = vec![te(&[("shm", false)], Direction::Publisher)];
        let subs4 = vec![te(&[("shm", false)], Direction::Subscriber)];
        let (common4, advertised4) = intersect_active_transports(&pubs4, &subs4);
        assert!(common4.is_empty());
        assert!(!advertised4);
    }

    #[test]
    fn similar_names_basic_typo_and_edges() {
        let pool = [
            "robot/imu",
            "robot/lidar",
            "robot/camera",
            "completely_unrelated",
        ];

        let hits = similar_names("robot/img", pool.iter().copied(), 3);
        assert!(
            hits.contains(&"robot/imu".to_string()),
            "typo `robot/img` should suggest `robot/imu`: got {hits:?}"
        );

        let exact = similar_names("robot/imu", pool.iter().copied(), 5);
        assert!(
            !exact.contains(&"robot/imu".to_string()),
            "exact match leaked into suggestions: {exact:?}"
        );

        assert!(similar_names("", pool.iter().copied(), 5).is_empty());
        assert!(similar_names("ab", pool.iter().copied(), 5).is_empty());

        let pool2 = ["robot/imu_a", "robot/imu_b", "robot/imu_c", "robot/imu_d"];
        let limited = similar_names("robot/imu", pool2.iter().copied(), 2);
        assert_eq!(limited.len(), 2);

        let none = similar_names(
            "robot/imu",
            ["xyz_stuff", "qqq_unrelated"].iter().copied(),
            5,
        );
        assert!(none.is_empty(), "expected no suggestions, got {none:?}");
    }

    #[test]
    fn build_hosts_aggregates_counts_and_domains() {
        let pubs = vec![{
            let mut e = te(&[], Direction::Publisher);
            e.host_name = "alpha".into();
            e.shm_transport_domain = "alpha".into();
            e
        }];
        let subs = vec![{
            let mut e = te(&[], Direction::Subscriber);
            e.host_name = "beta".into();
            e.shm_transport_domain = "beta".into();
            e
        }];
        let services = vec![ServiceEntry {
            service_name: "svc".into(),
            service_id: 1,
            host_name: "alpha".into(),
            process_id: 0,
            process_name: String::new(),
            unit_name: String::new(),
            version: 1,
            tcp_port_v0: 0,
            tcp_port_v1: 0,
            methods: vec![],
        }];
        let clients: Vec<ClientEntry> = vec![];
        let processes = vec![ProcessEntry {
            host_name: "alpha".into(),
            process_id: 0,
            process_name: String::new(),
            unit_name: String::new(),
            state_severity: 0,
            state_severity_level: 1,
            state_info: String::new(),
            runtime_version: "6.1.1".into(),
            config_file_path: String::new(),
        }];
        let hosts = build_hosts(&pubs, &subs, &services, &clients, &processes, &[]);
        let alpha = hosts.iter().find(|h| h.host_name == "alpha").unwrap();
        assert_eq!(alpha.publisher_count, 1);
        assert_eq!(alpha.subscriber_count, 0);
        assert_eq!(alpha.server_count, 1);
        assert_eq!(alpha.process_count, 1);
        assert_eq!(alpha.runtime_versions, vec!["6.1.1".to_string()]);
        let beta = hosts.iter().find(|h| h.host_name == "beta").unwrap();
        assert_eq!(beta.subscriber_count, 1);
        assert_eq!(beta.process_count, 0);
    }

    #[test]
    fn delta_direction_signs() {
        let r = measure_topic_no_io(2.0, Some(1.0));
        assert_eq!(r.delta_direction.unwrap(), DeltaDirection::AboveSpec);
        let r = measure_topic_no_io(0.5, Some(1.0));
        assert_eq!(r.delta_direction.unwrap(), DeltaDirection::BelowSpec);
        let r = measure_topic_no_io(1.0, Some(1.0));
        assert_eq!(r.delta_direction.unwrap(), DeltaDirection::OnSpec);
    }

    /// Helper: build a TopicStatsResult by hand to exercise `delta_direction` without spinning up
    /// an eCAL subscriber. Mirrors `measure_topic`'s branch.
    fn measure_topic_no_io(actual: f64, expected: Option<f64>) -> TopicStatsResult {
        let tol = DEFAULT_TOLERANCE_PCT;
        let (delta_hz, ratio, meets, dir) = compute_spec_check(expected, Some(actual), None);
        TopicStatsResult {
            topic_name: String::new(),
            duration_ms: 0,
            samples_observed: 0,
            wire_data_frequency_hz: Some(actual),
            total_bytes: 0,
            min_size_bytes: None,
            mean_size_bytes: None,
            max_size_bytes: None,
            min_gap_us: None,
            mean_gap_us: None,
            max_gap_us: None,
            gap_stddev_us: None,
            jitter_pct: None,
            first_timestamp_us: None,
            last_timestamp_us: None,
            type_name: None,
            encoding: None,
            expected_hz: expected,
            delta_hz,
            delta_direction: dir,
            ratio_to_target: ratio,
            meets_spec: meets,
            tolerance_pct: expected.map(|_| tol),
            publishers: None,
        }
    }

    #[test]
    fn descriptor_fingerprint_is_first_16_hex_of_sha256() {
        let info = DataTypeInfo {
            type_name: "Foo".into(),
            encoding: "proto".into(),
            descriptor: vec![1, 2, 3, 4],
        };
        let dt = datatype_information(info, false);
        assert_eq!(dt.descriptor_len, 4);
        let fp = dt.descriptor_fingerprint.expect("fingerprint must be set");
        assert_eq!(fp.len(), 16);
        assert!(fp.chars().all(|c| c.is_ascii_hexdigit()));

        let empty = DataTypeInfo {
            type_name: String::new(),
            encoding: String::new(),
            descriptor: vec![],
        };
        let dt = datatype_information(empty, false);
        assert!(dt.descriptor_fingerprint.is_none());
    }

    #[test]
    fn finding_severity_table_and_sort_order() {
        assert_eq!(
            severity_for(FindingCode::ShmDomainSplit),
            FindingSeverity::Warning
        );
        assert_eq!(
            severity_for(FindingCode::NoPublisher),
            FindingSeverity::Error
        );
        assert_eq!(
            severity_for(FindingCode::MultiplePublishers),
            FindingSeverity::Warning
        );
        assert_eq!(
            severity_for(FindingCode::DidYouMean),
            FindingSeverity::Info
        );

        // Sort is code_rank-primary (likely-root-cause), severity
        // tiebreaker. A load-bearing `multiple_publishers` (warning)
        // outranks an advisory `did_you_mean` (info) regardless of severity.
        let mut findings = vec![
            Finding {
                code: FindingCode::DidYouMean,
                severity: FindingSeverity::Info,
                message: "info".into(),
                detail: None,
            },
            Finding {
                code: FindingCode::MultiplePublishers,
                severity: FindingSeverity::Warning,
                message: "warn".into(),
                detail: None,
            },
            Finding {
                code: FindingCode::NoPublisher,
                severity: FindingSeverity::Error,
                message: "err".into(),
                detail: None,
            },
        ];
        findings.sort_by(|a, b| {
            code_rank(a.code)
                .cmp(&code_rank(b.code))
                .then(severity_rank(b.severity).cmp(&severity_rank(a.severity)))
        });
        assert!(matches!(findings[0].code, FindingCode::NoPublisher));
        assert!(matches!(findings[1].code, FindingCode::MultiplePublishers));
        assert!(matches!(findings[2].code, FindingCode::DidYouMean));
    }

    // ------- Test helpers for raw rustecal monitoring types -------

    fn raw_process(host: &str, pid: i32, severity: i32, shm_domain: &str) -> RawProcessInfo {
        RawProcessInfo {
            registration_clock: 1,
            host_name: host.to_string(),
            shm_transport_domain: shm_domain.to_string(),
            process_id: pid,
            process_name: format!("/proc/{pid}"),
            unit_name: format!("unit_{pid}"),
            process_parameter: String::new(),
            state_severity: severity,
            state_severity_level: 1,
            state_info: String::new(),
            time_sync_state: 0,
            time_sync_module_name: String::new(),
            component_init_state: 0,
            component_init_info: String::new(),
            runtime_version: "6.1.1".into(),
            config_file_path: String::new(),
        }
    }

    fn raw_topic(host: &str, pid: i32, topic_name: &str, topic_id: i64, hz_mhz: i32) -> RawTopicInfo {
        RawTopicInfo {
            registration_clock: 1,
            host_name: host.to_string(),
            shm_transport_domain: host.to_string(),
            process_id: pid,
            process_name: format!("/proc/{pid}"),
            unit_name: format!("unit_{pid}"),
            topic_id,
            topic_name: topic_name.to_string(),
            direction: "publisher".into(),
            data_type: rustecal_core::types::DataTypeInfo {
                type_name: "T".into(),
                encoding: "proto".into(),
                descriptor: vec![],
            },
            transport_layers: vec![],
            topic_size: 0,
            connections_local: 0,
            connections_external: 0,
            message_drops: 0,
            data_id: 0,
            data_clock: 0,
            data_frequency: hz_mhz,
        }
    }

    type RawProcessInfo = rustecal_core::core_types::monitoring::ProcessInfo;
    type RawTopicInfo = rustecal_core::core_types::monitoring::TopicInfo;
    type RawSnapshot = rustecal_core::core_types::monitoring::MonitoringSnapshot;

    // ----- U1 / U2: compute_spec_check tolerance boundary -----

    #[test]
    fn compute_spec_check_at_tolerance_boundary() {
        // Exact 10% boundary (default tolerance) is on-spec.
        let (delta, ratio, meets, dir) = compute_spec_check(Some(10.0), Some(11.0), None);
        assert_eq!(delta, Some(1.0));
        assert!((ratio.unwrap() - 1.1).abs() < 1e-9);
        assert_eq!(meets, Some(true));
        assert_eq!(dir, Some(DeltaDirection::OnSpec));

        // Just outside flips to AboveSpec.
        let (_, _, meets, dir) = compute_spec_check(Some(10.0), Some(11.001), None);
        assert_eq!(meets, Some(false));
        assert_eq!(dir, Some(DeltaDirection::AboveSpec));

        // Symmetric below.
        let (_, _, meets, dir) = compute_spec_check(Some(10.0), Some(8.999), None);
        assert_eq!(meets, Some(false));
        assert_eq!(dir, Some(DeltaDirection::BelowSpec));
    }

    #[test]
    fn compute_spec_check_none_when_expected_missing() {
        let (delta, ratio, meets, dir) = compute_spec_check(None, Some(5.0), None);
        assert!(delta.is_none() && ratio.is_none() && meets.is_none() && dir.is_none());

        // expected==0 also yields no spec check (guard against div-by-zero).
        let (delta, ratio, meets, dir) = compute_spec_check(Some(0.0), Some(5.0), None);
        assert!(delta.is_none() && ratio.is_none() && meets.is_none() && dir.is_none());
    }

    // ----- U3: build_hosts picks up shm_transport_domain from raw_processes only -----

    #[test]
    fn build_hosts_includes_shm_domain_from_raw_process_only() {
        // A host with NO pubs/subs/services but a process — its
        // shm_transport_domain should still surface via the
        // raw_processes pass.
        let raw = vec![raw_process("gamma", 42, 0, "gamma_shm_dom")];
        let processes = vec![ProcessEntry {
            host_name: "gamma".into(),
            process_id: 42,
            process_name: "/proc/42".into(),
            unit_name: "unit_42".into(),
            state_severity: 0,
            state_severity_level: 1,
            state_info: String::new(),
            runtime_version: "6.1.1".into(),
            config_file_path: String::new(),
        }];
        let hosts = build_hosts(&[], &[], &[], &[], &processes, &raw);
        let g = hosts.iter().find(|h| h.host_name == "gamma").unwrap();
        assert_eq!(g.shm_transport_domains, vec!["gamma_shm_dom".to_string()]);
        assert_eq!(g.process_count, 1);
    }

    // ----- U4: similar_names tiebreaks by len_diff -----

    #[test]
    fn similar_names_tiebreaks_by_length_diff() {
        // All three candidates contain every trigram of `robot/imu`
        // (`rob`, `obo`, `bot`, `ot/`, `t/i`, `/im`, `imu`), so the
        // hits-count component of the score is equal. The
        // closest-length candidate must rank first via the
        // -len_diff tiebreaker.
        let pool = [
            "robot/imuABC",         // len_diff = 3
            "robot/imuABCDEF",      // len_diff = 6
            "robot/imuABCDEFGHIJ",  // len_diff = 10
        ];
        let hits = similar_names("robot/imu", pool.iter().copied(), 3);
        assert_eq!(hits.first().map(String::as_str), Some("robot/imuABC"));
    }

    // ----- U5: collect_echoed_publishers thresholds at 2 -----

    #[test]
    fn collect_echoed_publishers_none_below_two_some_at_two_or_more() {
        let snap_one = RawSnapshot {
            processes: vec![],
            publishers: vec![raw_topic("h1", 1, "/t", 100, 10_000)],
            subscribers: vec![],
            servers: vec![],
            clients: vec![],
        };
        assert!(collect_echoed_publishers(&snap_one, "/t").is_none());

        let snap_two = RawSnapshot {
            processes: vec![],
            publishers: vec![
                raw_topic("h1", 1, "/t", 100, 10_000),
                raw_topic("h2", 2, "/t", 200, 20_000),
            ],
            subscribers: vec![],
            servers: vec![],
            clients: vec![],
        };
        let echoed = collect_echoed_publishers(&snap_two, "/t").expect("≥2 ⇒ Some");
        assert_eq!(echoed.len(), 2);
        // data_frequency is mHz; we surface Hz.
        assert!((echoed[0].registered_data_frequency_hz - 10.0).abs() < 1e-9);
        assert!((echoed[1].registered_data_frequency_hz - 20.0).abs() < 1e-9);
    }

    // ----- U6: did_you_mean with empty pool -----

    #[test]
    fn did_you_mean_empty_pool_returns_empty_candidates_and_null_suggestion() {
        let snap = RawSnapshot {
            processes: vec![],
            publishers: vec![],
            subscribers: vec![],
            servers: vec![],
            clients: vec![],
        };
        let pool: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        let detail = build_did_you_mean("/totally_bogus", &pool, &snap, pool.iter())
            .expect("did_you_mean always emits a detail object");
        let obj = detail.as_object().unwrap();
        assert!(obj.get("suggestion").unwrap().is_null());
        assert_eq!(obj.get("candidates").unwrap().as_array().unwrap().len(), 0);
    }

    // ----- U7: unhealthy_process finding for severity ≥ 2 -----

    #[test]
    fn unhealthy_process_finding_emitted_for_severity_ge_2() {
        let proc_state = raw_process("h1", 7, 3, "h1");
        let mut topic = te(&[], Direction::Publisher);
        topic.host_name = "h1".into();
        topic.process_id = 7;
        topic.unit_name = "unit_7".into();
        topic.topic_id = 999;

        let result = build_unhealthy_process_finding("publisher", &topic, &proc_state)
            .expect("severity 3 must produce a finding");
        let (code, msg, detail) = result;
        assert!(matches!(code, FindingCode::UnhealthyProcess));
        assert!(msg.contains("non-healthy"));
        let d = detail.expect("detail required").as_object().cloned().unwrap();
        for key in [
            "side",
            "host_name",
            "unit_name",
            "process_id",
            "topic_id",
            "state_severity",
            "state_severity_level",
            "state_info",
        ] {
            assert!(d.contains_key(key), "detail missing {key:?}: {d:?}");
        }
        assert_eq!(d["side"], serde_json::json!("publisher"));
        assert_eq!(d["topic_id"], serde_json::json!(999));
        assert_eq!(d["state_severity"], serde_json::json!(3));

        // severity 1 (healthy) ⇒ no finding.
        let healthy = raw_process("h1", 7, 1, "h1");
        assert!(build_unhealthy_process_finding("publisher", &topic, &healthy).is_none());
    }
}
