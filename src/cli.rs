//! Optional CLI surface over the same handlers exposed via MCP.
//!
//! Agents (and humans) often want a one-shot `ecal-mcp <tool> ...` invocation
//! rather than a long-lived MCP stdio session. Every subcommand here calls
//! the **same** `EcalServer` async method that the corresponding `#[tool]`
//! does — the only thing this module owns is argv parsing and JSON
//! pretty-printing. Zero duplication of the eCAL logic itself.
//!
//! `serve` (or running with no subcommand) keeps the original MCP-over-stdio
//! behavior.

use clap::{Parser, Subcommand};
use rmcp::handler::server::wrapper::Parameters;
use rmcp::ErrorData as McpError;
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::{json, Map, Value};

use crate::EcalServer;

// The `#[tool_router]` macro on EcalServer auto-generates `tool_router()`
// returning a `ToolRouter<EcalServer>` populated from the `#[tool(...)]`
// attributes. We use it as the *single source of truth* for tool name,
// description, and input JSON schema — there is intentionally no parallel
// hand-maintained list in this file. Adding a tool requires zero changes
// here beyond one line in `dispatch()` for typed argument deserialization.

/// Top-level CLI. Subcommand defaults to `serve` when omitted, so the original
/// "just run the MCP server" invocation still works without flags.
#[derive(Debug, Parser)]
#[command(
    name = "ecal-mcp",
    version,
    about = "Local MCP server + CLI for Eclipse eCAL introspection",
    long_about = "Run with no arguments (or `serve`) to expose the MCP server over stdio. \
                  Use `tools` to list available tools and their JSON schemas, or \
                  `call <tool>` to invoke a single tool and print its JSON result."
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Cmd>,
}

#[derive(Debug, Subcommand)]
pub enum Cmd {
    /// Run the MCP server over stdio (default).
    Serve,

    /// Print the list of MCP tool names + descriptions as JSON.
    ///
    /// Useful as the very first call from an agent: discover what tools are
    /// available and what each one is for. To learn the exact arguments a
    /// tool accepts, just call it with no args (or wrong args) — the serde
    /// error will name the required fields.
    Tools,

    /// Invoke a single tool and print its JSON result on stdout.
    ///
    /// Args are supplied either as a complete JSON object via `--json`, or
    /// piecemeal via repeated `-a key=value` flags (values are parsed as JSON
    /// when possible, falling back to a string). Use `tools --schemas` to
    /// see what each tool accepts.
    ///
    /// Example:
    ///
    ///   ecal-mcp call ecal_diagnose_topic -a topic=/sensors/lidar -a duration_ms=2000
    ///   ecal-mcp call ecal_list_publishers --json '{"name_pattern":"imu"}'
    Call {
        /// Tool name (e.g. `ecal_diagnose_topic`). See `ecal-mcp tools`.
        tool: String,

        /// Full JSON object of arguments. Mutually exclusive with `-a`.
        #[arg(long, conflicts_with = "arg")]
        json: Option<String>,

        /// Incremental `key=value` argument. Value parses as JSON when it
        /// looks like JSON (numbers, bools, objects, arrays, null), otherwise
        /// falls back to a plain string. Repeatable.
        #[arg(short = 'a', long = "arg", value_name = "KEY=VALUE")]
        arg: Vec<String>,

        /// Pretty-print the JSON output (default: compact one-line JSON, which
        /// is friendlier to pipe through `jq`).
        #[arg(long)]
        pretty: bool,
    },
}

/// Build the args JSON object from either `--json` or repeated `-a key=value` flags.
fn build_args(json: Option<String>, kv: Vec<String>) -> Result<Value, String> {
    if let Some(raw) = json {
        let v: Value = serde_json::from_str(&raw)
            .map_err(|e| format!("--json is not valid JSON: {e}"))?;
        if !v.is_object() {
            return Err("--json must be a JSON object".into());
        }
        return Ok(v);
    }
    let mut obj = Map::new();
    for entry in kv {
        let (k, v) = entry
            .split_once('=')
            .ok_or_else(|| format!("expected key=value, got {entry:?}"))?;
        // Try JSON first so `-a duration_ms=5000` becomes a number, not a
        // string. Plain strings without quotes are accepted as a fallback.
        let parsed = serde_json::from_str::<Value>(v).unwrap_or_else(|_| Value::String(v.into()));
        obj.insert(k.to_string(), parsed);
    }
    Ok(Value::Object(obj))
}

