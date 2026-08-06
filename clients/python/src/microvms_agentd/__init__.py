"""Python client for microvms-agentd, the exec-and-file-transfer daemon for AWS
Lambda MicroVMs.

Two layers, and the split is deliberate:

* `Session` and `ExecHandle` speak the daemon's wire protocol. No AWS, no boto3 —
  importable and testable against a local HTTP server.
* `Sandbox` wraps the AWS lifecycle: build an image, launch a VM, suspend, resume,
  terminate. This is where boto3 lives, imported lazily.

    from microvms_agentd import Sandbox

    with Sandbox(region="us-east-1") as box:
        box.build_image(name="agent-1", binary="agentd", bucket=..., build_role_arn=...)
        session = box.run(execution_role_arn=...)
        print(session.run_sync("uname -a", shell=True).stdout)
"""

from __future__ import annotations

from .errors import (
    AgentdError,
    AuthTokenMintError,
    Conflict,
    ExecTimeout,
    HttpError,
    NotBootstrapped,
    NotFound,
    OutputGap,
    ProtocolError,
    RequestTimeout,
    ServerError,
    StdinClosed,
    TooLarge,
    TransportError,
    Unauthorized,
)
from .exec_handle import ExecHandle
from .models import (
    ExecResult,
    Exit,
    Gap,
    Health,
    OutputChunk,
    Phase,
    StdinAck,
    StreamEvent,
    StreamKind,
)
from .sandbox import Image, Sandbox, build_artifact, default_dockerfile, default_hooks
from .session import Session
from .transport import DEFAULT_AGENT_PORT, ProxyAuth, Transport

__version__ = "0.1.0"

__all__ = [
    "DEFAULT_AGENT_PORT",
    "AgentdError",
    "AuthTokenMintError",
    "Conflict",
    "ExecHandle",
    "ExecResult",
    "ExecTimeout",
    "Exit",
    "Gap",
    "Health",
    "HttpError",
    "Image",
    "NotBootstrapped",
    "NotFound",
    "OutputChunk",
    "OutputGap",
    "Phase",
    "ProtocolError",
    "ProxyAuth",
    "RequestTimeout",
    "Sandbox",
    "ServerError",
    "Session",
    "StdinAck",
    "StdinClosed",
    "StreamEvent",
    "StreamKind",
    "TooLarge",
    "Transport",
    "TransportError",
    "Unauthorized",
    "__version__",
    "build_artifact",
    "default_dockerfile",
    "default_hooks",
]
