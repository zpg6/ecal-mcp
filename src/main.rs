//! Local MCP server that exposes Eclipse eCAL pub/sub & service introspection
//! and interaction over stdio, built on top of `rustecal` and `rmcp`.

use std::borrow::Cow;
use std::ffi::CStr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use parking_lot::Mutex;
use rmcp::{
    handler::server::wrapper::{Json, Parameters},
    model::{Implementation, ServerCapabilities, ServerInfo},
    schemars, tool, tool_handler, tool_router, ErrorData as McpError, ServerHandler, ServiceExt,
};
use rustecal::pubsub::publisher::Timestamp;
use rustecal::{
    Ecal, EcalComponents, PublisherMessage, ServiceClient, ServiceRequest, TypedPublisher,
    TypedSubscriber,
};
use rustecal_core::core_types::monitoring::TransportLayerType;
use rustecal_core::log_level::LogLevel;
use rustecal_types_bytes::BytesMessage;
use rustecal_types_string::StringMessage;
use serde::{Deserialize, Serialize};
use tracing_subscriber::EnvFilter;

// ---------------------------------------------------------------------------
// Tool argument & response types
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, schemars::JsonSchema)]
struct DataType {
    type_name: String,
    encoding: String,
    /// Length of the type descriptor blob (e.g. a serialized FileDescriptorSet
    /// for protobuf topics). Always present, cheap.
    descriptor_len: usize,
    /// The descriptor blob itself, base-64 encoded. Only included when
    /// `include_descriptors=true` is passed to the listing tool, since
    /// descriptors can be tens of KB per topic.
    #[serde(skip_serializing_if = "Option::is_none")]
    descriptor_base64: Option<String>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
struct TransportLayerEntry {
    /// One of: "udp_multicast", "shm", "tcp", "none", or "unknown:<n>". Note
    /// that the eCAL C++ enum spells UDP multicast as `udp_mc`; we use the
    /// long form here for readability — semantically equivalent.
    kind: String,
    version: i32,
    /// `true` when this endpoint has *actually transmitted (publisher) or
    /// received (subscriber) at least one sample on this layer*. This is
    /// post-facto, not "this layer is configured/available", so a freshly
    /// joined publisher with no traffic yet may show all layers as inactive.
    /// See eCAL's `STransportLayer` documentation for the definition.
    active: bool,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
struct TopicEntry {
    topic_name: String,
    direction: String,
    data_type: DataType,
    host_name: String,
    /// eCAL "shm transport domain" — defaults to `host_name`. Only matters
    /// for **cross-host-name** endpoints (e.g. two containers presenting
    /// different hostnames on the same Linux host): in that case both must
    /// agree on this string for SHM transport to be eligible. Same-host /
    /// same-hostname endpoints share SHM regardless of this value.
    shm_transport_domain: String,
    process_id: i32,
    process_name: String,
    unit_name: String,
    topic_id: i64,
    topic_size: i32,
    connections_local: i32,
    connections_external: i32,
    message_drops: i32,
    /// Raw eCAL `data_frequency` value, in **millihertz** (samples / 1000s).
    /// e.g. 1000 = 1 Hz, 10_000 = 10 Hz. See `data_frequency_hz` for a
    /// human-friendly conversion.
    data_frequency_millihertz: i32,
    /// `data_frequency_millihertz / 1000`, i.e. samples per second.
    data_frequency_hz: f64,
    /// Per-message counter (`eCAL_Monitoring_STopic.data_id`). Increments on
    /// every send; useful as an "is the publisher still moving?" liveness
    /// indicator independent of `data_frequency_hz`.
    data_id: i64,
    /// Internal eCAL clock at the last sample (`data_clock`). Combined with
    /// `data_id`, lets you tell whether two consecutive monitoring snapshots
    /// saw new traffic.
    data_clock: i64,
    /// Number of registration heartbeats the producer has emitted since
    /// `Ecal::initialize`. Cheap freshness signal — values increasing across
    /// snapshots ⇒ the producer is alive; flat ⇒ may have crashed even
    /// though it's still in the snapshot until the registration timeout.
    registration_clock: i32,
    /// Transport layers this endpoint has used. See `TransportLayerEntry`
    /// for the meaning of `active`.
    transport_layers: Vec<TransportLayerEntry>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
struct MethodEntry {
    method_name: String,
    request_type: String,
    response_type: String,
    call_count: i64,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
struct ServiceEntry {
    service_name: String,
    service_id: i64,
    host_name: String,
    process_id: i32,
    process_name: String,
    unit_name: String,
    methods: Vec<MethodEntry>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
struct ClientEntry {
    service_name: String,
    service_id: i64,
    host_name: String,
    process_id: i32,
    process_name: String,
    unit_name: String,
    methods: Vec<MethodEntry>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
struct ServiceClientsResult {
    clients: Vec<ClientEntry>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
struct LogEntry {
    /// Severity level (e.g. "Info", "Warning", "Error", "Fatal", "Debug1"…).
    level: String,
    /// Microseconds since epoch.
    timestamp_us: i64,
    host_name: String,
    process_name: String,
    process_id: i32,
    unit_name: String,
    content: String,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
struct LogsResult {
    logs: Vec<LogEntry>,
}

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
struct GetLogsArgs {
    /// Minimum severity to include, ordered verbose → fatal:
    /// `debug4 < debug3 < debug2 < debug1 < info < warning < error < fatal`.
    /// Defaults to `info` (i.e. drop debug noise). Pass `all` (or omit + a
    /// `since_timestamp_us`) to include debug entries.
    #[serde(default)]
    min_level: Option<String>,
    /// Drop entries with `timestamp_us` strictly less than this. Useful for
    /// "only show me logs since I started watching".
    #[serde(default)]
    since_timestamp_us: Option<i64>,
    /// Optional case-insensitive substring filter on the emitting process
    /// name.
    #[serde(default)]
    process_name_pattern: Option<String>,
}

/// Severity *priority* used for `min_level` filtering. eCAL's numeric
/// LogLevel values are bit flags (`Info=1, Debug1=16, Fatal=8, …`) which do
/// **not** form a usable >= ordering, so we project them onto an ordinal
/// scale here (verbose at the bottom, fatal at the top):
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

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct TopicStatsArgs {
    /// eCAL topic to measure.
    topic: String,
    /// Window in milliseconds. Defaults to 2000. Capped at 60000.
    #[serde(default)]
    duration_ms: Option<u64>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
struct TopicStatsResult {
    topic: String,
    duration_ms: u64,
    samples_observed: u64,
    /// Computed from `samples_observed / (duration_ms / 1000)`. This is the
    /// *measured* rate over the window, not the cached `data_frequency` from
    /// the monitoring snapshot. Note the denominator is the full wall-clock
    /// window, including any pre-discovery dead time before the first
    /// sample arrived — for slow topics this under-reports the steady-state
    /// rate. Cross-check with `mean_gap_us` (≈ 1e6 / steady-state Hz).
    observed_hz: f64,
    total_bytes: u64,
    /// Min / mean / max payload size in bytes over the window. `None` when
    /// no samples were observed.
    min_size_bytes: Option<usize>,
    mean_size_bytes: Option<f64>,
    max_size_bytes: Option<usize>,
    /// Inter-arrival gap (microseconds) between consecutive samples.
    /// Requires at least 2 samples; otherwise all `None`.
    min_gap_us: Option<i64>,
    mean_gap_us: Option<f64>,
    max_gap_us: Option<i64>,
    /// Standard deviation of the gaps. Useful as a "is this stream jittery?"
    /// signal. Requires at least 2 samples.
    gap_stddev_us: Option<f64>,
    first_timestamp_us: Option<i64>,
    last_timestamp_us: Option<i64>,
    /// Type/encoding observed on the wire (from the publisher's side). May
    /// differ from the monitoring snapshot if the publisher is mid-restart.
    type_name: Option<String>,
    encoding: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct DiagnoseTopicArgs {
    topic: String,
    /// How long to sample the topic for live rate/stats, in milliseconds.
    /// Defaults to 1500. Set to 0 to skip the live stats step (returns only
    /// the static monitoring view).
    #[serde(default)]
    duration_ms: Option<u64>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
struct DiagnoseTopicResult {
    topic: String,
    publishers: Vec<TopicEntry>,
    subscribers: Vec<TopicEntry>,
    /// Distinct `(type_name, encoding)` tuples seen across publishers AND
    /// subscribers. If this has more than one entry, you have a type
    /// mismatch — the canonical reason for "subscriber sees nothing".
    type_signatures: Vec<TypeSignature>,
    /// Active transport layers shared by *all* publishers and *all*
    /// subscribers on this topic. If this is empty while both pubs and subs
    /// exist, no layer is mutually agreed and data won't flow.
    common_active_transports: Vec<String>,
    /// Distinct `shm_transport_domain` values seen across pubs+subs. More
    /// than one means at least some endpoints can't share SHM.
    shm_transport_domains: Vec<String>,
    /// Highest-signal flags: ordered list of strings describing problems
    /// the diagnose tool detected. Empty list ⇒ no obvious issues.
    findings: Vec<String>,
    /// When the requested topic has zero matching pubs/subs, this lists up
    /// to 10 known topic names that share the most prefix characters with
    /// the request. Useful for catching typos / wrong namespaces.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    similar_topics: Vec<String>,
    /// Optional live measurement (omitted when `duration_ms=0`).
    #[serde(skip_serializing_if = "Option::is_none")]
    live_stats: Option<TopicStatsResult>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
struct TypeSignature {
    type_name: String,
    encoding: String,
    /// Number of endpoints (pubs+subs) that advertised this signature.
    occurrences: usize,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
struct PublishersResult {
    publishers: Vec<TopicEntry>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
struct SubscribersResult {
    subscribers: Vec<TopicEntry>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
struct ServicesResult {
    services: Vec<ServiceEntry>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
struct ProcessesResult {
    processes: Vec<ProcessEntry>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
struct ProcessEntry {
    host_name: String,
    process_id: i32,
    process_name: String,
    unit_name: String,
    /// eCAL `state_severity` enum value. Anything > 0 means the process has
    /// reported a non-healthy state (warning/critical/failed); see
    /// `state_severity_level` for the integer rank.
    state_severity: i32,
    state_severity_level: i32,
    state_info: String,
    runtime_version: String,
    /// Path to the `ecal.yaml` this process loaded (per its registration).
    /// Empty when the process started with the built-in defaults.
    config_file_path: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct PublishArgs {
    /// eCAL topic to publish on.
    topic: String,
    /// UTF-8 text payload. Mutually exclusive with `payload_base64`.
    #[serde(default)]
    text: Option<String>,
    /// Base-64 encoded raw bytes payload. Mutually exclusive with `text`.
    #[serde(default)]
    payload_base64: Option<String>,
    /// Milliseconds to wait after creating the publisher so subscribers can
    /// finish discovery before the message is sent. Defaults to 500.
    #[serde(default)]
    discovery_wait_ms: Option<u64>,
    /// Number of times to send the message (useful when subscribers may be
    /// slow to discover the publisher). Defaults to 1.
    #[serde(default)]
    repeat: Option<u32>,
    /// Delay in milliseconds between repeated sends. Defaults to 100.
    #[serde(default)]
    repeat_interval_ms: Option<u64>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
struct PublishResult {
    topic: String,
    bytes_sent: usize,
    sends_succeeded: u32,
    sends_attempted: u32,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct SubscribeArgs {
    /// eCAL topic to subscribe to.
    topic: String,
    /// How long to listen, in milliseconds. Defaults to 1000.
    #[serde(default)]
    duration_ms: Option<u64>,
    /// Maximum samples to return. Defaults to 10.
    #[serde(default)]
    max_samples: Option<usize>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
struct Sample {
    /// Topic the sample was received on (matches the subscribed topic, but
    /// useful when the same topic is fanned out via multiple publishers).
    topic_name: String,
    /// Send timestamp (microseconds since epoch).
    timestamp_us: i64,
    /// Publisher logical clock at send time.
    clock: i64,
    /// Payload size in bytes.
    size: usize,
    /// UTF-8 decoded payload, present only when the bytes are valid UTF-8.
    text: Option<String>,
    /// Base-64 encoded payload bytes (always present).
    payload_base64: String,
    encoding: String,
    type_name: String,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
struct SubscribeResult {
    topic: String,
    duration_ms: u64,
    samples_collected: usize,
    samples_dropped: usize,
    samples: Vec<Sample>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct CallServiceArgs {
    /// eCAL service name.
    service: String,
    /// Method name on the service.
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
    /// Milliseconds to wait for at least one server instance to be discovered
    /// before issuing the call. Defaults to 1000.
    #[serde(default)]
    discovery_wait_ms: Option<u64>,
    /// When set, only call the server instance whose `eCAL_SEntityId.entity_id`
    /// matches this value. Useful when you want to drive exactly one of N
    /// replicas. If no instance matches the filter, the call returns
    /// `instances: 0` rather than an error.
    #[serde(default)]
    target_server_entity_id: Option<u64>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
struct ServiceCallResponse {
    instance_index: usize,
    success: bool,
    error: Option<String>,
    /// Identity of the server instance that produced this response. Populated
    /// from `eCAL_SServiceResponse::server_id` (= the server's `eCAL_SEntityId`).
    server_entity_id: u64,
    server_process_id: i32,
    server_host_name: String,
    response_size: usize,
    response_text: Option<String>,
    response_base64: String,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
struct CallServiceResult {
    service: String,
    method: String,
    /// Total number of server instances eCAL discovered for this service
    /// before any `target_server_entity_id` filtering.
    discovered_instances: usize,
    /// Number of entries in `responses` (= `discovered_instances` unless a
    /// `target_server_entity_id` filter is in effect).
    instances: usize,
    responses: Vec<ServiceCallResponse>,
}

// ---------------------------------------------------------------------------
// MCP server
// ---------------------------------------------------------------------------

#[derive(Clone, Default)]
struct EcalServer;

fn snapshot() -> Result<rustecal_core::core_types::monitoring::MonitoringSnapshot, McpError> {
    rustecal_core::monitoring::Monitoring::get_snapshot().map_err(|e| {
        McpError::internal_error(format!("eCAL monitoring snapshot failed: {e:?}"), None)
    })
}

fn transport_kind(t: &TransportLayerType) -> String {
    match t {
        TransportLayerType::None => "none".into(),
        TransportLayerType::UdpMulticast => "udp_multicast".into(),
        TransportLayerType::Shm => "shm".into(),
        TransportLayerType::Tcp => "tcp".into(),
        TransportLayerType::Unknown(n) => format!("unknown:{n}"),
    }
}

fn topic_entry(
    t: rustecal_core::core_types::monitoring::TopicInfo,
    include_descriptors: bool,
) -> TopicEntry {
    let descriptor_len = t.data_type.descriptor.len();
    let descriptor_base64 = if include_descriptors && descriptor_len > 0 {
        Some(B64.encode(&t.data_type.descriptor))
    } else {
        None
    };
    let transport_layers = t
        .transport_layers
        .iter()
        .map(|l| TransportLayerEntry {
            kind: transport_kind(&l.transport_type),
            version: l.version,
            active: l.active,
        })
        .collect();
    TopicEntry {
        topic_name: t.topic_name,
        direction: t.direction,
        data_type: DataType {
            type_name: t.data_type.type_name,
            encoding: t.data_type.encoding,
            descriptor_len,
            descriptor_base64,
        },
        host_name: t.host_name,
        shm_transport_domain: t.shm_transport_domain,
        process_id: t.process_id,
        process_name: t.process_name,
        unit_name: t.unit_name,
        topic_id: t.topic_id,
        topic_size: t.topic_size,
        connections_local: t.connections_local,
        connections_external: t.connections_external,
        message_drops: t.message_drops,
        data_frequency_millihertz: t.data_frequency,
        data_frequency_hz: f64::from(t.data_frequency) / 1000.0,
        data_id: t.data_id,
        data_clock: t.data_clock,
        registration_clock: t.registration_clock,
        transport_layers,
    }
}

/// Lightweight subscribe-and-measure used by `ecal_topic_stats` and
/// `ecal_diagnose_topic`. Records per-sample timestamp + size only (no
/// payload retained) so it's cheap on high-rate topics.
fn measure_topic(topic: &str, duration_ms: u64) -> Result<TopicStatsResult, String> {
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
    let observed_hz = if duration_ms == 0 {
        0.0
    } else {
        n as f64 / (duration_ms as f64 / 1000.0)
    };

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

    Ok(TopicStatsResult {
        topic: topic.to_string(),
        duration_ms,
        samples_observed: n,
        observed_hz,
        total_bytes,
        min_size_bytes,
        mean_size_bytes,
        max_size_bytes,
        min_gap_us,
        mean_gap_us,
        max_gap_us,
        gap_stddev_us,
        first_timestamp_us: g.timestamps.iter().min().copied(),
        last_timestamp_us: g.timestamps.iter().max().copied(),
        type_name: g.type_name,
        encoding: g.encoding,
    })
}

/// Set-intersection of active transport "kind" strings across all entries on
/// each side. Returns `(intersection, side_advertised)`:
/// - `intersection`: the layers active on both pub and sub sides
/// - `side_advertised`: `true` only when at least one publisher *and* at
///   least one subscriber advertised a non-empty active transport set; eCAL
///   sometimes leaves subscriber transport_layers empty in monitoring
///   snapshots, in which case "no shared transport" is not a real finding.
fn intersect_active_transports(pubs: &[TopicEntry], subs: &[TopicEntry]) -> (Vec<String>, bool) {
    fn merge_active(side: &[TopicEntry]) -> std::collections::BTreeSet<String> {
        let mut acc: std::collections::BTreeSet<String> = Default::default();
        for e in side {
            for l in &e.transport_layers {
                if l.active {
                    acc.insert(l.kind.clone());
                }
            }
        }
        acc
    }
    let p = merge_active(pubs);
    let s = merge_active(subs);
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

/// Return up to `limit` names from `candidates` that look most like
/// `target`, ordered best-first. Cheap heuristic: prefer names whose
/// case-insensitive containment of any 3+ char substring of the target
/// scores highest, then prefer names with smaller absolute length
/// difference. Good enough to catch typos / wrong namespace prefixes.
fn similar_names<'a>(
    target: &str,
    candidates: impl Iterator<Item = &'a str>,
    limit: usize,
) -> Vec<String> {
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
            // Score: 3-gram hits dominate, then -|length difference|.
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
    scored.into_iter().take(limit).map(|(_, n)| n).collect()
}

/// Substring (case-insensitive) match. We deliberately avoid regex to keep
/// the surface area minimal — eCAL topic names are flat strings, and
/// case-insensitive substring covers the 95% case ("show me everything with
/// 'imu' in the name").
fn name_matches(haystack: &str, pattern: &Option<String>) -> bool {
    match pattern.as_deref() {
        None | Some("") => true,
        Some(p) => haystack
            .to_ascii_lowercase()
            .contains(&p.to_ascii_lowercase()),
    }
}

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
struct ListFilter {
    /// Optional case-insensitive substring filter on the entity name
    /// (`topic_name` for pub/sub, `service_name` for services/clients,
    /// `unit_name` or `process_name` for processes). Omit or pass `""` for
    /// "no filter".
    #[serde(default)]
    name_pattern: Option<String>,
    /// Optional case-insensitive substring filter on `data_type.type_name`.
    /// Mirrors the `--find` flag of `ecal_mon_cli`. Has no effect on
    /// service/process listings (services use method-level types).
    #[serde(default)]
    type_name_pattern: Option<String>,
    /// When true, include the binary type descriptor blob (base-64) in each
    /// `data_type` entry. Off by default because descriptors can be tens of
    /// KB per topic and you usually don't want them on a "give me everything"
    /// scan. Has no effect for service/process listings.
    #[serde(default)]
    include_descriptors: Option<bool>,
}

#[tool_router]
impl EcalServer {
    pub fn new() -> Self {
        Self
    }

    #[tool(
        description = "List eCAL publishers currently visible on the network. \
        Returns topic name, data type, transport layers, host, process, and \
        traffic stats. Pass `name_pattern` to substring-filter by topic, and \
        `include_descriptors=true` to attach base-64 type descriptor blobs \
        (off by default; can be tens of KB each)."
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
            .map(|t| topic_entry(t, include))
            .collect();
        Ok(Json(PublishersResult { publishers }))
    }

    #[tool(
        description = "List eCAL subscribers currently visible on the network. \
        Same shape and filtering as `ecal_list_publishers`."
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
            .map(|t| topic_entry(t, include))
            .collect();
        Ok(Json(SubscribersResult { subscribers }))
    }

    #[tool(
        description = "List eCAL service servers visible on the network, including each \
        method's request/response types. Pass `name_pattern` to substring-filter by \
        service name."
    )]
    async fn ecal_list_services(
        &self,
        Parameters(args): Parameters<ListFilter>,
    ) -> Result<Json<ServicesResult>, McpError> {
        let snap = snapshot()?;
        let services = snap
            .servers
            .into_iter()
            .filter(|s| name_matches(&s.service_name, &args.name_pattern))
            .map(|s| ServiceEntry {
                service_name: s.service_name,
                service_id: s.service_id,
                host_name: s.host_name,
                process_id: s.process_id,
                process_name: s.process_name,
                unit_name: s.unit_name,
                methods: s
                    .methods
                    .into_iter()
                    .map(|m| MethodEntry {
                        method_name: m.method_name,
                        request_type: m.request_type.type_name,
                        response_type: m.response_type.type_name,
                        call_count: m.call_count,
                    })
                    .collect(),
            })
            .collect();
        Ok(Json(ServicesResult { services }))
    }

    #[tool(
        description = "List eCAL service *clients* visible on the network. Useful for \
        discovering who is calling which service. Pass `name_pattern` to substring-filter \
        by service name."
    )]
    async fn ecal_list_service_clients(
        &self,
        Parameters(args): Parameters<ListFilter>,
    ) -> Result<Json<ServiceClientsResult>, McpError> {
        let snap = snapshot()?;
        let clients = snap
            .clients
            .into_iter()
            .filter(|c| name_matches(&c.service_name, &args.name_pattern))
            .map(|c| ClientEntry {
                service_name: c.service_name,
                service_id: c.service_id,
                host_name: c.host_name,
                process_id: c.process_id,
                process_name: c.process_name,
                unit_name: c.unit_name,
                methods: c
                    .methods
                    .into_iter()
                    .map(|m| MethodEntry {
                        method_name: m.method_name,
                        request_type: m.request_type.type_name,
                        response_type: m.response_type.type_name,
                        call_count: m.call_count,
                    })
                    .collect(),
            })
            .collect();
        Ok(Json(ServiceClientsResult { clients }))
    }

    #[tool(
        description = "Drain pending eCAL log messages emitted by all processes since the \
        last call. Calls `eCAL_Logging_GetLogging`, which empties the local log buffer; \
        repeated calls only return new entries. Optional filters: `min_level` (one of \
        info|warning|error|fatal|debug1|debug2|debug3, default info) drops anything \
        below the named severity; `since_timestamp_us` drops anything older; \
        `process_name_pattern` substring-filters by emitting process."
    )]
    async fn ecal_get_logs(
        &self,
        Parameters(args): Parameters<GetLogsArgs>,
    ) -> Result<Json<LogsResult>, McpError> {
        let logs = tokio::task::spawn_blocking(|| rustecal_core::log::Log::get_logging())
            .await
            .map_err(|e| McpError::internal_error(format!("get_logs task join: {e}"), None))?
            .map_err(|e| McpError::internal_error(format!("eCAL log fetch failed: {e:?}"), None))?;

        // Default min_level = info (drop debug noise).
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
        to substring-filter against `unit_name` or `process_name`."
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

    #[tool(description = "Publish a one-shot message to an eCAL topic. \
        Provide either `text` (UTF-8 string, sent as eCAL StringMessage) or `payload_base64` \
        (base-64 encoded bytes, sent as eCAL BytesMessage). Optionally repeat the send to \
        give late-joining subscribers a chance to receive it.")]
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
        let topic = args.topic.clone();

        let result = tokio::task::spawn_blocking(move || -> Result<PublishResult, String> {
            let mut sends_ok: u32 = 0;
            let bytes_sent: usize;

            // Wait for at least one subscriber to be discovered, up to
            // `discovery_wait`. eCAL's `Publisher::Send` returns `false` (and
            // is effectively a no-op) when `GetSubscriberCount() == 0`, so
            // polling here makes the first send actually land instead of
            // silently failing on a fresh publisher.
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
                topic,
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
        samples seen. Payloads are returned as both base-64 bytes and (when valid UTF-8) text."
    )]
    async fn ecal_subscribe(
        &self,
        Parameters(args): Parameters<SubscribeArgs>,
    ) -> Result<Json<SubscribeResult>, McpError> {
        let duration_ms = args.duration_ms.unwrap_or(1000);
        let max_samples = args.max_samples.unwrap_or(10);
        let topic = args.topic.clone();

        let result = tokio::task::spawn_blocking(move || -> Result<SubscribeResult, String> {
            let collected: Arc<Mutex<Vec<Sample>>> = Arc::new(Mutex::new(Vec::new()));
            let dropped: Arc<Mutex<usize>> = Arc::new(Mutex::new(0));

            let mut sub = TypedSubscriber::<BytesMessage>::new(&topic)
                .map_err(|e| format!("create subscriber: {e}"))?;

            {
                let collected = collected.clone();
                let dropped = dropped.clone();
                sub.set_callback(move |received| {
                    let mut guard = collected.lock();
                    if guard.len() >= max_samples {
                        *dropped.lock() += 1;
                        return;
                    }
                    let bytes: &[u8] = received.payload.data.as_ref();
                    let text = std::str::from_utf8(bytes).ok().map(|s| s.to_string());
                    guard.push(Sample {
                        topic_name: received.topic_name,
                        timestamp_us: received.timestamp,
                        clock: received.clock,
                        size: bytes.len(),
                        text,
                        payload_base64: B64.encode(bytes),
                        encoding: received.encoding,
                        type_name: received.type_name,
                    });
                });
            }

            std::thread::sleep(Duration::from_millis(duration_ms));
            drop(sub);

            let samples = std::mem::take(&mut *collected.lock());
            let drops = *dropped.lock();
            Ok(SubscribeResult {
                topic,
                duration_ms,
                samples_collected: samples.len(),
                samples_dropped: drops,
                samples,
            })
        })
        .await
        .map_err(|e| McpError::internal_error(format!("subscribe task join: {e}"), None))?
        .map_err(|e| McpError::internal_error(e, None))?;

        Ok(Json(result))
    }

    #[tool(
        description = "Call an eCAL service method on every discovered server instance. \
        Provide either `text` (UTF-8 string) or `payload_base64` (raw bytes) as the request \
        payload. Returns one entry per server instance."
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
                    instances: 0,
                    responses: Vec::new(),
                });
            }

            let target = args.target_server_entity_id;
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
                            server_entity_id: raw_id.entity_id,
                            server_process_id: raw_id.process_id,
                            server_host_name: host_name,
                            response_size: bytes.len(),
                            response_text: text,
                            response_base64: B64.encode(&bytes),
                        });
                    }
                    None => {
                        if target.is_some() {
                            // Without an instance ID lookup we can't tell
                            // whether the silent instance is the one the
                            // caller targeted; safer to drop than to
                            // pollute the result with a phantom row.
                            continue;
                        }
                        responses.push(ServiceCallResponse {
                            instance_index: responses.len(),
                            success: false,
                            // `instance.call` returns `None` when the underlying
                            // C call fails *or* the configured timeout elapses;
                            // we don't get the underlying eCAL error string.
                            error: Some(format!(
                                "service call returned no response within {timeout_ms} ms \
                                 (timeout, server crash, or transport failure)"
                            )),
                            server_entity_id: 0,
                            server_process_id: 0,
                            server_host_name: String::new(),
                            response_size: 0,
                            response_text: None,
                            response_base64: String::new(),
                        });
                    }
                }
            }

            Ok(CallServiceResult {
                service: service_name,
                method: method_name,
                discovered_instances: discovered,
                instances: responses.len(),
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
        Subscribes for `duration_ms` (default 2000, capped at 60000) and returns observed \
        rate, payload size stats, and inter-arrival jitter. Unlike the cached \
        `data_frequency` field on `ecal_list_publishers`, this is what is actually on the \
        wire right now."
    )]
    async fn ecal_topic_stats(
        &self,
        Parameters(args): Parameters<TopicStatsArgs>,
    ) -> Result<Json<TopicStatsResult>, McpError> {
        let duration_ms = args.duration_ms.unwrap_or(2000).min(60_000);
        let topic = args.topic.clone();

        let result = tokio::task::spawn_blocking(move || -> Result<TopicStatsResult, String> {
            measure_topic(&topic, duration_ms)
        })
        .await
        .map_err(|e| McpError::internal_error(format!("topic_stats task join: {e}"), None))?
        .map_err(|e| McpError::internal_error(e, None))?;

        Ok(Json(result))
    }

    #[tool(
        description = "Diagnose why an eCAL topic may not be flowing. Combines a \
        monitoring snapshot (all matching publishers + subscribers, their type signatures, \
        and per-side transport layers) with an optional live measurement window. Surfaces \
        a `findings` list of detected anomalies: type mismatches, no-publisher / \
        no-subscriber conditions, disjoint transport sets, and SHM-domain splits. This is \
        the first tool to reach for when answering 'why am I not getting data?'."
    )]
    async fn ecal_diagnose_topic(
        &self,
        Parameters(args): Parameters<DiagnoseTopicArgs>,
    ) -> Result<Json<DiagnoseTopicResult>, McpError> {
        let duration_ms = args.duration_ms.unwrap_or(1500).min(60_000);
        let topic = args.topic.clone();

        let snap = snapshot()?;
        let all_topic_names: std::collections::BTreeSet<String> = snap
            .publishers
            .iter()
            .chain(snap.subscribers.iter())
            .map(|t| t.topic_name.clone())
            .collect();
        let pubs: Vec<TopicEntry> = snap
            .publishers
            .iter()
            .filter(|t| t.topic_name == topic)
            .cloned()
            .map(|t| topic_entry(t, false))
            .collect();
        let subs: Vec<TopicEntry> = snap
            .subscribers
            .iter()
            .filter(|t| t.topic_name == topic)
            .cloned()
            .map(|t| topic_entry(t, false))
            .collect();

        let mut sigs: std::collections::BTreeMap<(String, String), usize> = Default::default();
        for e in pubs.iter().chain(subs.iter()) {
            *sigs
                .entry((e.data_type.type_name.clone(), e.data_type.encoding.clone()))
                .or_default() += 1;
        }
        let type_signatures: Vec<TypeSignature> = sigs
            .into_iter()
            .map(|((type_name, encoding), occurrences)| TypeSignature {
                type_name,
                encoding,
                occurrences,
            })
            .collect();

        let (common_active_transports, both_sides_advertised) =
            intersect_active_transports(&pubs, &subs);

        let mut domains: std::collections::BTreeSet<String> = Default::default();
        for e in pubs.iter().chain(subs.iter()) {
            domains.insert(e.shm_transport_domain.clone());
        }
        let shm_transport_domains: Vec<String> = domains.into_iter().collect();

        let mut findings = Vec::new();
        if pubs.is_empty() && subs.is_empty() {
            findings.push("no publishers or subscribers registered for this topic".into());
        } else if pubs.is_empty() {
            findings.push("subscribers exist but no publisher is registered".into());
        } else if subs.is_empty() {
            findings.push("publishers exist but no subscriber is registered".into());
        }
        if type_signatures.len() > 1 {
            // eCAL's transport layer delivers raw bytes regardless of type
            // metadata, so this isn't strictly a "data won't flow" case —
            // it's a "typed consumers will fail to deserialize" case.
            findings.push(format!(
                "metadata mismatch: {} distinct (type_name, encoding) signatures across endpoints \
                 — raw bytes still flow, but typed deserialization will fail on the side(s) that \
                 disagree",
                type_signatures.len()
            ));
        }
        if !pubs.is_empty()
            && !subs.is_empty()
            && both_sides_advertised
            && common_active_transports.is_empty()
        {
            findings.push(
                "publishers and subscribers share no active transport layer; data cannot flow"
                    .into(),
            );
        }
        // SHM-domain mismatch only matters across endpoints with *different
        // host_names*. Same-host endpoints are eligible for SHM regardless
        // of the configured domain (eCAL short-circuits on host equality).
        let mut hosts: std::collections::BTreeSet<&str> = Default::default();
        for e in pubs.iter().chain(subs.iter()) {
            hosts.insert(&e.host_name);
        }
        if hosts.len() > 1 && shm_transport_domains.len() > 1 {
            findings.push(format!(
                "{} hosts with {} distinct shm_transport_domain values: cross-host SHM is only \
                 eligible when all sides agree on the domain string",
                hosts.len(),
                shm_transport_domains.len()
            ));
        }
        // `message_drops` is a subscriber-side metric in eCAL: each
        // subscriber compares the publisher's message counter to what it
        // actually received. Attribute findings to the *subscriber* unit.
        for s in &subs {
            if s.message_drops > 0 {
                findings.push(format!(
                    "subscriber {} (pid {}) reports {} message drop(s) since registration",
                    s.unit_name, s.process_id, s.message_drops
                ));
            }
        }
        // Correlate with process state — a publisher whose process reports
        // warning/critical/failed will often surface as "no traffic" here.
        for e in pubs.iter().chain(subs.iter()) {
            if let Some(proc_state) = snap
                .processes
                .iter()
                .find(|p| p.process_id == e.process_id && p.host_name == e.host_name)
            {
                if proc_state.state_severity > 0 {
                    findings.push(format!(
                        "{} '{}' (pid {}) is in non-healthy state (severity={}, level={}): {}",
                        e.direction,
                        e.unit_name,
                        e.process_id,
                        proc_state.state_severity,
                        proc_state.state_severity_level,
                        if proc_state.state_info.is_empty() {
                            "<no state_info>"
                        } else {
                            &proc_state.state_info
                        },
                    ));
                }
            }
        }
        // Topic-name typo helper — only meaningful when the request has
        // zero matches.
        let similar_topics = if pubs.is_empty() && subs.is_empty() {
            similar_names(&topic, all_topic_names.iter().map(String::as_str), 10)
        } else {
            Vec::new()
        };

        let live_stats = if duration_ms == 0 {
            None
        } else {
            let topic_clone = topic.clone();
            let measured =
                tokio::task::spawn_blocking(move || measure_topic(&topic_clone, duration_ms))
                    .await
                    .map_err(|e| {
                        McpError::internal_error(format!("diagnose task join: {e}"), None)
                    })?
                    .map_err(|e| McpError::internal_error(e, None))?;
            if measured.samples_observed == 0 && !pubs.is_empty() {
                findings.push(format!(
                    "no samples observed in {duration_ms}ms despite {} registered publisher(s)",
                    pubs.len()
                ));
            }
            Some(measured)
        };

        Ok(Json(DiagnoseTopicResult {
            topic,
            publishers: pubs,
            subscribers: subs,
            type_signatures,
            common_active_transports,
            shm_transport_domains,
            findings,
            similar_topics,
            live_stats,
        }))
    }
}

#[tool_handler]
impl ServerHandler for EcalServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("ecal-mcp", env!("CARGO_PKG_VERSION")))
            .with_instructions(
                "Inspect and interact with an Eclipse eCAL pub/sub network. \
                 When debugging a topic that 'isn't working', start with \
                 `ecal_diagnose_topic` — it consolidates publishers, subscribers, \
                 type signatures, transport layers, process health, and a live \
                 measurement, then emits a `findings` list. \
                 Tools: ecal_list_publishers, ecal_list_subscribers, \
                 ecal_list_services, ecal_list_service_clients, ecal_list_processes, \
                 ecal_get_logs, ecal_publish, ecal_subscribe, ecal_topic_stats, \
                 ecal_diagnose_topic, ecal_call_service. \
                 List tools accept `name_pattern` for case-insensitive substring \
                 filtering on the entity name and (for pub/sub) `type_name_pattern` \
                 to filter by data type. Pass `include_descriptors=true` on pub/sub \
                 list calls to receive base-64 type descriptor blobs. \
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
    // Logs go to stderr so they don't interfere with the MCP stdio framing.
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info,rmcp=warn")),
        )
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();

