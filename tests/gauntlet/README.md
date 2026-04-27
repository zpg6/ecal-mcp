# ecal-mcp gauntlet

A scenario-based **agent UX evaluation** for the ecal-mcp tool surface.
Distinct from the `tests/e2e*.py` correctness suites: this harness sets up
deliberately puzzling eCAL topologies, then dispatches subagents (or human
runs) to **diagnose them using only the MCP tools**, and collects the
friction encountered along the way as feedback for tool descriptions and
ergonomic improvements.

## Scenarios

Each scenario is one tiny `docker-compose.yml` reusing the existing
`ecal-mcp:e2e` image and the bundled test binaries (`ecal-test-publisher`,
`ecal-test-subscriber`, `ecal-test-service-server`, plus the MCP itself).

| #  | Name                    | Setup                                                                                       | Puzzle                                                                                              | Tools the "right" answer pulls in            |
| -- | ----------------------- | ------------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------- | -------------------------------------------- |
| 01 | `discovery`             | 3 publishers, 1 subscriber, 1 service across 3 hosts                                        | "Survey what's on the network."                                                                     | `ecal_list_*`                                |
| 02 | `silent_topic`          | Subscriber on `/cmd_vel`, no publisher                                                      | "Robot won't move; `/cmd_vel` looks dead — find out why."                                           | `ecal_diagnose_topic`                        |
| 03 | `topic_typo`            | Publisher on `/sensors/lidar_front`, no `/sensor/lidar` anywhere                            | "Subscriber for `/sensor/lidar` sees nothing. Diagnose."                                            | `ecal_diagnose_topic` → `did_you_mean` finding |
| 04 | `rate_drift`            | Publisher on `/heartbeat` advertising at 1 Hz                                               | "`/heartbeat` should be 50 Hz. Verify and quantify the gap."                                        | `ecal_topic_stats` (vs the cached `registered_data_frequency_hz`) |
| 05 | `multi_replica_service` | Two `path_planner` service servers on different hosts                                       | "Call only the replica on host `planner-b` and prove only it answered."                             | `ecal_call_service` with `target_service_id` |

## Running a scenario

```bash
make image                                # if you haven't already
python3 tests/gauntlet/run_scenario.py up <name>     # bring stack up
python3 tests/gauntlet/run_scenario.py exec <name> tools   # any CLI cmd
python3 tests/gauntlet/run_scenario.py down <name>   # tear down
```

`run_scenario.py exec <name> ...` shells out into the MCP container and runs
the given `ecal-mcp` subcommand — anything `ecal-mcp tools | call <tool>`
accepts. That's the same surface a subagent uses.

## Subagent harness

Subagents are dispatched by the parent (Cursor) agent, one per scenario.
Each gets:

- The scenario's `task.md` (the puzzle, **not** the solution)
- Instructions to use `tests/gauntlet/run_scenario.py exec <name> ...` for
  every MCP interaction
- A "voice your friction out loud" instruction so confusion is captured

After running through all scenarios the parent consolidates feedback into
`tests/gauntlet/RETROSPECTIVE_V{N}.md` (one per round; the latest is the
canonical synthesis) and, if patterns emerge, updates tool descriptions
in `src/main.rs` and the agent-facing `AGENT_SKILL.md` at the repo root.
