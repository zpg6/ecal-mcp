---
name: run-ecal-gauntlet
description: Run the ecal-mcp gauntlet — fresh-user subagents read upstream eCAL docs, then AGENT_SKILL.md, then solve a Docker pub/sub puzzle and report back what felt eCAL-native vs invented. Use whenever the user says "run the gauntlet", "gauntlet vN", "rerun subagents on the scenarios", or asks to grade ecal-mcp's tools/skill against upstream eCAL.
---

# Run the ecal-mcp gauntlet

The gauntlet is the project's empirical test that `ecal-mcp` reads
"eCAL-native" to a fresh agent: each scenario is a Docker compose
topology that exposes one diagnostic puzzle. A subagent solves the
puzzle using only the `ecal-mcp` CLI and grades every name/shape it
sees against upstream eCAL headers.

This skill encodes the workflow that turned v3 → v4 → v5 retros into a
realignment plan. Run it the same way each round so retrospectives
remain comparable.

---

## Prerequisites

- Docker is running on the host.
- `ecal-mcp:e2e` image exists (build with `make image` if not).
- Upstream sources are checked out (subagents read them for grounding).
  Clone them on demand if missing — eCAL is pinned to a release tag,
  rustecal tracks its default branch (no upstream tag matches the eCAL
  6.1.1 cut yet):

  ```bash
  [ -d /tmp/ecal ]     || git clone --depth 1 --branch v6.1.1   https://github.com/eclipse-ecal/ecal.git     /tmp/ecal
  [ -d /tmp/rustecal ] || git clone --depth 1                   https://github.com/eclipse-ecal/rustecal.git /tmp/rustecal
  ```

  Run this before dispatching subagents. Use any local path you prefer;
  the templates only assume `/tmp/ecal` and `/tmp/rustecal` because
  that's the convention the gauntlet runs on.

- All `tests/gauntlet/scenarios/*` directories exist and bring up
  cleanly. New scenarios go under `tests/gauntlet/scenarios/NN_name/`
  with a `docker-compose.yml` and a `task.md`.

---

## Round-N workflow

```
Task progress:
- [ ] 1. Land code/skill changes in main branch
- [ ] 2. Smoke: make test && make e2e
- [ ] 3. Smoke each scenario (esp. new ones)
- [ ] 4. Archive previous results to results_v{N-1}/
- [ ] 5. Dispatch one subagent per scenario in parallel
- [ ] 6. Poll until all reports present + all containers torn down
- [ ] 7. Synthesize RETROSPECTIVE_V{N}.md via a synthesis subagent
- [ ] 8. Surface bottom-line + ranked Tier-1..4 plan to the user
```

### Step 1–3: Make sure the runtime is healthy

```bash
make test                    # cargo test --bin ecal-mcp, runs in builder image
make e2e                     # 30 e2e tests against ecal-mcp:e2e
# Smoke any new scenario:
python3 tests/gauntlet/run_scenario.py up   06_type_collision
python3 tests/gauntlet/run_scenario.py exec 06_type_collision tools | head
python3 tests/gauntlet/run_scenario.py down 06_type_collision
```

If any of these fail, **stop**: a broken scenario produces noise
retrospectives. Either fix the scenario or omit it from this round.

### Step 4: Archive previous results

```bash
mv tests/gauntlet/results tests/gauntlet/results_v{N-1}
mkdir -p tests/gauntlet/results
```

Don't archive the synthesis — `RETROSPECTIVE_V{N-1}.md` stays in
`tests/gauntlet/` so the synthesis subagent can compare tone.

### Step 5: Dispatch subagents (parallel)

One subagent per scenario, all in the same message, all
`run_in_background: true`. Use `subagent_type: generalPurpose`.

**Use the prompt template at `prompts/scenario.md`** — it enforces:

1. eCAL upstream first (~10 min reading `/tmp/ecal/...`).
2. Then `AGENT_SKILL.md`, with active grading against the upstream
   prime.
3. Then the puzzle, **without** reading `src/main.rs` (real-user mode).
4. Then a retrospective at
   `tests/gauntlet/results/NN_<scenario>.md` with sections matching
   the template.

