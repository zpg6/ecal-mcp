#!/usr/bin/env python3
"""Bring scenarios up/down and proxy `ecal-mcp` CLI calls into the mcp container.

Usage:
    python3 tests/gauntlet/run_scenario.py up   <name>
    python3 tests/gauntlet/run_scenario.py down <name>
    python3 tests/gauntlet/run_scenario.py status
    python3 tests/gauntlet/run_scenario.py exec <name> tools
    python3 tests/gauntlet/run_scenario.py exec <name> call ecal_diagnose_topic -a topic_name=/cmd_vel

Anything after `exec <name>` is passed verbatim to `ecal-mcp` inside the
scenario's `mcp` service container — the same surface the MCP server
exposes, but as a one-shot CLI returning JSON to stdout.

Designed to be the *only* file a subagent shells out to during a gauntlet
run, so the prompt for an agent reduces to: "use this script, here's the
puzzle".
"""
from __future__ import annotations

import json
import os
import subprocess
import sys
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[2]
SCENARIOS_DIR = Path(__file__).resolve().parent / "scenarios"
IMAGE_TAG = os.environ.get("ECAL_MCP_IMAGE", "ecal-mcp:e2e")
# eCAL's registration_refresh is 1s; allow several cycles for cross-bridge
# multicast registration to settle before the first agent call.
SETTLE_SECONDS = float(os.environ.get("ECAL_MCP_SETTLE", "5"))


def list_scenarios() -> list[str]:
    return sorted(
        d.name for d in SCENARIOS_DIR.iterdir()
        if d.is_dir() and d.name != "shared" and (d / "docker-compose.yml").exists()
    )


def compose_file(name: str) -> Path:
    p = SCENARIOS_DIR / name / "docker-compose.yml"
    if not p.exists():
        sys.exit(f"unknown scenario {name!r}; available: {list_scenarios()}")
    return p


def compose(name: str, *args: str, check: bool = True, capture: bool = False) -> subprocess.CompletedProcess[str]:
    cmd = ["docker", "compose", "-f", str(compose_file(name)), *args]
    return subprocess.run(
        cmd, check=check, text=True,
        capture_output=capture,
        env={**os.environ, "ECAL_MCP_IMAGE": IMAGE_TAG},
    )


def cmd_up(name: str) -> int:
    print(f"[gauntlet] bringing up scenario {name!r}", flush=True)
    compose(name, "up", "-d", "--force-recreate", "--remove-orphans")
    import time
    print(f"[gauntlet] sleeping {SETTLE_SECONDS}s for eCAL registration to converge", flush=True)
    time.sleep(SETTLE_SECONDS)
    return 0


def cmd_down(name: str) -> int:
    print(f"[gauntlet] tearing down scenario {name!r}", flush=True)
    compose(name, "down", "-v", "--remove-orphans", check=False)
    return 0


def cmd_status() -> int:
    out = subprocess.run(
        ["docker", "ps", "--format", "{{.Names}}\t{{.Status}}",
         "--filter", "name=ecal-mcp-gauntlet-"],
        capture_output=True, text=True, check=False,
    ).stdout
    print(out or "(no gauntlet containers running)")
    return 0


def cmd_exec(name: str, ecal_mcp_args: list[str]) -> int:
    """Run `ecal-mcp <args>` inside the scenario's mcp container.

    eCAL's startup banner is printed to stdout by the C library; we therefore
    drop everything before the FIRST line that looks like a JSON document
    (`{...}` or `[...]`). The `tools` subcommand skips eCAL init entirely so
    its stdout is already pure JSON. For tools that DO need eCAL, the JSON
    result is also a single compact line, so this filter is safe.
    """
    cmd = ["docker", "compose", "-f", str(compose_file(name)),
           "exec", "-T", "mcp", "/usr/local/bin/ecal-mcp", *ecal_mcp_args]
    proc = subprocess.run(cmd, text=True, capture_output=True, check=False,
                          env={**os.environ, "ECAL_MCP_IMAGE": IMAGE_TAG})
    if proc.returncode != 0:
        sys.stderr.write(proc.stderr)
        return proc.returncode
    # Filter eCAL banners out of stdout. eCAL prints `[eCAL]…` lines during
    # init/finalize, on either side of the JSON payload depending on tool
    # vs. process timing. The CLI always writes the JSON in one
    # `println!` call, so it lives in a single contiguous span starting at
    # the first `{`. We use json.JSONDecoder.raw_decode to consume exactly
    # that span — works for compact one-line JSON (default for `call`) and
    # pretty-printed multi-line JSON (default for `tools`) alike, and is
    # immune to trailing eCAL banners.
    text = proc.stdout
    decoder = json.JSONDecoder()
    idx = -1
    while True:
        idx = text.find("{", idx + 1)
        if idx == -1:
            break
        try:
            value, end = decoder.raw_decode(text[idx:])
        except json.JSONDecodeError:
            continue
        # Re-serialize so callers get a stable shape regardless of whether
        # the binary chose pretty or compact. Match prior contract: one
        # JSON document per line for easy parsing by tests.
        sys.stdout.write(json.dumps(value))
        sys.stdout.write("\n")
        _ = end  # span length for diagnostics if we ever need it
        return 0
    sys.stdout.write(text)
    return 1


def main(argv: list[str]) -> int:
    if len(argv) < 2:
        print(__doc__)
        print(f"\nAvailable scenarios: {list_scenarios()}")
        return 2
    op = argv[1]
    if op == "status":
        return cmd_status()
    if op == "up":
        return cmd_up(argv[2])
    if op == "down":
        return cmd_down(argv[2])
    if op == "exec":
        return cmd_exec(argv[2], argv[3:])
    sys.exit(f"unknown op {op!r}; use up | down | status | exec")


if __name__ == "__main__":
    sys.exit(main(sys.argv))
