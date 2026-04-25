#!/usr/bin/env python3
"""End-to-end test for ecal-mcp.

Builds the Docker image, starts a single container with two helper publishers
(slow + fast), one helper service server (`echo` + `reverse`), and the MCP
server itself, then drives the MCP over stdio with NDJSON JSON-RPC.

Each `runner.run(name, body)` is asserted independently; failures don't abort
the run. The full list of cases is documented in README.md (the "Testing"
section). Set `ECAL_MCP_KEEP_CONTAINER=1` to leave the container running for
debugging; on any failure the last 300 lines of container logs are dumped.
"""

from __future__ import annotations

import base64
import json
import os
import subprocess
import sys
import threading
import time
import uuid
from dataclasses import dataclass
from queue import Queue
from typing import Any, Callable

REPO_ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
IMAGE_TAG = os.environ.get("ECAL_MCP_IMAGE", "ecal-mcp:e2e")
CONTAINER_NAME = os.environ.get("ECAL_MCP_CONTAINER", f"ecal-mcp-e2e-{uuid.uuid4().hex[:8]}")

PUB_TOPIC = "ecal_mcp_e2e_pub"          # topic the test publisher streams on
ROUNDTRIP_TEXT_TOPIC = "ecal_mcp_e2e_rt_text"
ROUNDTRIP_BYTES_TOPIC = "ecal_mcp_e2e_rt_bytes"
FLOOD_TOPIC = "ecal_mcp_e2e_flood"
SERVICE_NAME = "ecal_mcp_e2e_service"
ABSENT_SERVICE_NAME = "ecal_mcp_e2e_does_not_exist"

EXPECTED_TOOLS = {
    "ecal_list_publishers",
    "ecal_list_subscribers",
    "ecal_list_services",
    "ecal_list_service_clients",
    "ecal_list_processes",
    "ecal_get_logs",
    "ecal_publish",
    "ecal_subscribe",
    "ecal_call_service",
    "ecal_topic_stats",
    "ecal_diagnose_topic",
}


# ---------------------------------------------------------------------------
# Plumbing
# ---------------------------------------------------------------------------


def log(msg: str) -> None:
    print(f"[e2e] {msg}", flush=True)


def run(cmd: list[str], **kw: Any) -> subprocess.CompletedProcess[str]:
    log("$ " + " ".join(cmd))
    return subprocess.run(cmd, check=True, text=True, **kw)


def docker_exists(name: str) -> bool:
    out = subprocess.run(
        ["docker", "ps", "-a", "--filter", f"name=^{name}$", "--format", "{{.Names}}"],
        text=True, capture_output=True,
    )
    return name in out.stdout.split()


# ---------------------------------------------------------------------------
# Stdio MCP client (NDJSON JSON-RPC)
# ---------------------------------------------------------------------------


@dataclass
class JsonRpcResponse:
    id: int | str
    result: Any | None
    error: Any | None


class StdioMcpClient:
    def __init__(self, popen: subprocess.Popen[bytes]) -> None:
        self._proc = popen
        self._responses: dict[int | str, JsonRpcResponse] = {}
        self._notifications: Queue[dict[str, Any]] = Queue()
        self._cv = threading.Condition()
        self._closed = False
        self._next_id = 1
        threading.Thread(target=self._read_loop, daemon=True).start()
        threading.Thread(target=self._stderr_loop, daemon=True).start()

    def _read_loop(self) -> None:
        assert self._proc.stdout is not None
        for raw in self._proc.stdout:
            line = raw.decode("utf-8", errors="replace").strip()
            if not line:
                continue
            try:
                msg = json.loads(line)
            except json.JSONDecodeError:
                log(f"[mcp<-] non-json: {line!r}")
                continue
            if "id" in msg and ("result" in msg or "error" in msg):
                resp = JsonRpcResponse(id=msg["id"], result=msg.get("result"), error=msg.get("error"))
                with self._cv:
                    self._responses[resp.id] = resp
                    self._cv.notify_all()
            else:
                self._notifications.put(msg)
        with self._cv:
            self._closed = True
            self._cv.notify_all()

    def _stderr_loop(self) -> None:
        assert self._proc.stderr is not None
        for raw in self._proc.stderr:
            sys.stderr.write(f"[mcp-stderr] {raw.decode('utf-8', errors='replace')}")

    def _send(self, payload: dict[str, Any]) -> None:
        assert self._proc.stdin is not None
        self._proc.stdin.write((json.dumps(payload) + "\n").encode("utf-8"))
        self._proc.stdin.flush()

    def request(self, method: str, params: dict[str, Any] | None = None, timeout: float = 15.0) -> Any:
        rpc_id = self._next_id
        self._next_id += 1
        payload: dict[str, Any] = {"jsonrpc": "2.0", "id": rpc_id, "method": method}
        if params is not None:
            payload["params"] = params
        log(f"[mcp->] {method} id={rpc_id}")
        self._send(payload)
        deadline = time.time() + timeout
        with self._cv:
            while rpc_id not in self._responses:
                if self._closed:
                    raise RuntimeError(f"MCP server closed before responding to {method}")
                remaining = deadline - time.time()
                if remaining <= 0:
                    raise TimeoutError(f"MCP request {method} timed out after {timeout}s")
                self._cv.wait(timeout=remaining)
            return self._responses.pop(rpc_id)

    def request_ok(self, method: str, params: dict[str, Any] | None = None, timeout: float = 15.0) -> Any:
        resp = self.request(method, params, timeout=timeout)
        if resp.error is not None:
            raise RuntimeError(f"MCP error on {method}: {resp.error}")
        return resp.result

    def notify(self, method: str, params: dict[str, Any] | None = None) -> None:
        payload: dict[str, Any] = {"jsonrpc": "2.0", "method": method}
        if params is not None:
            payload["params"] = params
        log(f"[mcp->] notify {method}")
        self._send(payload)

    def close(self) -> None:
        if self._proc.stdin and not self._proc.stdin.closed:
            try:
                self._proc.stdin.close()
            except OSError:
                pass


