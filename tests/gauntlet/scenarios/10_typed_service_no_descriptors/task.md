# Scenario 10: typed_service_no_descriptors

A teammate writes:

> "There's a `PingService` running on host `server` that I need to
> call from a script — they say method `Ping` takes a typed protobuf
> request and returns a typed protobuf response, but they're on PTO.
> Tell me the request and response types (full proto type name and
> encoding), then actually call `Ping` once with a valid payload and
> show me the response. The bus is the only documentation I have."

You have only `ecal-mcp` and the eCAL CLI tools. **Don't** read source
code in `/work` for the answer; this is a live-network diagnosis.

Your final answer must include:

- The exact service name, method name, replica count, and `service_id`
  of the replica you targeted.
- The request type's full datatype info: encoding (e.g. `proto`),
  type name (e.g. `pb.foo.PingRequest`), and whether a descriptor /
  fingerprint was available from MCP without escaping its API.
- The same triple for the response type.
- The actual `ecal_call_service` you executed to invoke `Ping` and
  the response you got back.
- An honest verdict: was MCP alone sufficient to make this *typed*
  RPC call, or did you have to escape to upstream sample source / a
  shell-into-container to recover the proto type names? If you had to
  escape, name the specific MCP field you wished was populated.

## Capture as you go

After each tool call, briefly note (in your reply / scratchpad):

- Which tool you reached for and **why** (your reasoning, not just the name)
- Any moment where the description, schema, or output shape made you guess
- Anything you wish the tool returned but didn't, or did return but cluttered things
- Whether the service-side response gave you enough type evidence to
  reconstruct the request payload, or whether you had to brute-force
  it (e.g. send raw text / empty bytes and see what came back).

> The interesting tension here is between MCP's pub/sub surface
> (`datatype_information.{name, encoding, descriptor_len}` is on every
> publisher row) and its service surface (`methods[*]` carries no
> per-method datatype info today). Critique whether that asymmetry is
> defensible or whether `SMethod.{request,response}_datatype_information`
> from upstream `monitoring.h` ought to flow through.