    // ALL = DEFAULT | MONITORING | LOGGING | TIMESYNC, which gives us the
    // monitoring snapshot AND log retrieval out of the box without having to
    // remember the individual flags.
    Ecal::initialize(Some("ecal_mcp"), EcalComponents::ALL, None)
        .map_err(|e| anyhow::anyhow!("eCAL initialize failed: {e:?}"))?;
    tracing::info!("eCAL initialized; serving MCP over stdio");

    let outcome: Result<()> = async {
        let service = EcalServer::new()
            .serve(rmcp::transport::stdio())
            .await
            .inspect_err(|e| tracing::error!(error = ?e, "MCP serve error"))?;
        service.waiting().await?;
        Ok(())
    }
    .await;

    Ecal::finalize();
    tracing::info!("eCAL finalized; MCP server shut down");

    outcome
}

#[cfg(test)]
mod tests {
    use super::*;

    fn te(layers: &[(&str, bool)]) -> TopicEntry {
        TopicEntry {
            topic_name: String::new(),
            direction: String::new(),
            data_type: DataType {
                type_name: String::new(),
                encoding: String::new(),
                descriptor_len: 0,
                descriptor_base64: None,
            },
            host_name: String::new(),
            shm_transport_domain: String::new(),
            process_id: 0,
            process_name: String::new(),
            unit_name: String::new(),
            topic_id: 0,
            topic_size: 0,
            connections_local: 0,
            connections_external: 0,
            message_drops: 0,
            data_frequency_millihertz: 0,
            data_frequency_hz: 0.0,
            data_id: 0,
            data_clock: 0,
            registration_clock: 0,
            transport_layers: layers
                .iter()
                .map(|(k, a)| TransportLayerEntry {
                    kind: (*k).to_string(),
                    version: 1,
                    active: *a,
                })
                .collect(),
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
        // Strict ordering — a regression that swaps two entries here would
        // silently corrupt the level filter in `ecal_get_logs`.
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
        // Both functions must agree on the ordinal for every named level.
        // If a refactor renumbers one but not the other, log filtering
        // silently misclassifies entries.
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
        // The sentinel values mean "no filter" and must hash to 0 so any
        // real entry passes through.
        assert_eq!(level_priority(LogLevel::None), 0);
        assert_eq!(level_priority(LogLevel::All), 0);
    }

    #[test]
    fn intersect_active_transports_semantics() {
        let pubs = vec![te(&[("shm", true), ("udp_multicast", false)])];
        let subs = vec![te(&[("shm", true), ("tcp", true)])];
        let (common, advertised) = intersect_active_transports(&pubs, &subs);
        assert_eq!(common, vec!["shm"]);
        assert!(advertised);

        // Disjoint active layers → empty intersection but both advertised
        // (this is the "data won't flow" case the diagnostic flags).
        let pubs2 = vec![te(&[("udp_multicast", true)])];
        let subs2 = vec![te(&[("shm", true)])];
        let (common2, advertised2) = intersect_active_transports(&pubs2, &subs2);
        assert!(common2.is_empty());
        assert!(advertised2);

        // Subscriber side hasn't reported any active layer yet (common
        // during the first registration cycle): we must NOT claim "no shared
        // layer" — that's the false-positive we explicitly fixed.
        let pubs3 = vec![te(&[("shm", true)])];
        let subs3 = vec![te(&[("shm", false), ("udp_multicast", false)])];
        let (common3, advertised3) = intersect_active_transports(&pubs3, &subs3);
        assert!(common3.is_empty());
        assert!(
            !advertised3,
            "subscriber with no active layers must clear `advertised` \
                 so the diagnostic stays silent"
        );

        // Inactive layers on both sides never count, even if they match.
        let pubs4 = vec![te(&[("shm", false)])];
        let subs4 = vec![te(&[("shm", false)])];
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

        // Exact match is excluded — suggestions are always alternatives.
        let exact = similar_names("robot/imu", pool.iter().copied(), 5);
        assert!(
            !exact.contains(&"robot/imu".to_string()),
            "exact match leaked into suggestions: {exact:?}"
        );

        // Empty target shouldn't blow up (it has no 3-grams → zero hits).
        assert!(similar_names("", pool.iter().copied(), 5).is_empty());

        // Short target (<3 chars) also has no 3-grams; empty result is the
        // contract, not a panic.
        assert!(similar_names("ab", pool.iter().copied(), 5).is_empty());

        // Limit honored.
        let pool2 = ["robot/imu_a", "robot/imu_b", "robot/imu_c", "robot/imu_d"];
        let limited = similar_names("robot/imu", pool2.iter().copied(), 2);
        assert_eq!(limited.len(), 2);

        // No fuzzy matches → empty (we never return total junk).
        let none = similar_names(
            "robot/imu",
            ["xyz_stuff", "qqq_unrelated"].iter().copied(),
            5,
        );
        assert!(none.is_empty(), "expected no suggestions, got {none:?}");
    }
}