/// Reflect the live `#[tool(...)]` registry on `EcalServer` so `ecal-mcp tools`
/// always returns exactly what the MCP server advertises — same names, same
/// descriptions, same JSON schemas, no parallel manifest to keep in sync.
fn tools_manifest() -> Value {
    let router = EcalServer::tool_router();
    let mut arr: Vec<Value> = router
        .list_all()
        .into_iter()
        .map(|t| {
            json!({
                "name": t.name,
                "description": t.description,
                "input_schema": *t.input_schema,
            })
        })
        .collect();
    arr.sort_by(|a, b| {
        a.get("name")
            .and_then(Value::as_str)
            .cmp(&b.get("name").and_then(Value::as_str))
    });
    json!({ "tools": arr })
}

/// Run a CLI subcommand against an already-constructed `EcalServer`. The caller
/// owns eCAL initialization/finalization (kept in `main` so both MCP and CLI
/// paths share that one-time setup).
pub async fn run(server: &EcalServer, cmd: Cmd) -> Result<Value, String> {
    match cmd {
        Cmd::Serve => unreachable!("Serve handled in main()"),
        Cmd::Tools => Ok(tools_manifest()),
        Cmd::Call { tool, json, arg, pretty: _ } => {
            let args = build_args(json, arg)?;
            dispatch(server, &tool, args).await
        }
    }
}

/// Universal tool dispatch. Each arm deserializes into the same `*Args` struct
/// the MCP `#[tool]` macro consumes, then calls the same async method on
/// `EcalServer`, then serializes the success branch's `Json<T>` back to a
/// generic JSON value.
async fn dispatch(server: &EcalServer, tool: &str, args: Value) -> Result<Value, String> {
    macro_rules! call {
        ($args_ty:ty, $method:ident) => {{
            let parsed: $args_ty = parse_args(args)?;
            let res = server
                .$method(Parameters(parsed))
                .await
                .map_err(mcp_err_to_string)?;
            to_json(&res.0)
        }};
    }

    match tool {
        "ecal_list_publishers" => call!(crate::ListFilter, ecal_list_publishers),
        "ecal_list_subscribers" => call!(crate::ListFilter, ecal_list_subscribers),
        "ecal_list_services" => call!(crate::ListFilter, ecal_list_services),
        "ecal_list_service_clients" => call!(crate::ListFilter, ecal_list_service_clients),
        "ecal_list_processes" => call!(crate::ListFilter, ecal_list_processes),
        "ecal_get_logs" => call!(crate::GetLogsArgs, ecal_get_logs),
        "ecal_publish" => call!(crate::PublishArgs, ecal_publish),
        "ecal_subscribe" => call!(crate::SubscribeArgs, ecal_subscribe),
        "ecal_call_service" => call!(crate::CallServiceArgs, ecal_call_service),
        "ecal_topic_stats" => call!(crate::TopicStatsArgs, ecal_topic_stats),
        "ecal_diagnose_topic" => call!(crate::DiagnoseTopicArgs, ecal_diagnose_topic),
        other => Err(format!(
            "unknown tool {other:?}; run `ecal-mcp tools` to list available tools"
        )),
    }
}

fn parse_args<T: DeserializeOwned>(v: Value) -> Result<T, String> {
    // An empty object `{}` is valid for tools whose every field is optional
    // (the listing tools); required-field tools surface the missing field
    // via a normal serde error, which is the right message for an agent.
    serde_json::from_value::<T>(v).map_err(|e| format!("invalid arguments: {e}"))
}

fn to_json<T: Serialize>(v: &T) -> Result<Value, String> {
    serde_json::to_value(v).map_err(|e| format!("serialize result: {e}"))
}

