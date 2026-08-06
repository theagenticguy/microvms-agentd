# microvms-agentd-client

Python client for [`microvms-agentd`](../../README.md), the exec-and-file-transfer
daemon for AWS Lambda MicroVMs.

The daemon is layer one: it supplies exec and file transfer, which the service
itself does not provide. This is layer two, and it exists because the fiddly parts
are the same for every consumer and easy to get wrong — minting a proxy auth token
that expires at 60 minutes, sending both required proxy headers, resuming a
dropped output stream at the right byte offset, and telling a retryable 503 apart
from a fatal 401.

Two layers, and the split is load-bearing:

- `Session` and `ExecHandle` speak the wire protocol. No AWS. Testable against a
  local HTTP server, which is how the test suite runs with no credentials.
- `Sandbox` wraps the AWS lifecycle — build an image, launch a VM, suspend, resume,
  terminate — and is lifted from `conformance/`, which has been run against real
  AWS.

```
pip install -e clients/python
```

## One-shot command

```python
from microvms_agentd import Sandbox

with Sandbox(region="us-east-1") as box:
    box.build_image(
        name="agentd-demo",
        binary="target/aarch64-unknown-linux-musl/release/agentd",
        bucket="my-artifact-bucket",
        build_role_arn="arn:aws:iam::...:role/build",
    )
    session = box.run(execution_role_arn="arn:aws:iam::...:role/exec")

    result = session.run_sync("uname -a; pwd", shell=True)
    print(result.exit_code, result.stdout)
```

`run_sync` starts, waits, and acks in that order. The order matters: an ack
*releases* the output, so a poll issued after one reports `phase: acked` with
nothing in it. Getting that backwards is a silent empty-output bug, which is why
the library sequences it for you.

An omitted `cwd` means the child inherits the image `WORKDIR`. Passing `"/"`
explicitly is not the same thing and breaks any prebuilt-image task that expects
its own working directory.

## Streaming a long agent run

```python
from microvms_agentd import Gap, Exit, OutputChunk

handle = session.run(
    "while true; do date; sleep 5; done",
    shell=True,
    stdin=True,
    exec_id="agent-run-1",  # stable, so a retry after your own restart is safe
)

for event in handle.stream():
    match event:
        case OutputChunk():
            print(event.text(), end="")
        case Gap():
            # Those bytes are gone: the daemon's replay ring evicted them, or this
            # subscriber fell behind. Surfaced as a typed event rather than
            # skipped, because a client that cannot tell missing output from no
            # output will read a truncated log as a complete one.
            print(f"\n[lost {event.size} bytes at {event.start}]")
        case Exit():
            print(f"\nexit={event.exit_code} total={event.offset}")

handle.write_stdin(b"some input\n")
handle.close_stdin()  # nothing else closes it; the child hangs on read otherwise
```

`stream()` reconnects on a dropped connection using the last offset it actually
delivered. That is the whole reason the offset exists in the protocol — E2B's
equivalent is broken precisely because it lacks one, leaving a caller to choose
between losing everything after a drop and replaying from zero. The reconnect
condition is exact: a body that ended *with* an `exit` event is a finished command,
and a body that ended without one was cut. That distinction is why the wire format
is SSE and not a chunked byte stream.

`stream(raise_on_gap=True)` turns a gap into an `OutputGap` exception, for a caller
that must have complete output.

## Suspend, then resume a warm sandbox

```python
box.suspend()  # freeze; nothing is lost
# ... minutes later ...
session = box.resume()

# Everything survived: the in-memory agent token, the filesystem, exec records
# including unacked output, and running processes. Measured 2026-08-05, us-east-1.
assert session.health().bootstrapped
print(session.download_file("/tmp/state.json"))
```

Two sharp edges the API cannot hide:

- The launch-time `idlePolicy` **terminates** a suspended VM after
  `suspended_sec` (600 by default). A "resume later" affordance silently stops
  working once that window passes — raise it at `run()` time if you plan to
  suspend deliberately.
- The guest observes the whole suspension as a single time jump, so any timeout,
  lease, or TLS session held by a running command expires at once on resume.

## Errors

Retryable and fatal are different types, so a caller never parses a message:

| Type | Status | Meaning |
| --- | --- | --- |
| `NotBootstrapped` | 503 | the run hook has not landed yet — **retry** |
| `Unauthorized` | 401 | wrong bearer token — fatal |
| `ProtocolError` | 400 | malformed body, missing query key, refused tar member |
| `NotFound` | 404 | a genuinely absent exec id, file, or directory |
| `Conflict` | 409 | bootstrap hijack, second ack, ack while running, stdin not requested |
| `TooLarge` | 413 | over a body, tar, or stdin cap |
| `StdinClosed` | 410 | stdin saw EOF or the child stopped reading — fatal |
| `RequestTimeout` | 408 | the child did not drain stdin in time — **retry** |
| `TransportError` | — | no status arrived — **retry** |
| `AuthTokenMintError` | — | `CreateMicrovmAuthToken` failed — **retry** |

400 and 404 are deliberately never collapsed: clients map 404 onto
`FileNotFoundError`, so answering it for a protocol typo turns a bad request into a
phantom missing artifact. That is how one real defect hid for a full review round.

```python
from microvms_agentd import NotBootstrapped, Unauthorized

try:
    session.run_sync("true")
except NotBootstrapped:
    ...  # wait and retry
except Unauthorized:
    raise  # your credential is wrong; retrying will not help
```

## Tests

No AWS, no VM. The fake daemon is a real socket server, because two of the
properties under test are transport properties: a stream cut mid-body has to be
distinguishable from one that ended, and an SSE frame has to survive arriving in
two reads.

```
uv run --with pytest --with httpx --with boto3 pytest clients/python -q
uvx ruff check clients/python && uvx ruff format --check clients/python
```
