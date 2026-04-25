#!/usr/bin/env python3
"""Cross-container ("realnet") e2e for ecal-mcp.

The default e2e suite (`tests/e2e.py`) runs every eCAL participant inside a
single container and lets them talk over loopback registration + SHM. That
catches almost all behavioral regressions, but it leaves three production
code paths blind:

  1. Cross-host transport selection. With everyone on one host, eCAL picks
     SHM and never needs UDP multicast or TCP. Real users with one process
     per node never see SHM for cross-host topics.

  2. Real protobuf type descriptors. The default suite only publishes
     `StringMessage`. `ecal_list_publishers(include_descriptors=true)` is
     therefore exercised against a degenerate type — it never sees the
     `FileDescriptorSet`-shaped blobs that real users care about.

  3. Cross-host SHM-domain handling. `ecal_diagnose_topic` has a finding
     for "N hosts (≥2) with distinct shm_transport_domain values" that
     simply cannot fire when there's only one host.

This harness fixes all three with a 4-host topology that samples
every realistic placement variant — pure pub, pure sub, pure svc, and
two flavors of mixed-role host:

                publisher  subscriber  service  MCP
   pub  (mixed)      ✓          ✓                    (pub + sub on same topic)
   sub  (pure)                  ✓
   svc  (pure)                            ✓
   mcp  (mixed)      ✓                           ✓   (MCP + pub on the same host)

PUB_TOPIC therefore has **2 publishers across 2 hosts** (pub @ 10 Hz,
mcp @ 5 Hz) and **2 dedicated remote subscribers across 2 hosts**
(pub, sub), in addition to MCP's transient subs from
ecal_subscribe / ecal_diagnose_topic. Why this mix matters:

  * Mixed-role host (`pub`): same node both publishes AND subscribes
    to the same topic — eCAL's self-loop case + the most realistic
    production placement. No test of ours hit this before.
  * Pure subscriber host (`sub`): non-MCP cross-host subscriber so
    `ecal_list_subscribers` and `ecal_diagnose_topic` actually see a
    remote subscriber registration (eCAL filters the caller's own
    monitoring entries out, so MCP-side ad-hoc subscribes wouldn't
    surface here on their own).
  * Pure service host (`svc`): no pub/sub at all. Service routing
    must work to a host that shares no topic transport state with
    anyone — the strongest possible "service routing is independent
    of topic routing" guarantee.

All four containers mount `tests/realnet/ecal.yaml`, which forces:
  - eCAL network mode (UDP-multicast registration on 239.0.0.1)
  - publisher TCP layer enabled
  - subscriber UDP + SHM disabled — cross-host data MUST land on TCP

A single suite run therefore exercises four independent bridge hops:
  data       : pub → mcp        (TCP, cross-host topic data)
  data       : mcp → sub        (TCP, MCP's own publisher → remote sub)
  service    : mcp → svc        (cross-host RPC to a no-other-state host)
  metadata   : pub, sub, svc → mcp  (UDP-multicast registration from all)

Set `ECAL_MCP_KEEP_CONTAINERS=1` to leave the compose stack up after the
run for poking with `docker compose -f tests/realnet/docker-compose.yml exec mcp …`.
"""

from __future__ import annotations

import base64
import os
import subprocess
import sys
import time
from typing import Any

# Reuse the JSON-RPC plumbing + assertion helpers from the in-container suite.
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from e2e import (  # noqa: E402
    StdioMcpClient,
    TestRunner,
    assert_eq,
    assert_true,
    call_tool,
    log,
)

REPO_ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
COMPOSE_DIR = os.path.join(REPO_ROOT, "tests", "realnet")
COMPOSE_FILE = os.path.join(COMPOSE_DIR, "docker-compose.yml")
IMAGE_TAG = os.environ.get("ECAL_MCP_IMAGE", "ecal-mcp:e2e")

PUB_HOST = "ecal-pub"
SUB_HOST = "ecal-sub"
SVC_HOST = "ecal-svc"
MCP_HOST = "ecal-mcp"

PUB_TOPIC = "ecal_mcp_realnet_pub"
SERVICE_NAME = "ecal_mcp_realnet_service"
PERSON_TOPIC = "person"  # set by ecal_sample_person_send


def compose(*args: str, check: bool = True, capture: bool = False) -> subprocess.CompletedProcess[str]:
    cmd = ["docker", "compose", "-f", COMPOSE_FILE, *args]
    log("$ " + " ".join(cmd))
    return subprocess.run(
        cmd,
        check=check,
        text=True,
        capture_output=capture,
        env={**os.environ, "ECAL_MCP_IMAGE": IMAGE_TAG},
    )


def ensure_image() -> None:
    """`docker compose up` will pull on miss; we want a hard failure instead.

    The realnet suite is meant to ride on the same image the regular e2e
    builds, so if it isn't here we tell the caller exactly what to do
    rather than silently triggering a slow registry round-trip.
    """
    out = subprocess.run(
        ["docker", "image", "inspect", IMAGE_TAG],
        capture_output=True, text=True,
    )
    if out.returncode != 0:
        raise SystemExit(
            f"image {IMAGE_TAG!r} not found locally — run `make image` first "
            f"(or set ECAL_MCP_IMAGE to a built tag)"
        )