def call_tool(client: StdioMcpClient, name: str, arguments: dict[str, Any] | None = None,
              timeout: float = 20.0) -> Any:
    params: dict[str, Any] = {"name": name}
    if arguments is not None:
        params["arguments"] = arguments
    result = client.request_ok("tools/call", params, timeout=timeout)
    if result.get("isError"):
        raise RuntimeError(f"Tool {name} reported error: {result}")
    if (sc := result.get("structuredContent")) is not None:
        return sc
    for block in result.get("content", []):
        if block.get("type") == "text":
            return json.loads(block["text"])
    raise RuntimeError(f"No structured content from tool {name}: {result}")


def call_tool_expect_error(client: StdioMcpClient, name: str, arguments: dict[str, Any]) -> str:
    """Invoke a tool that should fail; return a stringified error."""
    params = {"name": name, "arguments": arguments}
    resp = client.request("tools/call", params, timeout=15.0)
    if resp.error is not None:
        return json.dumps(resp.error)
    if isinstance(resp.result, dict) and resp.result.get("isError"):
        return json.dumps(resp.result)
    raise AssertionError(
        f"expected tool {name} with args {arguments!r} to fail, got result={resp.result!r}"
    )


# ---------------------------------------------------------------------------
# Tiny test harness
# ---------------------------------------------------------------------------


class TestRunner:
    def __init__(self) -> None:
        self.failures: list[tuple[str, str]] = []
        self.passed = 0

    def run(self, name: str, body: Callable[[], None]) -> None:
        log(f"==> {name}")
        try:
            body()
            self.passed += 1
            log(f"    PASS: {name}")
        except Exception as exc:  # noqa: BLE001
            self.failures.append((name, repr(exc)))
            log(f"    FAIL: {name}: {exc!r}")

    def summary(self) -> int:
        total = self.passed + len(self.failures)
        log(f"---\n{self.passed}/{total} passed")
        for name, err in self.failures:
            log(f"  - {name}: {err}")
        return 0 if not self.failures else 1


def assert_eq(actual: Any, expected: Any, label: str) -> None:
    if actual != expected:
        raise AssertionError(f"{label}: expected {expected!r}, got {actual!r}")


def assert_true(cond: bool, label: str) -> None:
    if not cond:
        raise AssertionError(label)


# ---------------------------------------------------------------------------
# Container lifecycle
# ---------------------------------------------------------------------------


def build_image() -> None:
    log(f"Building image {IMAGE_TAG} (this can take several minutes the first time)...")
    run(["docker", "build", "-t", IMAGE_TAG, REPO_ROOT])


def start_container() -> None:
    if docker_exists(CONTAINER_NAME):
        run(["docker", "rm", "-f", CONTAINER_NAME])
    log(f"Starting container {CONTAINER_NAME}")
    run(
        [
            "docker", "run", "-d",
            "--name", CONTAINER_NAME,
            "--shm-size=256m",
            "--entrypoint", "tail",
            IMAGE_TAG, "-f", "/dev/null",
        ]
    )


def stop_container() -> None:
    if os.environ.get("ECAL_MCP_KEEP_CONTAINER") == "1":
        log(f"ECAL_MCP_KEEP_CONTAINER=1; leaving container {CONTAINER_NAME} alive")
        return
    if docker_exists(CONTAINER_NAME):
        log(f"Removing container {CONTAINER_NAME}")
        subprocess.run(["docker", "rm", "-f", CONTAINER_NAME], check=False)


def start_helpers() -> None:
    log("Starting in-container test publisher")
    run([
        "docker", "exec", "-d", CONTAINER_NAME,
        "/usr/local/bin/ecal-test-publisher",
        "--topic", PUB_TOPIC,
        "--prefix", "hello-from-e2e",
        "--interval-ms", "100",
    ])
    log("Starting in-container fast publisher (for max_samples test)")
    run([
        "docker", "exec", "-d", CONTAINER_NAME,
        "/usr/local/bin/ecal-test-publisher",
        "--topic", FLOOD_TOPIC,
        "--prefix", "flood",
        "--interval-ms", "10",
    ])
    log("Starting in-container test service server")
    run([
        "docker", "exec", "-d", CONTAINER_NAME,
        "/usr/local/bin/ecal-test-service-server",
        "--service", SERVICE_NAME,
    ])


