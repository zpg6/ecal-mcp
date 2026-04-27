# Scenario 07: no_common_transport

A teammate writes:

> "We deployed `controller` and `actuator` to two new hosts and now
> `/ctrl/cmd_vel` is **completely silent at the consumer**. The pub is
> running, the sub is running, eCAL monitor sees them both, but no
> samples ever land. eCAL's been working everywhere else."

You have only `ecal-mcp` and the eCAL CLI tools. Find the root cause
and answer specifically:

- Are there registered publisher(s) and subscriber(s) on `/ctrl/cmd_vel`?
  How many of each?
- What is each side advertising as its **available transport layers**,
  and which (if any) are actually carrying data?
- What is the precise root cause (one sentence — be exact about layer
  semantics)?
- What concrete config change unblocks data flow?

Also: do the same eCAL `transport_layer` values you see in the MCP
match what you'd expect from the upstream documentation? If not, call
that out.

## Capture as you go

After each tool call, briefly note (in your reply / scratchpad):

- Which tool you reached for and **why** (your reasoning, not just the name)
- Any moment where the description, schema, or output shape made you guess
- Anything you wish the tool returned but didn't, or did return but cluttered things