fn mcp_err_to_string(e: McpError) -> String {
    // McpError prints fine via Debug; agents can still grep `message`/`code`.
    format!("{e:?}")
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- build_args: argv → JSON object ---------------------------------

    #[test]
    fn build_args_kv_typed_coercion() {
        // Numbers, bools, null, and JSON literals must be parsed as JSON, not
        // smuggled through as strings. Otherwise `-a duration_ms=5000` would
        // hit the underlying serde with a string and fail the integer field.
        let v = build_args(
            None,
            vec![
                "duration_ms=5000".into(),
                "include_descriptors=true".into(),
                "name_pattern=lidar".into(),
                "since_timestamp_us=null".into(),
                "tags=[\"a\",\"b\"]".into(),
            ],
        )
        .unwrap();
        let o = v.as_object().unwrap();
        assert_eq!(o["duration_ms"], json!(5000));
        assert_eq!(o["include_descriptors"], json!(true));
        // Plain strings without JSON quoting must fall back to a string —
        // otherwise `name_pattern=lidar` would fail JSON parsing and error.
        assert_eq!(o["name_pattern"], json!("lidar"));
        assert_eq!(o["since_timestamp_us"], Value::Null);
        assert_eq!(o["tags"], json!(["a", "b"]));
    }

    #[test]
    fn build_args_kv_quoted_string_stays_string() {
        // A value that happens to look like a JSON string literal MUST decode
        // to the unquoted string, not double-quoted nonsense.
        let v = build_args(None, vec!["text=\"hello\"".into()]).unwrap();
        assert_eq!(v["text"], json!("hello"));
    }

    #[test]
    fn build_args_json_path() {
        let v = build_args(Some("{\"topic\":\"/foo\",\"duration_ms\":2000}".into()), vec![])
            .unwrap();
        assert_eq!(v["topic"], json!("/foo"));
        assert_eq!(v["duration_ms"], json!(2000));
    }

    #[test]
    fn build_args_json_must_be_object() {
        let err = build_args(Some("[1,2,3]".into()), vec![]).unwrap_err();
        assert!(
            err.contains("must be a JSON object"),
            "expected object-type guard, got {err:?}"
        );
    }

    #[test]
    fn build_args_kv_requires_equals() {
        let err = build_args(None, vec!["bare_token".into()]).unwrap_err();
        assert!(
            err.contains("key=value"),
            "expected key=value hint, got {err:?}"
        );
    }

    // ---- tools_manifest: reflection invariants --------------------------
    //
    // These are the load-bearing tests for the "no duplicated tool metadata"
    // refactor. If someone re-introduces a hand-written manifest, the count
    // and required-field assertions below will diverge from the live
    // `#[tool(...)]` registry and fail loudly.

    #[test]
    fn tools_manifest_reflects_router_one_to_one() {
        let manifest = tools_manifest();
        let arr = manifest["tools"].as_array().unwrap();
        let router = EcalServer::tool_router();
        assert_eq!(
            arr.len(),
            router.list_all().len(),
            "tools_manifest should mirror tool_router 1:1"
        );
    }

    #[test]
    fn tools_manifest_every_entry_has_description_and_schema() {
        // The single-source-of-truth promise: every tool must carry a
        // description (so agents can pick one) and a non-trivial input schema
        // (so they can construct args). A regression that drops the
        // `description = ...` attribute on any tool will trip this.
        for entry in tools_manifest()["tools"].as_array().unwrap() {
            let name = entry["name"].as_str().unwrap_or("<unnamed>");
            let desc = entry["description"].as_str().unwrap_or("");
            assert!(
                !desc.trim().is_empty(),
                "tool {name:?} has no description in the live registry"
            );
            let schema = entry["input_schema"].as_object().unwrap_or_else(|| {
                panic!("tool {name:?} has no input_schema object")
            });
            assert_eq!(
                schema.get("type").and_then(Value::as_str),
                Some("object"),
                "tool {name:?} input_schema must be a JSON object schema"
            );
        }
    }

    #[test]
    fn tools_manifest_is_sorted_and_includes_known_tools() {
        let arr = tools_manifest()["tools"].as_array().unwrap().clone();
        let names: Vec<&str> = arr.iter().map(|e| e["name"].as_str().unwrap()).collect();
        // Stable order is part of the CLI contract — agents grep this output.
        let mut sorted = names.clone();
        sorted.sort();
        assert_eq!(names, sorted, "tools_manifest output must be name-sorted");
        // Spot-check the three tools agents reach for first; if any of these
        // ever gets renamed, callers will need to know.
        for must_have in ["ecal_diagnose_topic", "ecal_topic_stats", "ecal_list_publishers"] {
            assert!(
                names.contains(&must_have),
                "tools_manifest missing {must_have:?}: {names:?}"
            );
        }
    }
}

/// Public helper so `main` can decide pretty-vs-compact based on the parsed
/// command without re-parsing argv.
pub fn print_value(v: &Value, pretty: bool) {
    let s = if pretty {
        serde_json::to_string_pretty(v).unwrap_or_else(|e| format!("{{\"error\":\"{e}\"}}"))
    } else {
        serde_json::to_string(v).unwrap_or_else(|e| format!("{{\"error\":\"{e}\"}}"))
    };
    println!("{s}");
}
