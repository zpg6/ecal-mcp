# Scenario 09: dual_publishers_one_topic

A teammate writes:

> "Localization on `consumer` is going haywire — `/odom` data looks
> right, then suddenly it looks like 4 Hz nonsense, then snaps back.
> We rebuilt the localization node and the bug stayed. The /odom
> producer team swears their service is at 100 Hz, on-spec. Find what
> is actually going onto `/odom` and tell us which one to kill."

You have only `ecal-mcp` and the eCAL CLI tools. **Don't** read source
code in `/work` for the answer; this is a live-network diagnosis.

Your final answer must include:

- The exact topic involved and the count of registered publishers and
  subscribers on it.
- A complete table of every publisher on that topic with its
  `host_name`, `process_name`, `unit_name`, and the rate that
  publisher individually contributes (not the aggregate the subscriber
  sees).
- The aggregate rate observed by the subscriber and any jitter / drop
  signal that confirms the diagnosis.
- Which publisher is the rogue (the one to kill) and the operator-level
  evidence (host + process) you'd quote to justify killing it.
- A one-line concrete remediation (e.g. "kill the ROGUE-DEBUG pub on
  host `rogue`; only one producer should publish `/odom`").

## Capture as you go

After each tool call, briefly note (in your reply / scratchpad):

- Which tool you reached for and **why** (your reasoning, not just the name)
- Any moment where the description, schema, or output shape made you guess
- Anything you wish the tool returned but didn't, or did return but cluttered things
- Whether anything in the API directly flagged "two publishers on one topic" as suspicious, or whether you had to derive that from raw counts