def bring_up() -> None:
    log("Bringing up realnet compose stack")
    compose("up", "-d", "--force-recreate", "--remove-orphans")


def tear_down() -> None:
    if os.environ.get("ECAL_MCP_KEEP_CONTAINERS") == "1":
        log("ECAL_MCP_KEEP_CONTAINERS=1; leaving realnet stack alive")
        return
    log("Tearing down realnet compose stack")
    compose("down", "-v", "--remove-orphans", check=False)


def open_mcp_client() -> StdioMcpClient:
    log("Launching ecal-mcp via docker compose exec mcp")
    proc = subprocess.Popen(
        ["docker", "compose", "-f", COMPOSE_FILE, "exec", "-T",
         "mcp", "/usr/local/bin/ecal-mcp"],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        bufsize=0,
        env={**os.environ, "ECAL_MCP_IMAGE": IMAGE_TAG},
    )
    return StdioMcpClient(proc)


def dump_logs() -> None:
    log("--- pub container logs (tail 200) ---")
    compose("logs", "--tail", "200", "pub", check=False)
    log("--- mcp container logs (tail 200) ---")
    compose("logs", "--tail", "200", "mcp", check=False)


def has_person_sample() -> bool:
    """Detect at runtime whether eCAL's protobuf person sample shipped in
    the runtime image. The deb usually installs it to /usr/bin, but the
    binary name moved from `_snd` to `_send` in eCAL 6.1, so we probe for
    both."""
    out = subprocess.run(
        ["docker", "compose", "-f", COMPOSE_FILE, "exec", "-T", "pub",
         "/bin/sh", "-c",
         "command -v ecal_sample_person_send "
         "|| command -v ecal_sample_person_snd"],
        capture_output=True, text=True,
        env={**os.environ, "ECAL_MCP_IMAGE": IMAGE_TAG},
    )
    return out.returncode == 0 and bool(out.stdout.strip())


# ---------------------------------------------------------------------------
# Assertions
# ---------------------------------------------------------------------------


def find_pub(pubs: list[dict[str, Any]], topic: str, host: str) -> dict[str, Any]:
    for p in pubs:
        if p["topic_name"] == topic and p["host_name"] == host:
            return p
    raise AssertionError(
        f"no publisher for topic {topic!r} on host {host!r} in "
        f"{[(p['topic_name'], p['host_name']) for p in pubs]}"
    )


