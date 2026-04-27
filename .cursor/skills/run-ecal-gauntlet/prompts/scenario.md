Fresh new user of `ecal-mcp` — never used it before. Docker is running. The workspace root is the `ecal-mcp` repo (paths below are workspace-relative).

## Step 1 — Ground in upstream eCAL FIRST

Before the puzzle or the agent skill, read upstream broadly to internalize what eCAL itself calls things. Take ~10 minutes.

If `/tmp/ecal` or `/tmp/rustecal` is missing, clone them first (eCAL is pinned to a stable tag, rustecal tracks its default branch — safe to re-clone idempotently):

```bash
[ -d /tmp/ecal ]     || git clone --depth 1 --branch v6.1.1 https://github.com/eclipse-ecal/ecal.git     /tmp/ecal
[ -d /tmp/rustecal ] || git clone --depth 1                 https://github.com/eclipse-ecal/rustecal.git /tmp/rustecal
```

Start with the **Upstream eCAL index** section at the bottom of `AGENT_SKILL.md` — it points to the highest-signal upstream reads in roughly a 10-minute pass. Then drill into:

- `/tmp/ecal/doc/rst/getting_started/` (esp `monitor.rst`, `network.rst`, `services.rst`)
- `/tmp/ecal/doc/rst/advanced/ecal_internals.rst`
- `/tmp/ecal/ecal/core/include/ecal/types/monitoring.h`
- `/tmp/ecal/ecal/core/include/ecal/types.h`
- `/tmp/ecal/ecal/core/include/ecal/registration.h`
- `/tmp/rustecal/docs/`, `/tmp/rustecal/rustecal-core/src/core_types/monitoring.rs`

If your scenario centers on services, also: `/tmp/ecal/ecal/core/include/ecal/service/`.
If on transports/config: `/tmp/ecal/ecal/core/include/ecal/config/`.
If on protobuf type info: `/tmp/ecal/ecal/core/include/ecal/types.h` (`SDataTypeInformation`).

Build a mental model of `SMonitoring`, `STopic`, `STransportLayer`, `SDataTypeInformation`, `SServer/SClient`, `SEntityId`, `Monitoring::Entity` bitmask, registration callbacks, `data_clock` vs `registration_clock` vs `data_frequency`, and `shm_transport_domain`.

## Step 2 — Read the agent skill

Then read `AGENT_SKILL.md` (workspace root). As you read it, **continuously ask yourself**: does this read like it was written by someone who knows eCAL? Does each tool/field name match what upstream eCAL calls it? Where does it diverge or invent vocabulary? Note any cognitive overhead.

## Step 3 — Solve the puzzle

Read the brief: `tests/gauntlet/scenarios/<SCENARIO>/task.md`.

Bring the scenario up:

```
python3 tests/gauntlet/run_scenario.py up <SCENARIO>
```

Solve only via the ecal-mcp CLI:

```
python3 tests/gauntlet/run_scenario.py exec <SCENARIO> tools
python3 tests/gauntlet/run_scenario.py exec <SCENARIO> call <tool> -a key=value ...
```

Tear down at the end with `down <SCENARIO>`.

**Do NOT read `src/main.rs` or any other ecal-mcp source.** Use only `ecal-mcp tools` schemas, AGENT_SKILL.md, and your eCAL knowledge. If MCP outputs feel non-eCAL-native, cross-reference upstream headers under `/tmp/ecal/...`.

## Step 4 — Write a retrospective

Write to `tests/gauntlet/results/<SCENARIO>.md` with these sections:

1. **Solution** — concise tables of what's actually on the bus and the specific answer to the brief.
2. **Tool trail** — every tool call with the reasoning behind it.
3. **What worked well** — be specific about which names/shapes felt 1:1 with upstream eCAL.
4. **Friction & confusion** — points where the API made you guess, or you wanted X but got Y.
5. **Non-native to eCAL** — concrete divergences from upstream, cited under `/tmp/ecal/...`. Names, shapes, abstractions, missing concepts. Be ruthless.
6. **Cognitive overhead** — anything that forced you to translate eCAL terminology into MCP terminology in your head.
7. **Recommendations** — one-PR-sized fixes ranked.
8. **Tool sufficiency** — could you have answered without escaping to source / shell / `ecal_mon_cli`?

Return a one-paragraph summary to me. Be a tough reviewer; assume nothing about the API is sacred.

> **Scenario-specific critique anchors** (replace this block per dispatch with 1–2 sentences when the scenario has a distinctive failure mode worth focusing on, e.g. "specifically critique whether `type_metadata_mismatch` carries enough context to act on", "specifically critique whether `transport_layer[*].active` semantics match upstream `STransportLayer.active`"; otherwise delete this block).
