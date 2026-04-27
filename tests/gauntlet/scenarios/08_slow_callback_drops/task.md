# Scenario 08: slow_callback_drops

A teammate writes:

> "The new telemetry consumer on `slow-sub` is missing samples — our
> dashboard shows gaps even though the producer log says it's sending
> at 1 kHz. The producer team swears it's healthy. Confirm whether
> samples are actually being lost, on which side, and why."

You have only `ecal-mcp` and the eCAL CLI tools. **Don't** read source
code in `/work` for the answer; this is a live-network diagnosis.

Your final answer must include:

- The exact topic involved, plus the publisher's rate and the
  subscriber's effective receive rate.
- A specific, quoted count (or rate) of dropped messages, with the
  field/finding you used to prove it.
- Which side (publisher or subscriber) is the bottleneck and the
  evidence that distinguishes "producer too fast" from "subscriber
  too slow".
- A one-line concrete remediation (e.g. "raise subscriber callback
  throughput / move handler off the receive thread").

## Capture as you go

After each tool call, briefly note (in your reply / scratchpad):

- Which tool you reached for and **why** (your reasoning, not just the name)
- Any moment where the description, schema, or output shape made you guess
- Anything you wish the tool returned but didn't, or did return but cluttered things