def main() -> int:
    ensure_image()
    bring_up()
    runner = TestRunner()
    try:
        # eCAL's registration_refresh is 1s; allow several cycles for
        # multicast registration to settle across the bridge.
        log("Waiting for cross-container registration to converge...")
        time.sleep(6.0)

        person_available = has_person_sample()
        log(f"Person sample available on pub: {person_available}")

        client = open_mcp_client()
        try:
            # ---- 1. handshake -----------------------------------------------
            def t_handshake() -> None:
                init = client.request_ok(
                    "initialize",
                    {
                        "protocolVersion": "2024-11-05",
                        "capabilities": {},
                        "clientInfo": {"name": "ecal-mcp-realnet", "version": "0.0.1"},
                    },
                )
                assert_eq(init["serverInfo"]["name"], "ecal-mcp", "serverInfo.name")
                client.notify("notifications/initialized")

            runner.run("initialize handshake", t_handshake)
            time.sleep(3.0)

            # ---- 2. cross-host process visibility ---------------------------
            # We pin cross-bridge multicast registration via two
            # *role-unique* unit_names — units that run on exactly one
            # host so there's no duplicate-unit-name race:
            #
            #   * `ecal_test_service_server` exists ONLY on SVC_HOST
            #   * `ecal_sample_person_send`  exists ONLY on PUB_HOST
            #     (gated on availability of the bundled deb sample)
            #
            # We deliberately don't gate on `ecal_test_publisher` or
            # `ecal_test_subscriber` here — those unit_names run on
            # two hosts each, and process-level snapshots can race on
            # duplicate unit_names (sometimes one host's registration
            # gets dropped from the snapshot). Topic-level registration
            # for the same processes does NOT race, and we pin those
            # deterministically via the multi-publisher and
            # multi-subscriber listing tests below.
            def t_cross_host_processes() -> None:
                deadline = time.time() + 15.0
                last_svc: set[str] = set()
                last_person: set[str] = set()
                require_person = person_available
                while time.time() < deadline:
                    procs = call_tool(client, "ecal_list_processes")["processes"]
                    last_svc = {
                        p["host_name"] for p in procs
                        if p["unit_name"] == "ecal_test_service_server"
                    }
                    last_person = {
                        p["host_name"] for p in procs
                        if "person" in p["unit_name"].lower()
                    }
                    svc_ok = SVC_HOST in last_svc
                    person_ok = (not require_person) or (PUB_HOST in last_person)
                    if svc_ok and person_ok:
                        return
                    time.sleep(0.5)
                raise AssertionError(
                    f"cross-host registration never converged in 15s: "
                    f"service hosts={last_svc!r} (want {{{SVC_HOST!r}}}); "
                    f"person hosts={last_person!r} "
                    f"(want {PUB_HOST!r}; require_person={require_person})"
                )

            runner.run("cross-host process visibility", t_cross_host_processes)

            # ---- 3. multi-publisher data flow (TCP, both hosts) -------------
            # Two publishers share PUB_TOPIC: one on `pub` ("realnet" @
            # 10 Hz), one on `mcp` ("realnet-mcp" @ 5 Hz). A single
            # ecal_subscribe call must interleave samples from both —
            # that is the proof of:
            #   * data flows pub → mcp (cross-host, the original test)
            #   * data flows mcp's local pub → mcp's subscribe
            #     (same-host, but TCP because subscriber.shm is disabled)
            #   * the merged stream isn't deduped or routed to one source
            # Per-publisher counters must each be monotonic *within their
            # own prefix* (not across; eCAL doesn't merge clocks).
            def t_multi_publisher_data_flow() -> None:
                sub = call_tool(client, "ecal_subscribe", {
                    "topic": PUB_TOPIC, "duration_ms": 5000, "max_samples": 50,
                }, timeout=20.0)
                assert_true(
                    sub["samples_collected"] >= 8,
                    f"too few samples to verify two interleaved sources: {sub}",
                )

                by_prefix: dict[str, list[int]] = {"realnet": [], "realnet-mcp": []}
                for s in sub["samples"]:
                    text = s.get("text") or ""
                    assert_eq(s.get("topic_name"), PUB_TOPIC, "sample.topic_name")
                    parts = text.split()
                    assert_true(
                        len(parts) == 2 and parts[0] in by_prefix,
                        f"unexpected sample text {text!r}: {s}",
                    )
                    by_prefix[parts[0]].append(int(parts[1]))

                # BOTH publishers must be represented. Without this, all
                # the multi-pub assertions downstream are vacuous.
                assert_true(
                    by_prefix["realnet"],
                    f"no samples from `pub` host publisher: {by_prefix}",
                )
                assert_true(
                    by_prefix["realnet-mcp"],
                    f"no samples from `mcp` host publisher: {by_prefix}",
                )
                # Strictly increasing within each prefix proves we
                # received a real interleaved stream, not a replay.
                for prefix, counters in by_prefix.items():
                    assert_true(
                        all(b > a for a, b in zip(counters, counters[1:])),
                        f"{prefix!r} counters not strictly increasing: {counters}",
                    )

            runner.run("multi-publisher data flow (TCP, both hosts)",
                       t_multi_publisher_data_flow)

            # ---- 4. topic_stats: full field validation cross-host -----------
            # Validate every numeric the tool advertises, not just rate.
            # Internal invariants (min ≤ mean ≤ max, stddev ≥ 0) are the
            # cheap way to catch cross-host corruption / accounting bugs.
            def t_topic_stats() -> None:
                stats = call_tool(client, "ecal_topic_stats", {
                    "topic": PUB_TOPIC, "duration_ms": 5000,
                }, timeout=20.0)
                assert_true(
                    stats["samples_observed"] >= 5,
                    f"too few samples for cross-host stats: {stats}",
                )
                hz = stats["observed_hz"]
                # Combined rate of two publishers: 10 Hz (`pub`) + 5 Hz
                # (`mcp`) = 15 Hz nominal. TCP session setup eats the
                # front of the window, so a wide band is expected; but
                # the floor is now ≥5 Hz (had to be ≥1 with one pub),
                # which is a real tightening that catches "one publisher
                # dropped out" regressions.
                assert_true(
                    5.0 <= hz <= 30.0,
                    f"observed_hz {hz!r} outside two-publisher band [5, 30]",
                )
                for k in ("min_size_bytes", "mean_size_bytes", "max_size_bytes",
                          "min_gap_us", "mean_gap_us", "max_gap_us", "gap_stddev_us",
                          "first_timestamp_us", "last_timestamp_us"):
                    assert_true(k in stats, f"missing field {k}: {stats}")
                assert_true(
                    stats["min_size_bytes"] <= stats["mean_size_bytes"] <= stats["max_size_bytes"],
                    f"size stats out of order: {stats}",
                )
                # "realnet 0" through "realnet <large>" — minimum payload
                # is "realnet 0" (9 bytes), well above zero.
                assert_true(
                    stats["min_size_bytes"] >= len("realnet 0"),
                    f"min payload smaller than 'realnet 0': {stats}",
                )
                assert_true(
                    stats["min_gap_us"] <= stats["mean_gap_us"] <= stats["max_gap_us"],
                    f"gap stats out of order: {stats}",
                )
                assert_true(
                    stats["gap_stddev_us"] >= 0,
                    f"negative stddev: {stats}",
                )
                assert_true(
                    stats["first_timestamp_us"] < stats["last_timestamp_us"],
                    f"timestamps not monotonic: {stats}",
                )

            runner.run("cross-host topic_stats full fields", t_topic_stats)

            # ---- 6. diagnose: healthy + cross-host SHM-domain finding -------
            # eCAL only flags `transport_layer.active=true` while data is
            # mid-transit, so `common_active_transports` is genuinely
            # racy on a freshly-negotiated TCP session — see the comment
            # on `intersect_active_transports` in src/main.rs. Rather
            # than fight that, we assert what's structurally invariant:
            #   * pub and sub both registered
            #   * the registration topology is genuinely cross-host
            #     (≥2 distinct shm_transport_domain values)
            #   * the cross-host SHM-domain finding fires (the dead-code
            #     path the in-container suite cannot reach)
            #   * the "data cannot flow" finding does NOT fire — its
            #     guard requires both_sides_advertised, so even with
            #     transient empty layer lists this should hold
            def t_diagnose_healthy() -> None:
                import threading
                holder: dict[str, Any] = {}

                def do_sub() -> None:
                    holder["sub"] = call_tool(client, "ecal_subscribe", {
                        "topic": PUB_TOPIC, "duration_ms": 6000, "max_samples": 1,
                    }, timeout=20.0)

                t = threading.Thread(target=do_sub)
                t.start()
                # Let the subscriber register and exchange at least one
                # registration broadcast cycle before snapshotting.
                time.sleep(3.0)
                resp = call_tool(client, "ecal_diagnose_topic", {
                    "topic": PUB_TOPIC, "duration_ms": 0,
                }, timeout=15.0)
                t.join(timeout=20)

                assert_true(
                    len(resp["publishers"]) >= 2 and len(resp["subscribers"]) >= 2,
                    f"diagnose missed pub(s) or sub(s): pubs={len(resp['publishers'])} "
                    f"subs={len(resp['subscribers'])} resp={resp}",
                )
                # Both publisher hosts must be represented. Single-host
                # would mean the second pub didn't register cross-bridge.
                pub_hosts_set = {p["host_name"] for p in resp["publishers"]}
                assert_eq(
                    pub_hosts_set, {PUB_HOST, MCP_HOST},
                    "diagnose pub hosts (must be {pub, mcp})",
                )
                # Both *remote* subscriber hosts must be represented. The
                # MCP-side ad-hoc subscriber may also surface (it's a
                # different process from the caller, so it's not always
                # filtered out), but we require AT MINIMUM both pub and
                # sub — the dedicated remote subscribers — to be there.
                sub_hosts_set = {s["host_name"] for s in resp["subscribers"]}
                assert_true(
                    {PUB_HOST, SUB_HOST}.issubset(sub_hosts_set),
                    f"diagnose missed remote subscriber(s); want ⊇ "
                    f"{{{PUB_HOST!r}, {SUB_HOST!r}}}, got {sub_hosts_set!r}",
                )
                assert_true(
                    len(resp["shm_transport_domains"]) >= 2,
                    f"only one SHM domain: topology not actually cross-host? {resp}",
                )
                assert_true(
                    any(
                        "shm_transport_domain" in f and "cross-host" in f.lower()
                        for f in resp["findings"]
                    ),
                    f"diagnose missed cross-host SHM-domain finding: {resp['findings']}",
                )
                assert_true(
                    not any(
                        "data cannot flow" in f for f in resp["findings"]
                    ),
                    f"diagnose claims no transport when TCP is negotiated: {resp['findings']}",
                )

            runner.run("ecal_diagnose_topic across hosts is clean", t_diagnose_healthy)

            # ---- 7. multi-publisher listing on a shared topic ---------------
            # Two `ecal-test-publisher` processes share PUB_TOPIC, one
            # per host. ecal_list_publishers must return BOTH as distinct
            # entries with different host_name AND different entity_id.
            # If the listing collapses by topic name (an easy bug), this
            # fails. If it collapses by host (also easy), this fails.
            def t_multi_publisher_listing() -> None:
                pubs = [
                    p for p in call_tool(client, "ecal_list_publishers")["publishers"]
                    if p["topic_name"] == PUB_TOPIC
                ]
                assert_eq(len(pubs), 2, f"expected exactly 2 pubs on {PUB_TOPIC!r}: {pubs}")

                hosts = {p["host_name"] for p in pubs}
                assert_eq(hosts, {PUB_HOST, MCP_HOST},
                          "multi-publisher hosts (must be {pub, mcp})")

                topic_ids = [p["topic_id"] for p in pubs]
                assert_eq(len(set(topic_ids)), 2,
                          f"topic_ids not distinct across publishers: {topic_ids}")

                # Each publisher must independently advertise TCP — that's
                # the layer in use for both the cross-host AND the
                # same-host (subscriber.shm disabled) data paths.
                for p in pubs:
                    kinds = {l["kind"].lower() for l in p["transport_layers"]}
                    assert_true(
                        "tcp" in kinds,
                        f"publisher on {p['host_name']!r} missing TCP layer: {p['transport_layers']}",
                    )

                # The MCP-side publisher is on the same host as the
                # caller. A subtle eCAL behavior worth pinning: local
                # publishers DO surface in the caller's monitoring
                # snapshot (only the caller's *own* registrations are
                # filtered, and the test publisher is a separate process).
                # If this regresses to filter-by-host, this fails loudly.
                mcp_side = [p for p in pubs if p["host_name"] == MCP_HOST]
                assert_eq(len(mcp_side), 1,
                          f"MCP-host publisher invisible from MCP — local-host filtering bug? {pubs}")

            runner.run("multi-publisher listing on shared topic",
                       t_multi_publisher_listing)

            # ---- 7b. multi-subscriber listing on a shared topic -------------
            # Two `ecal-test-subscriber` processes share PUB_TOPIC, on
            # `pub` (mixed pub+sub) and `sub` (pure subscriber).
            # ecal_list_subscribers must return BOTH with distinct
            # host_name and distinct topic_id. This is the FIRST
            # assertion in the suite that *deterministically* exercises
            # cross-bridge sub-direction registration: MCP's own ad-hoc
            # subscribers (from ecal_subscribe / ecal_diagnose_topic)
            # are short-lived measurement windows and may or may not
            # be present at snapshot time, so a stable remote
            # subscriber is what proves the sub-direction multicast
            # registration plumbing works.
            #
            # (eCAL's own filtering: `CSampleApplier::AcceptRegistrationSample`
            # gates same-process samples on `registration.loopback`,
            # which defaults to `true` — and we keep it true. So local
            # subs *can* surface; we just don't want to depend on
            # them being present at the right moment.)
            def t_multi_subscriber_listing() -> None:
                subs = [
                    s for s in call_tool(client, "ecal_list_subscribers")["subscribers"]
                    if s["topic_name"] == PUB_TOPIC
                ]
                assert_true(
                    len(subs) >= 2,
                    f"expected ≥2 subscribers on {PUB_TOPIC!r}: {subs}",
                )

                hosts = {s["host_name"] for s in subs}
                assert_true(
                    {PUB_HOST, SUB_HOST}.issubset(hosts),
                    f"remote subscriber hosts missing — want ⊇ "
                    f"{{{PUB_HOST!r}, {SUB_HOST!r}}}, got {hosts!r}",
                )

                topic_ids = [s["topic_id"] for s in subs]
                assert_eq(
                    len(set(topic_ids)), len(topic_ids),
                    f"subscriber topic_ids not distinct: {topic_ids}",
                )

                # Each subscriber must advertise TCP (subscriber.shm
                # is off, so the only viable cross-host layer is TCP).
                for s in subs:
                    kinds = {l["kind"].lower() for l in s["transport_layers"]}
                    assert_true(
                        "tcp" in kinds,
                        f"subscriber on {s['host_name']!r} missing TCP layer: "
                        f"{s['transport_layers']}",
                    )

            runner.run("multi-subscriber listing on shared topic",
                       t_multi_subscriber_listing)

            # ---- 7c. mixed-role host attribution ----------------------------
            # PUB_HOST runs BOTH a publisher AND a subscriber on
            # PUB_TOPIC. Mixed-role hosts are common in production but
            # have never been pinned by this suite. We assert the
            # exact role decomposition across the topology:
            #
            #   PUB_TOPIC publisher hosts   == {pub, mcp}
            #   PUB_TOPIC subscriber hosts  ⊇ {pub, sub}   (MCP may transit)
            #   PUB_HOST appears in BOTH the pubs and subs sets
            #
            # If listing logic ever drops a host from one direction
            # because the same host already appears in the other
            # (an easy "set-by-host" dedup bug), this catches it.
            def t_mixed_role_host_attribution() -> None:
                pubs = [
                    p for p in call_tool(client, "ecal_list_publishers")["publishers"]
                    if p["topic_name"] == PUB_TOPIC
                ]
                subs = [
                    s for s in call_tool(client, "ecal_list_subscribers")["subscribers"]
                    if s["topic_name"] == PUB_TOPIC
                ]
                pub_hosts = {p["host_name"] for p in pubs}
                sub_hosts = {s["host_name"] for s in subs}
                assert_eq(pub_hosts, {PUB_HOST, MCP_HOST},
                          f"PUB_TOPIC publisher hosts: {pub_hosts}")
                assert_true({PUB_HOST, SUB_HOST}.issubset(sub_hosts),
                            f"PUB_TOPIC subscriber hosts missing — got {sub_hosts}")
                assert_true(
                    PUB_HOST in pub_hosts and PUB_HOST in sub_hosts,
                    f"mixed-role host {PUB_HOST!r} not in BOTH directions: "
                    f"pubs={pub_hosts} subs={sub_hosts}",
                )
                # SVC_HOST must appear in NEITHER direction — that's
                # what makes it a pure-service host, the strongest
                # role-decoupling proof in the topology.
                assert_true(
                    SVC_HOST not in pub_hosts and SVC_HOST not in sub_hosts,
                    f"role bleed: {SVC_HOST!r} appears as a pub or sub: "
                    f"pubs={pub_hosts} subs={sub_hosts}",
                )

            runner.run("mixed-role host attribution (pub+sub on one host)",
                       t_mixed_role_host_attribution)

            # ---- 8. cross-host service call ---------------------------------
            def t_cross_host_service() -> None:
                payload = "realnet-ping"
                resp = call_tool(client, "ecal_call_service", {
                    "service": SERVICE_NAME,
                    "method": "echo",
                    "text": payload,
                    "timeout_ms": 3000,
                    "discovery_wait_ms": 2500,
                }, timeout=15.0)
                ok = [r for r in resp["responses"] if r["success"]]
                assert_true(bool(ok), f"no successful service response: {resp}")
                assert_eq(ok[0]["response_text"], payload, "echo response_text")
                # The service runs on SVC_HOST — a host that has NO
                # publishers and NO subscribers. If MCP routed the call
                # to PUB_HOST or any other host where it had open TCP
                # sessions, server_host_name would mismatch. This is the
                # 3-host topology's load-bearing service assertion.
                assert_eq(
                    ok[0].get("server_host_name"), SVC_HOST,
                    "server_host_name (cross-host service, must be svc-only host)",
                )

            runner.run("cross-host service call", t_cross_host_service)

            # ---- 8b. ecal_list_services attribution (svc-only host) ---------
            # Direct, no-RPC proof that service registration's host
            # attribution lives on SVC_HOST — independent of whatever
            # per-call routing decision drove `server_host_name` in the
            # previous test. (Role-bleed against pub/sub on PUB_TOPIC
            # is asserted separately by mixed-role host attribution.)
            def t_list_services_attribution() -> None:
                ours = [
                    s for s in call_tool(client, "ecal_list_services")["services"]
                    if s["service_name"] == SERVICE_NAME
                ]
                assert_true(
                    len(ours) >= 1,
                    f"{SERVICE_NAME!r} not visible from MCP: {ours}",
                )
                hosts = {s["host_name"] for s in ours}
                assert_eq(
                    hosts, {SVC_HOST},
                    f"service host_name set must be exactly {{{SVC_HOST!r}}}",
                )

            runner.run("ecal_list_services attribution (svc-only host)",
                       t_list_services_attribution)

            # ---- 7. service call: target_server_entity_id cross-host --------
            # Two-step: discover entity_id via a wide call, then drive
            # exactly that replica with target_server_entity_id.
            # Critical for users running >1 replica of the same service
            # across hosts. Note on semantics: eCAL's `CClientInstance`
            # is already per-server, but the ecal-mcp handler issues
            # one `ClientInstance::call` per discovered replica and
            # post-filters the responses by entity_id. So the
            # behavioral guarantee under test is "exactly one matching
            # response, no leakage from sibling replicas, plus
            # `discovered_instances` still reports the full set" —
            # not "only one network RPC was issued." This still
            # catches the failure modes that matter (filter dropped,
            # entity_id mismatched after a bridge traversal, bogus
            # ids matching real responses). Mirrors tier-1 case 21
            # but cross-bridge.
            def t_target_entity_id_cross_host() -> None:
                ping = call_tool(client, "ecal_call_service", {
                    "service": SERVICE_NAME,
                    "method": "echo",
                    "text": "discover-eid",
                    "timeout_ms": 2500,
                    "discovery_wait_ms": 2000,
                }, timeout=15.0)
                ok = [r for r in ping["responses"] if r["success"]]
                assert_true(bool(ok), f"discovery call failed: {ping}")
                assert_eq(ok[0]["server_host_name"], SVC_HOST,
                          "discovery probe must also land on svc-only host")
                eid = ok[0]["server_entity_id"]
                assert_true(eid > 0, f"server_entity_id not populated: {ok[0]}")

                hit = call_tool(client, "ecal_call_service", {
                    "service": SERVICE_NAME,
                    "method": "echo",
                    "text": "targeted-realnet",
                    "timeout_ms": 2500,
                    "discovery_wait_ms": 2000,
                    "target_server_entity_id": eid,
                }, timeout=15.0)
                assert_true(hit["instances"] >= 1, f"targeted call missed: {hit}")
                hit_ok = [r for r in hit["responses"] if r["success"]]
                assert_true(bool(hit_ok), f"targeted call got no success response: {hit}")
                for r in hit_ok:
                    assert_eq(r["server_entity_id"], eid, "responding entity_id")
                    assert_eq(r["server_host_name"], SVC_HOST,
                              "targeted call still on svc-only host")
                    assert_eq(r["response_text"], "targeted-realnet", "echo payload")

                # Negative: a fake entity_id must match nothing — proves
                # the filter is real, not a no-op.
                miss = call_tool(client, "ecal_call_service", {
                    "service": SERVICE_NAME,
                    "method": "echo",
                    "text": "should-not-route",
                    "timeout_ms": 1500,
                    "discovery_wait_ms": 1500,
                    "target_server_entity_id": 0xdeadbeef_deadbeef,
                }, timeout=15.0)
                assert_eq(miss["instances"], 0, "bogus entity_id matched something")
                assert_true(
                    miss["discovered_instances"] >= 1,
                    f"discovered_instances must show real server still visible: {miss}",
                )

            runner.run("service target_server_entity_id cross-host", t_target_entity_id_cross_host)

            # ---- 8. service client registration crosses the bridge ----------
            # When MCP (on ecal-mcp) calls a service in `pub`, MCP becomes
            # a *service client*. The client-side registration must reach
            # `pub`'s monitoring layer too — MCP queries from its own end,
            # so `host_name` on the client entry should be ecal-mcp. This
            # is the only test that verifies the inverse direction of
            # registration plumbing (client→server, mcp→pub).
            def t_list_service_clients_cross_host() -> None:
                import threading
                holder: dict[str, Any] = {}

                def do_call() -> None:
                    holder["resp"] = call_tool(client, "ecal_call_service", {
                        "service": SERVICE_NAME,
                        "method": "echo",
                        "text": "client-registration-probe",
                        "timeout_ms": 2500,
                        "discovery_wait_ms": 2000,
                    }, timeout=15.0)

                t = threading.Thread(target=do_call)
                t.start()
                # Hold-open: clients deregister fast. Sample a few times
                # while the call is mid-flight.
                ours: list[dict[str, Any]] = []
                deadline = time.time() + 6.0
                while time.time() < deadline and not ours:
                    clients = call_tool(client, "ecal_list_service_clients").get("clients", [])
                    ours = [
                        c for c in clients
                        if c["service_name"] == SERVICE_NAME
                    ]
                    if not ours:
                        time.sleep(0.5)
                t.join(timeout=15)

                assert_true(
                    bool(ours),
                    f"no service client registration on {SERVICE_NAME!r} during cross-host call",
                )
                # eCAL stamps the *client* registration with the client
                # process's host (= MCP's host). If this read is itself
                # filtered to "remote-only" the result is empty — but it
                # isn't, so we should see ecal-mcp here.
                client_hosts = {c["host_name"] for c in ours}
                assert_true(
                    MCP_HOST in client_hosts,
                    f"client registration missing MCP host: {client_hosts}",
                )

            runner.run("service client registration crosses bridge", t_list_service_clients_cross_host)

            # ---- 9. multi-topic listing parity ------------------------------
            # Same host, two distinct publishers (ecal_test_publisher and
            # ecal_sample_person_send) must both surface in a single
            # cross-host snapshot, with correct host_name on each. Catches
            # any "first publisher wins" / "type collision drops the
            # second" bugs in the listing pipeline.
            def t_multi_topic_listing() -> None:
                if not person_available:
                    log("    SKIP: ecal_sample_person_send not in image")
                    return
                pubs = call_tool(client, "ecal_list_publishers")["publishers"]
                by_topic = {(p["topic_name"], p["host_name"]): p for p in pubs}
                assert_true(
                    (PUB_TOPIC, PUB_HOST) in by_topic,
                    f"{PUB_TOPIC!r} on {PUB_HOST!r} missing: {sorted(by_topic.keys())}",
                )
                assert_true(
                    (PERSON_TOPIC, PUB_HOST) in by_topic,
                    f"{PERSON_TOPIC!r} on {PUB_HOST!r} missing: {sorted(by_topic.keys())}",
                )
                # Type encodings must differ (string vs proto) — proves we
                # didn't accidentally collapse two distinct registrations
                # into one.
                pub_enc = (by_topic[(PUB_TOPIC, PUB_HOST)].get("data_type") or {}).get("encoding", "")
                person_enc = (by_topic[(PERSON_TOPIC, PUB_HOST)].get("data_type") or {}).get("encoding", "")
                assert_true(
                    pub_enc.lower() != person_enc.lower(),
                    f"both topics report same encoding {pub_enc!r}; types collapsed?",
                )
                assert_true(
                    person_enc.lower().startswith("proto"),
                    f"{PERSON_TOPIC!r} encoding should be proto, got {person_enc!r}",
                )

            runner.run("multi-topic listing parity", t_multi_topic_listing)

            # ---- 10. real protobuf descriptor (person sample) ---------------
            # Tightened: instead of just "bytes contain 'Person'", we
            # parse enough of the FileDescriptorProto wire format to
            # extract the package name and confirm it matches what the
            # eCAL person sample actually publishes.
            def t_person_protobuf() -> None:
                if not person_available:
                    log("    SKIP: ecal_sample_person_send not in image")
                    return
                pubs = call_tool(client, "ecal_list_publishers", {
                    "name_pattern": PERSON_TOPIC,
                    "include_descriptors": True,
                })["publishers"]
                ours = [
                    p for p in pubs
                    if p["topic_name"] == PERSON_TOPIC and p["host_name"] == PUB_HOST
                ]
                assert_true(
                    bool(ours),
                    f"person sample publisher not visible: "
                    f"{[(p['topic_name'], p['host_name']) for p in pubs]}",
                )
                p = ours[0]
                dt = p.get("data_type") or {}
                type_name = dt.get("type_name", "")
                encoding = dt.get("encoding", "")
                assert_true(
                    "person" in type_name.lower(),
                    f"unexpected protobuf type name: {type_name!r}",
                )
                assert_true(
                    encoding.lower().startswith("proto"),
                    f"expected proto encoding, got {encoding!r}",
                )
                desc_b64 = dt.get("descriptor_base64")
                assert_true(
                    bool(desc_b64),
                    f"include_descriptors requested but no descriptor returned: {p}",
                )
                raw = base64.b64decode(desc_b64)
                assert_eq(len(raw), dt["descriptor_len"], "descriptor_len matches blob")
                # FileDescriptorSet is `repeated FileDescriptorProto file = 1;`,
                # FileDescriptorProto starts with `string name = 1;`. Both
                # length-delimited, so the blob always begins with tag 0x0a
                # (field 1, wire type 2). Anything else means we're not
                # looking at a real descriptor.
                assert_true(
                    raw[0] == 0x0a,
                    f"descriptor doesn't start with proto tag 0x0a: {raw[:8].hex()}",
                )
                # The eCAL person sample's .proto declares
                # `package pb.People;` and `message Person { ... }`.
                # Both literals must appear in the serialized descriptor.
                for needle in (b"pb.People", b"Person"):
                    assert_true(
                        needle in raw,
                        f"descriptor missing {needle!r}; did the upstream "
                        f"sample's .proto change? len={len(raw)}",
                    )

            runner.run("real protobuf descriptor (person sample)", t_person_protobuf)

            # ---- 11. diagnose typed protobuf topic + live measurement -------
            # Exercises two paths the StringMessage diagnose can't:
            #   * type_signatures populated with a real (proto, pb.People.Person)
            #   * live_stats present (duration_ms > 0 path)
            # Plus all the cross-host invariants we asserted on PUB_TOPIC.
            def t_diagnose_person_with_live() -> None:
                if not person_available:
                    log("    SKIP: ecal_sample_person_send not in image")
                    return
                resp = call_tool(client, "ecal_diagnose_topic", {
                    "topic": PERSON_TOPIC, "duration_ms": 2500,
                }, timeout=20.0)
                assert_true(
                    len(resp["publishers"]) >= 1,
                    f"diagnose missed person publisher: {resp}",
                )
                # type_signatures must contain exactly one entry (one type
                # in play); encoding must be proto-flavored.
                sigs = resp.get("type_signatures") or []
                assert_eq(len(sigs), 1, f"unexpected type_signatures: {sigs}")
                assert_true(
                    sigs[0]["encoding"].lower().startswith("proto"),
                    f"expected proto encoding, got {sigs[0]!r}",
                )
                assert_true(
                    "person" in sigs[0]["type_name"].lower(),
                    f"expected Person type_name, got {sigs[0]!r}",
                )
                # live_stats present and shaped right.
                live = resp.get("live_stats")
                assert_true(live is not None, f"live_stats missing despite duration_ms>0: {resp}")
                for k in ("samples_observed", "observed_hz",
                          "min_size_bytes", "max_size_bytes"):
                    assert_true(k in live, f"live_stats missing {k}: {live}")
                # The eCAL person sample publishes at 2 Hz by default.
                # We don't subscribe in this test (diagnose's window
                # creates its own sub), so we only require any samples;
                # rate band would race the sample's send cadence.
                assert_true(
                    live["samples_observed"] >= 1,
                    f"diagnose live window saw zero samples on {PERSON_TOPIC!r}: {live}",
                )
                # Cross-host topology invariants — same as PUB_TOPIC.
                assert_true(
                    len(resp["shm_transport_domains"]) >= 1,
                    f"missing shm_transport_domains: {resp}",
                )
                pub_hosts = {p["host_name"] for p in resp["publishers"]}
                assert_eq(pub_hosts, {PUB_HOST}, "person publisher host")
                assert_true(
                    not any("data cannot flow" in f for f in resp["findings"]),
                    f"unexpected 'data cannot flow' finding: {resp['findings']}",
                )

            runner.run("diagnose typed topic + live_stats", t_diagnose_person_with_live)

        finally:
            client.close()

        rc = runner.summary()
        if rc != 0:
            dump_logs()
        return rc
    except Exception as exc:  # noqa: BLE001
        log(f"FATAL: {exc!r}")
        dump_logs()
        return 2
    finally:
        tear_down()


if __name__ == "__main__":
    sys.exit(main())