For brand-new scenarios, add 1–2 sentences in the prompt about what
the puzzle stresses (e.g. type collision → focus critique on
`type_signatures` / `datatype_information`).

**Do NOT** include API hints or mention recent renames. Subagents are
new users; biasing them defeats the test.

### Step 6: Poll until done

```bash
ls tests/gauntlet/results/                     # 1 .md per scenario
docker ps --filter name=ecal-mcp-gauntlet- --format '{{.Names}}'
```

When the results count matches scenario count *and* the docker filter
returns empty, every subagent finished and tore down its stack. If a
subagent left containers behind, run
`python3 tests/gauntlet/run_scenario.py down NN_<name>` manually.

Typical wall-clock: 3–7 minutes per scenario, parallel.

### Step 7: Synthesize

Dispatch **one more** generalPurpose subagent to produce
`tests/gauntlet/RETROSPECTIVE_V{N}.md`. Its prompt must reference the
prior round's retrospective for tone and require these sections:

1. Intro (methodology, scenario list, which were new)
2. Wins (multi-agent consensus on what felt eCAL-native)
3. Hard-scenario findings (what the new scenarios surfaced)
4. Remaining divergences from upstream (≥2 agents, cited under
   `/tmp/ecal/...`)
5. Cognitive overhead taxonomy (vocabulary / shape / missing concept)
6. Single-agent observations
7. `AGENT_SKILL.md` feedback
8. Ranked next-realignment plan (Tier 1 rename/shape, Tier 2 polish,
   Tier 3 additive, Tier 4 skill corrections)
9. Bottom line

The synthesis subagent must NOT invent claims — only synthesize what
the per-scenario reports actually say.

### Step 8: Report to user

Surface to the user:

- Round number, scenario count, new scenarios.
- Top 3–5 multi-agent wins.
- Top 3–5 multi-agent divergences (with cite paths).
- Hard-scenario gaps (often the round's most actionable feedback).
- Minimal next-round fix list (the Tier-1/Tier-3 short list).

End with a citation to the synthesis: e.g.
`tests/gauntlet/RETROSPECTIVE_V{N}.md`.

---

## Scenario authoring rules

When asked to "add a scenario" or "make the gauntlet harder":

- One puzzle per scenario; one MCP-detectable signal per puzzle.
- Use `tests/gauntlet/scenarios/shared/ecal.yaml` unless the puzzle
  *requires* per-host config divergence (e.g. scenario 07).
- Prefer existing test binaries (`ecal-test-publisher`,
  `ecal-test-subscriber`, `ecal-test-service-server`) and upstream
  samples (`ecal_sample_person_send` for proto, etc.) over inventing
  new ones.
- Compose name `ecal-mcp-gauntlet-NN`; mcp container name
  `ecal-mcp-gauntlet-NN-mcp`. The runner relies on this prefix.
- `task.md` describes the operator's complaint and the required
  answer shape — never the API path. Subagents must reach for tools
  themselves.

Hard scenarios are valued: a scenario that **surfaces an MCP gap**
(e.g. 07's `no_common_transport` finding when each side has different
transport layers enabled) is a successful test, not a failed one.

---

## Files

- `prompts/scenario.md` — per-scenario subagent prompt template.
- `prompts/synthesis.md` — synthesis-subagent prompt template.
- Reference: `tests/gauntlet/RETROSPECTIVE_V8.md` (or whichever
  `RETROSPECTIVE_V*.md` is numerically latest in `tests/gauntlet/`)
  is the most recent good synthesis to model new rounds on.

---

## Anti-patterns

- **Don't** include API field names or recent-change hints in
  per-scenario prompts. That biases the test toward confirming what
  you just shipped.
- **Don't** let subagents read `src/main.rs` to figure out fields.
  They must use `ecal-mcp tools` schemas like a real user.
- **Don't** run scenarios sequentially "to be safe". Compose stacks
  are namespaced (`ecal-mcp-gauntlet-NN`); parallel works and saves
  ~5x wall-clock.
- **Don't** skip the docs-first prime. Without it, "feels eCAL-native"
  becomes a popularity vote, not a grounded comparison.
