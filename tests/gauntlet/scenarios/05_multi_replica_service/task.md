# Scenario 05: multi_replica_service

There are multiple replicas of the `path_planner` service running across
the network. Operator says: **"Call ONLY the replica on host `planner-b`
with the `echo` method, payload `'targeted'`. I need proof that only that
one replica answered, not the other(s)."**

Your final answer should include:

- The list of replicas you discovered and how they differ
- The exact response from the planner-b replica
- **Evidence** that the planner-a replica did not answer this targeted call
- Brief description of the mechanism you used to scope the call

## Capture as you go

After each tool call, briefly note:

- Which tool you reached for and **why** (your reasoning, not just the name)
- Any moment where the description, schema, or output shape made you guess
- Anything you wish the tool returned but didn't, or did return but cluttered things
