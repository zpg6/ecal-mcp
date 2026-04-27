# Scenario 04: rate_drift

The system spec says: **"`/heartbeat` MUST publish at 50 Hz."** Confirm
whether it's actually meeting that, and if not, by how much.

Your final answer should include:

- Measured rate (and your confidence in it)
- Measured payload size (min / max / mean if available)
- Measured jitter / inter-arrival behavior
- A direct yes/no on the 50 Hz spec, with the gap quantified
- A note on **how** you got the measurement and why that source is
  authoritative

## Capture as you go

After each tool call, briefly note:

- Which tool you reached for and **why** (your reasoning, not just the name)
- Any moment where the description, schema, or output shape made you guess
- Anything you wish the tool returned but didn't, or did return but cluttered things
