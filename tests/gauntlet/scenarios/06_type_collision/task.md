# Scenario 06: type_collision

A teammate writes:

> "We're getting **garbage on `person`** — the consumer logs random bytes
> alongside ASCII. We *think* it's one of the publishers but nothing
> errors. Tell me which publisher is the wrong one and what should the
> fix be."

You have only `ecal-mcp` and the eCAL CLI tools. **Don't** read source
code in `/work` for the answer; this is a live-network diagnosis.

Your final answer must include:

- The exact topic involved and the count of registered publishers and
  subscribers on it.
- Each distinct **type signature** present on that topic (encoding +
  type name).
- Which publisher (host + process) is producing each type signature.
- Which type the consumer is built for, and therefore which publisher
  must change.
- A one-line concrete remediation (e.g. "rename the proto pub's topic
  to `/objects/person`").

## Capture as you go

After each tool call, briefly note (in your reply / scratchpad):

- Which tool you reached for and **why** (your reasoning, not just the name)
- Any moment where the description, schema, or output shape made you guess
- Anything you wish the tool returned but didn't, or did return but cluttered things