def open_mcp_client() -> StdioMcpClient:
    log("Launching ecal-mcp via docker exec")
    proc = subprocess.Popen(
        ["docker", "exec", "-i", CONTAINER_NAME, "/usr/local/bin/ecal-mcp"],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        bufsize=0,
    )
    return StdioMcpClient(proc)


# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------


def main() -> int:
    build_image()
    start_container()
    runner = TestRunner()
    try:
        start_helpers()
        # eCAL's default `registration_refresh` is 1000ms, so a brand-new
        # process needs ~2 cycles before it has a complete picture of the
        # network. Sleep a bit longer to be safe.
        time.sleep(4.0)

        client = open_mcp_client()
        try:
            # ---- 1. initialize ----------------------------------------------
            def t_initialize() -> None:
                init = client.request_ok(
                    "initialize",
                    {
                        "protocolVersion": "2024-11-05",
                        "capabilities": {},
                        "clientInfo": {"name": "ecal-mcp-e2e", "version": "0.0.1"},
                    },
                )
                assert_eq(init["serverInfo"]["name"], "ecal-mcp", "serverInfo.name")
                client.notify("notifications/initialized")

            runner.run("initialize handshake", t_initialize)

            # Give the freshly-spawned MCP process time to absorb registration
            # broadcasts (~registration_refresh interval).
            time.sleep(3.0)

            # ---- 2. tools/list ----------------------------------------------
            def t_tools_list() -> None:
                tools = client.request_ok("tools/list", {})
                names = sorted(t["name"] for t in tools["tools"])
                missing = EXPECTED_TOOLS - set(names)
                assert_true(not missing, f"missing tools: {missing}")
                for t in tools["tools"]:
                    assert_true(
                        isinstance(t.get("inputSchema"), dict)
                        and t["inputSchema"].get("type") == "object",
                        f"tool {t['name']} missing object inputSchema",
                    )

            runner.run("tools/list reports schemas", t_tools_list)

            # ---- 3. list_publishers -----------------------------------------
            def t_list_publishers() -> None:
                pubs = call_tool(client, "ecal_list_publishers")["publishers"]
                assert_true(any(p["topic_name"] == PUB_TOPIC for p in pubs),
                            f"PUB_TOPIC {PUB_TOPIC!r} not found in {[p['topic_name'] for p in pubs]}")
                assert_true(any(p["topic_name"] == FLOOD_TOPIC for p in pubs),
                            f"FLOOD_TOPIC {FLOOD_TOPIC!r} not found")
                for p in pubs:
                    for k in ("topic_name", "data_type", "host_name",
                              "shm_transport_domain", "process_id",
                              "data_frequency_millihertz", "data_frequency_hz",
                              "transport_layers", "data_id", "data_clock",
                              "registration_clock"):
                        assert_true(k in p, f"publisher entry missing {k}: {p}")
                    assert_true(p["data_frequency_hz"] >= 0,
                                f"data_frequency_hz negative: {p}")
                    assert_true(isinstance(p["transport_layers"], list),
                                f"transport_layers not list: {p}")
                    for layer in p["transport_layers"]:
                        for lk in ("kind", "version", "active"):
                            assert_true(lk in layer, f"layer missing {lk}: {layer}")
                    assert_true(p["registration_clock"] >= 0,
                                f"registration_clock negative: {p}")

            runner.run("ecal_list_publishers", t_list_publishers)

            # ---- 4. list_processes ------------------------------------------
            def t_list_processes() -> None:
                procs = call_tool(client, "ecal_list_processes")["processes"]
                units = {p["unit_name"] for p in procs}
                assert_true("ecal_test_publisher" in units,
                            f"publisher unit not visible: {units}")
                assert_true("ecal_test_service_server" in units,
                            f"service-server unit not visible: {units}")

            runner.run("ecal_list_processes", t_list_processes)

            # ---- 5. list_services -------------------------------------------
            def t_list_services() -> None:
                svcs = call_tool(client, "ecal_list_services")["services"]
                ours = [s for s in svcs if s["service_name"] == SERVICE_NAME]
                assert_true(len(ours) >= 1, f"{SERVICE_NAME!r} not in {svcs!r}")
                methods = {m["method_name"] for m in ours[0]["methods"]}
                assert_true({"echo", "reverse"}.issubset(methods),
                            f"expected echo+reverse, got {methods}")

            runner.run("ecal_list_services", t_list_services)

            # ---- 6. subscribe sees publisher --------------------------------
            def t_subscribe_text() -> None:
                sub = call_tool(client, "ecal_subscribe", {
                    "topic": PUB_TOPIC, "duration_ms": 3500, "max_samples": 5,
                })
                assert_true(sub["samples_collected"] >= 1, f"no samples: {sub}")
                first = sub["samples"][0]
                assert_true(
                    first["text"] is not None and first["text"].startswith("hello-from-e2e"),
                    f"unexpected sample: {first}",
                )
                assert_eq(first.get("topic_name"), PUB_TOPIC, "sample.topic_name")
                # base64 round-trip should equal the bytes of `text`
                decoded = base64.b64decode(first["payload_base64"])
                assert_eq(decoded.decode("utf-8"), first["text"], "payload_base64 == text")

            runner.run("ecal_subscribe (string)", t_subscribe_text)

            # ---- 7. subscribers list non-empty during a parallel subscribe ---
            def t_list_subscribers_during_sub() -> None:
                holder: dict[str, Any] = {}

                def do_sub() -> None:
                    holder["sub"] = call_tool(client, "ecal_subscribe", {
                        "topic": PUB_TOPIC, "duration_ms": 3500, "max_samples": 1,
                    })
                t = threading.Thread(target=do_sub)
                t.start()
                time.sleep(2.0)
                subs = call_tool(client, "ecal_list_subscribers")["subscribers"]
                t.join(timeout=10)
                assert_true(any(s["topic_name"] == PUB_TOPIC for s in subs),
                            f"subscriber on {PUB_TOPIC!r} not visible: {[s['topic_name'] for s in subs]}")

            runner.run("ecal_list_subscribers", t_list_subscribers_during_sub)

            # ---- 8. max_samples enforcement ---------------------------------
            def t_max_samples() -> None:
                sub = call_tool(client, "ecal_subscribe", {
                    "topic": FLOOD_TOPIC, "duration_ms": 3000, "max_samples": 3,
                })
                assert_eq(sub["samples_collected"], 3, "samples_collected")
                assert_true(sub["samples_dropped"] >= 1,
                            f"expected drops with fast publisher, got {sub}")

            runner.run("ecal_subscribe respects max_samples", t_max_samples)

            # ---- 9. text round-trip publish + subscribe ---------------------
            def roundtrip(topic: str, text: str | None = None,
                          payload_b64: str | None = None) -> dict[str, Any]:
                """Run subscribe + publish in parallel, return the subscribe result."""
                sub_args = {"topic": topic, "duration_ms": 4500, "max_samples": 5}
                holder: dict[str, Any] = {}

                def do_sub() -> None:
                    holder["sub"] = call_tool(client, "ecal_subscribe", sub_args, timeout=15.0)

                t = threading.Thread(target=do_sub)
                t.start()
                time.sleep(1.2)

                args: dict[str, Any] = {
                    "topic": topic,
                    "discovery_wait_ms": 2000,
                    "repeat": 5,
                    "repeat_interval_ms": 100,
                }
                if text is not None:
                    args["text"] = text
                if payload_b64 is not None:
                    args["payload_base64"] = payload_b64
                pub = call_tool(client, "ecal_publish", args, timeout=15.0)
                assert_eq(pub["sends_attempted"], args["repeat"], "sends_attempted")
                t.join(timeout=15)
                assert_true("sub" in holder, "subscribe thread did not finish")
                return holder["sub"]

            def t_text_roundtrip() -> None:
                payload = "round-trip-from-mcp"
                sub = roundtrip(ROUNDTRIP_TEXT_TOPIC, text=payload)
                assert_true(any(s.get("text") == payload for s in sub["samples"]),
                            f"payload {payload!r} not in {sub!r}")

            runner.run("ecal_publish text round-trip", t_text_roundtrip)

            # ---- 10. binary round-trip ---------------------------------------
            def t_bytes_roundtrip() -> None:
                # Include a NUL and high bytes so we can be sure we're not just
                # piggy-backing on the string path.
                blob = bytes([0, 1, 2, 0xfe, 0xff, 0x00, 0x42]) + b"raw-binary"
                sub = roundtrip(ROUNDTRIP_BYTES_TOPIC, payload_b64=base64.b64encode(blob).decode())
                hits = [s for s in sub["samples"]
                        if base64.b64decode(s["payload_base64"]) == blob]
                assert_true(bool(hits), f"binary payload not echoed: {sub!r}")
                # NUL byte means the bytes are NOT valid UTF-8 → text should be None.
                assert_true(hits[0].get("text") is None,
                            f"expected text=None for non-UTF-8 payload, got {hits[0]}")

            runner.run("ecal_publish bytes round-trip", t_bytes_roundtrip)

            # ---- 11. service echo --------------------------------------------
            def t_service_echo() -> None:
                payload = "ping-1234"
                resp = call_tool(client, "ecal_call_service", {
                    "service": SERVICE_NAME,
                    "method": "echo",
                    "text": payload,
                    "timeout_ms": 2000,
                    "discovery_wait_ms": 1500,
                })
                assert_true(resp["instances"] >= 1, f"no service instances: {resp}")
                ok = [r for r in resp["responses"] if r["success"]]
                assert_true(bool(ok), f"no successful response: {resp}")
                assert_eq(ok[0]["response_text"], payload, "echo response_text")
                # Server identity is plumbed through from eCAL_SServiceResponse.
                for r in ok:
                    assert_true(r.get("server_entity_id", 0) > 0,
                                f"server_entity_id not populated: {r}")
                    assert_true(r.get("server_process_id", 0) > 0,
                                f"server_process_id not populated: {r}")
                    assert_true(bool(r.get("server_host_name")),
                                f"server_host_name empty: {r}")

            runner.run("ecal_call_service echo", t_service_echo)

            # ---- 12. service reverse + binary --------------------------------
            def t_service_reverse_bytes() -> None:
                blob = bytes([0x01, 0x02, 0x03, 0x04, 0x05, 0xfa, 0xfb, 0xfc])
                resp = call_tool(client, "ecal_call_service", {
                    "service": SERVICE_NAME,
                    "method": "reverse",
                    "payload_base64": base64.b64encode(blob).decode(),
                    "timeout_ms": 2000,
                    "discovery_wait_ms": 1500,
                })
                ok = [r for r in resp["responses"] if r["success"]]
                assert_true(bool(ok), f"no successful response: {resp}")
                got = base64.b64decode(ok[0]["response_base64"])
                assert_eq(got, blob[::-1], "reversed bytes")

            runner.run("ecal_call_service reverse (bytes)", t_service_reverse_bytes)

            # ---- 13. validation: must supply exactly one of text/base64 ------
            def t_publish_validation() -> None:
                err1 = call_tool_expect_error(client, "ecal_publish", {
                    "topic": "x", "text": "hi", "payload_base64": base64.b64encode(b"hi").decode(),
                })
                assert_true("exactly one" in err1.lower() or "invalid" in err1.lower(),
                            f"unexpected error: {err1}")
                err2 = call_tool_expect_error(client, "ecal_publish", {"topic": "x"})
                assert_true("exactly one" in err2.lower() or "invalid" in err2.lower(),
                            f"unexpected error: {err2}")

            runner.run("ecal_publish input validation", t_publish_validation)

            # ---- 14. service-call against absent service is graceful --------
            def t_absent_service() -> None:
                resp = call_tool(client, "ecal_call_service", {
                    "service": ABSENT_SERVICE_NAME,
                    "method": "echo",
                    "text": "anybody?",
                    "timeout_ms": 500,
                    "discovery_wait_ms": 600,
                })
                assert_eq(resp["instances"], 0, "instances for absent service")
                assert_eq(resp["responses"], [], "responses for absent service")

            runner.run("ecal_call_service against absent service", t_absent_service)

            # ---- 15. ecal_list_service_clients ------------------------------
            def t_list_service_clients() -> None:
                # Issue a call so a client registration definitely exists, then
                # snapshot. (Clients deregister quickly, so do this together.)
                holder: dict[str, Any] = {}

                def do_call() -> None:
                    holder["resp"] = call_tool(client, "ecal_call_service", {
                        "service": SERVICE_NAME,
                        "method": "echo",
                        "text": "client-discovery-probe",
                        "timeout_ms": 2000,
                        "discovery_wait_ms": 1500,
                    }, timeout=15.0)

                t = threading.Thread(target=do_call)
                t.start()
                time.sleep(1.5)
                clients_resp = call_tool(client, "ecal_list_service_clients")
                t.join(timeout=15)
                clients = clients_resp.get("clients", [])
                ours = [c for c in clients if c["service_name"] == SERVICE_NAME]
                # We at least see *some* client registration. Don't hard-fail
                # if eCAL prunes it before we snapshot — but the field must be
                # a list of well-formed entries.
                assert_true(isinstance(clients, list), f"clients not list: {clients_resp}")
                for c in clients:
                    for k in ("service_name", "host_name", "process_id", "methods"):
                        assert_true(k in c, f"client entry missing {k}: {c}")
                if ours:
                    log(f"saw {len(ours)} client(s) on {SERVICE_NAME}")

            runner.run("ecal_list_service_clients", t_list_service_clients)

            # ---- 16. ecal_get_logs ------------------------------------------
            def t_get_logs() -> None:
                resp = call_tool(client, "ecal_get_logs")
                logs = resp.get("logs")
                assert_true(isinstance(logs, list), f"logs not list: {resp}")
                # Don't assert non-empty: with default eCAL config many envs
                # produce zero log entries. Just validate field shape if any.
                for entry in logs:
                    for k in ("level", "timestamp_us", "host_name",
                              "process_name", "process_id", "content"):
                        assert_true(k in entry, f"log entry missing {k}: {entry}")

            runner.run("ecal_get_logs", t_get_logs)

            # ---- 17. name_pattern filter on lists ---------------------------
            def t_name_pattern_filter() -> None:
                # Should narrow to just FLOOD_TOPIC.
                pubs = call_tool(client, "ecal_list_publishers",
                                 {"name_pattern": "flood"})["publishers"]
                names = {p["topic_name"] for p in pubs}
                assert_true(FLOOD_TOPIC in names, f"flood pub not in {names}")
                assert_true(PUB_TOPIC not in names,
                            f"PUB_TOPIC leaked through pattern filter: {names}")
                # Empty pattern should be a no-op.
                all_pubs = call_tool(client, "ecal_list_publishers",
                                     {"name_pattern": ""})["publishers"]
                assert_true(len(all_pubs) >= 2, f"empty pattern filtered: {all_pubs}")
                # Service-side filter.
                svcs = call_tool(client, "ecal_list_services",
                                 {"name_pattern": "e2e_service"})["services"]
                assert_true(any(s["service_name"] == SERVICE_NAME for s in svcs),
                            f"service pattern filter dropped target: {svcs}")

            runner.run("name_pattern filtering", t_name_pattern_filter)

            # ---- 18. ecal_topic_stats ---------------------------------------
            def t_topic_stats() -> None:
                resp = call_tool(client, "ecal_topic_stats", {
                    "topic": FLOOD_TOPIC,
                    "duration_ms": 3500,
                }, timeout=15.0)
                # Fast publisher runs every 10ms; first ~1s is eaten by
                # subscriber discovery, so be generous on the floor.
                assert_true(resp["samples_observed"] >= 30,
                            f"too few samples: {resp}")
                assert_true(resp["observed_hz"] > 15.0,
                            f"observed_hz too low: {resp}")
                for k in ("min_size_bytes", "mean_size_bytes", "max_size_bytes",
                          "min_gap_us", "mean_gap_us", "max_gap_us", "gap_stddev_us",
                          "first_timestamp_us", "last_timestamp_us"):
                    assert_true(k in resp, f"missing field {k}: {resp}")
                assert_true(resp["min_gap_us"] >= 0, f"negative gap: {resp}")
                assert_true(resp["gap_stddev_us"] >= 0, f"negative stddev: {resp}")
                # max_gap >= mean_gap >= min_gap by construction.
                assert_true(resp["min_gap_us"] <= resp["mean_gap_us"] <= resp["max_gap_us"],
                            f"gap stats out of order: {resp}")

            runner.run("ecal_topic_stats", t_topic_stats)

            # ---- 19. ecal_diagnose_topic — happy path -----------------------
            def t_diagnose_happy() -> None:
                # Run a parallel subscribe so a subscriber registration exists
                # while we diagnose; otherwise the tool will (correctly) flag
                # "no subscriber registered".
                holder: dict[str, Any] = {}

                def do_sub() -> None:
                    holder["sub"] = call_tool(client, "ecal_subscribe", {
                        "topic": PUB_TOPIC, "duration_ms": 4000, "max_samples": 1,
                    }, timeout=15.0)

                t = threading.Thread(target=do_sub)
                t.start()
                time.sleep(1.0)
                resp = call_tool(client, "ecal_diagnose_topic", {
                    "topic": PUB_TOPIC, "duration_ms": 1500,
                }, timeout=15.0)
                t.join(timeout=15)
                assert_true(len(resp["publishers"]) >= 1,
                            f"diagnose missed publisher: {resp}")
                assert_true(resp["live_stats"] is not None
                            and resp["live_stats"]["samples_observed"] >= 1,
                            f"diagnose live_stats empty: {resp}")
                # The e2e helper subscribes as BytesMessage even though the
                # publisher emits StringMessage, so we expect *exactly* two
                # distinct type signatures here. A regression that collapsed
                # them (or duplicated them) would change this count.
                # The e2e helper subscribes as BytesMessage even though the
                # publisher emits StringMessage, so we expect *at least* two
                # distinct type signatures. A regression that collapsed the
                # two endpoints' types would leave us with 1.
                assert_true(len(resp["type_signatures"]) >= 2,
                            f"expected >=2 distinct signatures, got {resp['type_signatures']}")
                # And `findings` must call out the metadata mismatch.
                assert_true(any("metadata mismatch" in f for f in resp["findings"]),
                            f"diagnose missed metadata mismatch: {resp['findings']}")
                # On a healthy topic, we should not see "no publisher" /
                # "no subscriber" / "share no active transport" findings.
                # A type-signature mismatch is *expected* in this e2e because
                # we deliberately read a StringMessage publisher with a
                # BytesMessage subscriber.
                fatal = [f for f in resp["findings"]
                         if "no publisher" in f
                         or "no subscriber" in f
                         or "share no active transport" in f]
                assert_true(not fatal, f"unexpected fatal findings: {resp['findings']}")

            runner.run("ecal_diagnose_topic (healthy)", t_diagnose_happy)

            # ---- 20. ecal_diagnose_topic — missing topic --------------------
            def t_diagnose_missing() -> None:
                resp = call_tool(client, "ecal_diagnose_topic", {
                    "topic": "definitely_does_not_exist_xyz",
                    "duration_ms": 0,  # skip live measurement
                }, timeout=10.0)
                assert_eq(resp["publishers"], [], "no pubs expected")
                assert_eq(resp["subscribers"], [], "no subs expected")
                assert_true(any("no publishers or subscribers" in f for f in resp["findings"]),
                            f"missing diagnostic finding: {resp}")
                # `live_stats` is omitted (Option::None + skip_serializing_if).
                assert_true(resp.get("live_stats") is None,
                            "live_stats should be omitted/None")

            runner.run("ecal_diagnose_topic (absent)", t_diagnose_missing)

            # ---- 21. ecal_call_service target_server_entity_id --------------
            def t_call_service_target() -> None:
                # Discover the entity_id of our one server first.
                ping = call_tool(client, "ecal_call_service", {
                    "service": SERVICE_NAME,
                    "method": "echo",
                    "text": "id-probe",
                    "timeout_ms": 2000,
                    "discovery_wait_ms": 1500,
                })
                ok = [r for r in ping["responses"] if r["success"]]
                assert_true(bool(ok), f"probe failed: {ping}")
                eid = ok[0]["server_entity_id"]
                # Now target that entity id.
                hit = call_tool(client, "ecal_call_service", {
                    "service": SERVICE_NAME,
                    "method": "echo",
                    "text": "targeted",
                    "timeout_ms": 2000,
                    "discovery_wait_ms": 1500,
                    "target_server_entity_id": eid,
                })
                assert_true(hit["instances"] >= 1, f"target call missed: {hit}")
                for r in hit["responses"]:
                    assert_eq(r["server_entity_id"], eid, "targeted server_entity_id")
                # And a bogus target should match zero instances post-filter,
                # while still reporting that we *did* discover at least one.
                # (This is the whole point of `discovered_instances`: it lets
                # callers distinguish "service is gone" from "your target
                # filter matched nothing".)
                miss = call_tool(client, "ecal_call_service", {
                    "service": SERVICE_NAME,
                    "method": "echo",
                    "text": "nope",
                    "timeout_ms": 1000,
                    # ecal_call_service builds a *fresh* ServiceClient each
                    # invocation, so discovery has to start over. Give it
                    # enough time to clear eCAL's ~1s registration refresh,
                    # otherwise we can't tell "filter rejected the instance"
                    # from "discovery just hadn't completed yet".
                    "discovery_wait_ms": 1500,
                    "target_server_entity_id": 0xdeadbeef_deadbeef,
                })
                assert_eq(miss["instances"], 0, "bogus target should match nothing")
                assert_true(miss["discovered_instances"] >= 1,
                            f"target filter swallowed discovery: {miss}")
                assert_eq(miss["responses"], [],
                          "bogus target must produce no responses")

            runner.run("ecal_call_service target_server_entity_id", t_call_service_target)

            # ---- 22. descriptor_base64 opt-in -------------------------------
            def t_descriptors_optin() -> None:
                without = call_tool(client, "ecal_list_publishers",
                                    {"name_pattern": "flood"})["publishers"]
                for p in without:
                    assert_true("descriptor_base64" not in p["data_type"]
                                or p["data_type"].get("descriptor_base64") is None,
                                f"descriptor leaked when not requested: {p}")
                # Eagerly request descriptors. We can't guarantee non-empty
                # for StringMessage (eCAL may not attach one), but the field
                # must serialize without error and obey the opt-in flag.
                with_ = call_tool(client, "ecal_list_publishers",
                                  {"name_pattern": "flood",
                                   "include_descriptors": True})["publishers"]
                for p in with_:
                    dt = p["data_type"]
                    if dt["descriptor_len"] > 0:
                        assert_true(isinstance(dt.get("descriptor_base64"), str),
                                    f"descriptor_base64 missing despite descriptor_len>0: {p}")

            runner.run("descriptor base64 opt-in", t_descriptors_optin)

            # ---- 23. ecal_get_logs filtering --------------------------------
            def t_logs_filtering() -> None:
                # min_level=fatal should drop everything below fatal. Verify
                # both that nothing below fatal leaks through *and* that the
                # baseline (no filter) is a superset of the filtered call.
                baseline = call_tool(client, "ecal_get_logs")["logs"]
                fatal_only = call_tool(client, "ecal_get_logs",
                                       {"min_level": "fatal"})["logs"]
                assert_true(len(fatal_only) <= len(baseline),
                            f"fatal-only filter returned more rows than baseline: "
                            f"{len(fatal_only)} > {len(baseline)}")
                for entry in fatal_only:
                    assert_eq(entry["level"].lower(), "fatal",
                              f"non-fatal leaked through filter: {entry}")
                # Mid-range filter (warning) is structurally valid and must
                # never return entries that are strictly below "warning".
                below_warning = {"info", "debug1", "debug2", "debug3", "debug4"}
                warn_up = call_tool(client, "ecal_get_logs",
                                    {"min_level": "warning"})["logs"]
                for entry in warn_up:
                    assert_true(entry["level"].lower() not in below_warning,
                                f"info/debug leaked past min_level=warning: {entry}")
                # Future timestamp filter must drop everything.
                future = 9_999_999_999_999_999
                resp_future = call_tool(client, "ecal_get_logs",
                                        {"since_timestamp_us": future})
                assert_eq(resp_future["logs"], [], "future timestamp should filter all logs")
                # process_name_pattern combines with the level filter.
                pat = call_tool(client, "ecal_get_logs",
                                {"process_name_pattern": "definitely_no_such_process"})
                assert_eq(pat["logs"], [], "bogus process pattern should filter all logs")

            runner.run("ecal_get_logs filters", t_logs_filtering)

            # ---- 24. type_name_pattern filter -------------------------------
            def t_type_name_pattern() -> None:
                # StringMessage publishers should match "string"; bytes should not.
                pubs = call_tool(client, "ecal_list_publishers",
                                 {"type_name_pattern": "string"})["publishers"]
                names = {p["topic_name"] for p in pubs}
                assert_true(PUB_TOPIC in names or FLOOD_TOPIC in names,
                            f"string-typed pub not found: {names}")
                for p in pubs:
                    assert_true("string" in p["data_type"]["type_name"].lower(),
                                f"non-matching type leaked: {p['data_type']}")
                # A nonsense pattern returns nothing.
                empty = call_tool(client, "ecal_list_publishers",
                                  {"type_name_pattern": "definitely_not_a_real_type"})
                assert_eq(empty["publishers"], [], "bogus type filter not empty")

            runner.run("type_name_pattern filtering", t_type_name_pattern)

            # ---- 25. similar_topics suggestion ------------------------------
            def t_similar_topics() -> None:
                # Slight typo of FLOOD_TOPIC.
                resp = call_tool(client, "ecal_diagnose_topic", {
                    "topic": FLOOD_TOPIC + "_typo",
                    "duration_ms": 0,
                })
                assert_true("similar_topics" in resp,
                            f"missing similar_topics: {resp}")
                assert_true(FLOOD_TOPIC in resp["similar_topics"],
                            f"flood topic not suggested: {resp['similar_topics']}")

            runner.run("ecal_diagnose_topic similar_topics", t_similar_topics)

            # ---- 26. discovered_instances on call_service -------------------
            def t_discovered_instances() -> None:
                resp = call_tool(client, "ecal_call_service", {
                    "service": SERVICE_NAME,
                    "method": "echo",
                    "text": "discover-count",
                    "timeout_ms": 2000,
                    "discovery_wait_ms": 1500,
                })
                assert_true("discovered_instances" in resp,
                            f"missing discovered_instances: {resp}")
                assert_eq(resp["discovered_instances"], resp["instances"],
                          "discovered != responded without target filter")

            runner.run("call_service discovered_instances", t_discovered_instances)

            # ---- 27. process entries: state_severity_level + config_file_path
            def t_process_extras() -> None:
                resp = call_tool(client, "ecal_list_processes", {})
                assert_true(len(resp["processes"]) >= 1,
                            f"no processes: {resp}")
                for p in resp["processes"]:
                    for k in ("state_severity_level", "config_file_path"):
                        assert_true(k in p, f"process entry missing {k}: {p}")

            runner.run("process extras", t_process_extras)

            # ---- 28. diagnose flags publisher-only topic --------------------
            def t_diagnose_publisher_only() -> None:
                # FLOOD_TOPIC has a live publisher and (by this point in the
                # run) no persistent subscriber, so diagnose should call out
                # the missing subscriber side. We pass duration_ms=0 to skip
                # live measurement (which would itself create a subscriber
                # registration *after* the snapshot is taken — the snapshot
                # is what feeds `findings`).
                resp = call_tool(client, "ecal_diagnose_topic", {
                    "topic": FLOOD_TOPIC, "duration_ms": 0,
                })
                assert_true(len(resp["publishers"]) >= 1,
                            f"flood publisher missing: {resp}")
                assert_true(any("no subscriber" in f for f in resp["findings"]),
                            f"diagnose missed 'no subscriber' finding: {resp['findings']}")

            runner.run("ecal_diagnose_topic (publisher-only)", t_diagnose_publisher_only)

            # ---- 29. ecal_topic_stats on absent topic -----------------------
            def t_topic_stats_absent() -> None:
                # Must return cleanly with zero samples — not error, not hang.
                resp = call_tool(client, "ecal_topic_stats", {
                    "topic": "definitely_no_such_topic_xyz",
                    "duration_ms": 200,
                }, timeout=10.0)
                assert_eq(resp["samples_observed"], 0,
                          f"unexpected samples on absent topic: {resp}")
                assert_eq(resp["observed_hz"], 0.0,
                          f"observed_hz should be 0 with no samples: {resp}")

            runner.run("ecal_topic_stats (absent topic)", t_topic_stats_absent)

            # ---- 30. ecal_call_service: real service, absent method --------
            def t_call_service_absent_method() -> None:
                # Service exists, method does not. We must still discover the
                # instance, but every response should report failure with a
                # non-empty error_msg — this is the most common real-world
                # bug we want to surface.
                resp = call_tool(client, "ecal_call_service", {
                    "service": SERVICE_NAME,
                    "method": "no_such_method",
                    "text": "missing-method-probe",
                    "timeout_ms": 1500,
                    "discovery_wait_ms": 1500,
                })
                assert_true(resp["discovered_instances"] >= 1,
                            f"server vanished mid-test: {resp}")
                assert_true(resp["instances"] >= 1,
                            f"call short-circuited before reaching server: {resp}")
                # Every response must be a failure carrying an error message.
                for r in resp["responses"]:
                    assert_eq(r["success"], False,
                              f"absent method reported success: {r}")
                    assert_true(bool(r.get("error")),
                                f"absent method returned no error message: {r}")

            runner.run("ecal_call_service (absent method)", t_call_service_absent_method)

        finally:
            client.close()

        rc = runner.summary()
        if rc != 0:
            log("Dumping container logs for failed run...")
            subprocess.run(["docker", "logs", "--tail", "300", CONTAINER_NAME], check=False)
        return rc
    except Exception as exc:  # noqa: BLE001
        log(f"FATAL: {exc!r}")
        subprocess.run(["docker", "logs", "--tail", "300", CONTAINER_NAME], check=False)
        return 2
    finally:
        stop_container()


if __name__ == "__main__":
    sys.exit(main())
