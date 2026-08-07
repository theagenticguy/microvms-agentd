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

from .cost import (
    RATES,
    Amount,
    BillingLine,
    CostPhase,
    CostReport,
    Duration,
    EstimatedUSD,
    LineItem,
    Provenance,
    RateTable,
    ResidencyComparison,
    StaleRateTable,
    Total,
    Unpriced,
    compare_residency,
    estimate_run,
    run_report,
)
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
from .sandbox import (
    BaseImage,
    Image,
    NetworkConnector,
    Sandbox,
    build_artifact,
    default_dockerfile,
    default_hooks,
)
from .session import Session
from .sizing import (
    DEFAULT_BASELINE_MIB,
    SIZE_CLASSES,
    SizeClass,
    default_size_class,
    size_class_for,
)
from .transport import DEFAULT_AGENT_PORT, ProxyAuth, Transport

__version__ = "0.1.0"

# Imported *after* `__version__` rather than with the block above, and that ordering
# is load-bearing: `cli` reads `__version__` from this package to stamp `--version`
# and the manifest, so an import placed with its alphabetical siblings would run
# `cli` before the name exists and fail with a partially-initialized module. The
# alternative — duplicating the version string in `cli.py` — is the drift this
# import exists to avoid.
from .cli import main as cli_main

__all__ = [
    "DEFAULT_AGENT_PORT",
    "DEFAULT_BASELINE_MIB",
    "RATES",
    "SIZE_CLASSES",
    "AgentdError",
    "Amount",
    "AuthTokenMintError",
    "BaseImage",
    "BillingLine",
    "Conflict",
    "CostPhase",
    "CostReport",
    "Duration",
    "EstimatedUSD",
    "ExecHandle",
    "ExecResult",
    "ExecTimeout",
    "Exit",
    "Gap",
    "Health",
    "HttpError",
    "Image",
    "LineItem",
    "NetworkConnector",
    "NotBootstrapped",
    "NotFound",
    "OutputChunk",
    "OutputGap",
    "Phase",
    "ProtocolError",
    "Provenance",
    "ProxyAuth",
    "RateTable",
    "RequestTimeout",
    "ResidencyComparison",
    "Sandbox",
    "ServerError",
    "Session",
    "SizeClass",
    "StaleRateTable",
    "StdinAck",
    "StdinClosed",
    "StreamEvent",
    "StreamKind",
    "TooLarge",
    "Total",
    "Transport",
    "TransportError",
    "Unauthorized",
    "Unpriced",
    "__version__",
    "build_artifact",
    "cli_main",
    "compare_residency",
    "default_dockerfile",
    "default_hooks",
    "default_size_class",
    "estimate_run",
    "run_report",
    "size_class_for",
]
