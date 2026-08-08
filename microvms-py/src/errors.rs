//! The exception hierarchy, and the one conversion from a core [`Error`].
//!
//! # One exception per `ErrorKind`, all under one base
//!
//! Thirteen `ErrorKind`s become thirteen exception types under `MicrovmError`, so
//! `except MicrovmError` catches everything this library raises and
//! `except WindowClosedError` catches the one case a caller can act on. That mirrors
//! `errors.py`'s class-per-condition shape, which the conformance oracle asserts on.
//!
//! # The codes travel on the exception, not in the message
//!
//! Every raised exception carries `.code` (the `ERR_*` string), `.kind` (the same, for a
//! caller who thinks in kinds), `.wire_kind` (the daemon status class, or `None`), and
//! `.retryable`. Nobody should parse a message — that rule is why these are attributes.
//! The message itself is the core's, unchanged, because the core's messages are the ones
//! that name the `docs/PLATFORM.md` finding and a binding that reworded them would
//! discard the whole point of the closure.
//!
//! # No binding-local validation anywhere in this file
//!
//! Every error here originates in `microvms-core`. There is no path where this module
//! decides something is invalid: it translates a refusal the core already made. A gap in
//! the core stays a gap and gets noted in the packet (the kickoff rule), because a check
//! added here would be the copy most callers hit and the copy nothing else tests.

use microvms_core::{Error, ErrorKind};
use pyo3::create_exception;
use pyo3::exceptions::PyException;
use pyo3::prelude::*;
use pyo3::types::PyType;

create_exception!(
    microvms,
    MicrovmError,
    PyException,
    "Base of every exception this library raises. Carries `.code`, `.kind`, `.wire_kind`, \
     and `.retryable` so nothing has to parse a message."
);

create_exception!(
    microvms,
    UnexpectedError,
    MicrovmError,
    "ERR_UNEXPECTED — no handler claimed this. A bug in the client, not the platform."
);
create_exception!(
    microvms,
    InvalidArgError,
    MicrovmError,
    "ERR_INVALID_ARG — refused locally, before any AWS call. Every trap closure lands here."
);
create_exception!(
    microvms,
    RetryableError,
    MicrovmError,
    "ERR_RETRYABLE — transient. Run the identical request again."
);
create_exception!(
    microvms,
    CredentialsError,
    MicrovmError,
    "ERR_CREDENTIALS — an identity is wrong or absent; waiting will not fix it."
);
create_exception!(
    microvms,
    ProtocolError,
    MicrovmError,
    "ERR_PROTOCOL — the daemon rejected the request on its merits. Read `.wire_kind` to \
     tell a 400 from a 404."
);
create_exception!(
    microvms,
    BuildWedgedError,
    MicrovmError,
    "ERR_BUILD_WEDGED — the image build was never scheduled: the clientToken replay \
     signature."
);
create_exception!(
    microvms,
    LaunchDiedError,
    MicrovmError,
    "ERR_LAUNCH_DIED — the MicroVM reached a terminal state before RUNNING. Read the \
     message's stateReason."
);
create_exception!(
    microvms,
    WindowClosedError,
    MicrovmError,
    "ERR_WINDOW_CLOSED — the launch-time suspended window passed, so there is nothing to \
     resume. A longer window has to be set at launch on the next VM."
);
create_exception!(
    microvms,
    PlatformError,
    MicrovmError,
    "ERR_PLATFORM — a control-plane failure with no more specific class."
);
create_exception!(
    microvms,
    TimeoutError,
    MicrovmError,
    "ERR_TIMEOUT — a client-side deadline elapsed. The VM and the exec are untouched."
);
create_exception!(
    microvms,
    InterruptedError,
    MicrovmError,
    "ERR_INTERRUPTED — interrupted after launch; teardown ran and any leak is named."
);
create_exception!(
    microvms,
    PreconditionError,
    MicrovmError,
    "ERR_PRECONDITION — a prerequisite is missing."
);
create_exception!(
    microvms,
    ExecFailedError,
    MicrovmError,
    "ERR_EXEC_FAILED — the sandbox worked and the command in it exited non-zero. The one \
     failure that means nothing is wrong with the platform."
);

/// The exception type for one kind.
///
/// A `match` rather than a lookup table, so a kind added to the core's closed enum is a
/// compile error here rather than a silent fall-through to the base class.
fn exception_for(py: Python<'_>, kind: ErrorKind) -> Bound<'_, PyType> {
    match kind {
        ErrorKind::Unexpected => py.get_type::<UnexpectedError>(),
        ErrorKind::InvalidArg => py.get_type::<InvalidArgError>(),
        ErrorKind::Retryable => py.get_type::<RetryableError>(),
        ErrorKind::Credentials => py.get_type::<CredentialsError>(),
        ErrorKind::Protocol => py.get_type::<ProtocolError>(),
        ErrorKind::BuildWedged => py.get_type::<BuildWedgedError>(),
        ErrorKind::LaunchDied => py.get_type::<LaunchDiedError>(),
        ErrorKind::WindowClosed => py.get_type::<WindowClosedError>(),
        ErrorKind::Platform => py.get_type::<PlatformError>(),
        ErrorKind::Timeout => py.get_type::<TimeoutError>(),
        ErrorKind::Interrupted => py.get_type::<InterruptedError>(),
        ErrorKind::Precondition => py.get_type::<PreconditionError>(),
        ErrorKind::ExecFailed => py.get_type::<ExecFailedError>(),
    }
}

/// A core error, as the Python exception for its kind, with the codes attached.
///
/// The attachment needs the GIL, so this is a named function rather than the body of a
/// `From` impl — see [`CoreError`] for the ergonomic wrapper the `?` operator uses.
pub(crate) fn to_py_err(py: Python<'_>, error: &Error) -> PyErr {
    let exception = exception_for(py, error.kind());
    let raised = PyErr::from_type(exception, error.to_string());
    // Attributes rather than a structured message. Setting them can only fail if the
    // exception instance refuses an attribute, which a `PyException` subclass does not;
    // a failure here is swallowed rather than replacing the real error with a
    // decoration failure — the same reasoning as the core's teardown returning a report.
    let value = raised.value(py);
    let _ = value.setattr("code", error.code());
    let _ = value.setattr("kind", error.code());
    let _ = value.setattr(
        "wire_kind",
        error.wire_kind().map(microvms_core::WireKind::as_str),
    );
    let _ = value.setattr("retryable", error.retryable());
    raised
}

/// A core [`Error`] on its way to Python.
///
/// `From<Error> for PyErr` is impossible — both types are foreign to this crate — so the
/// `?` operator needs a local newtype. Every fallible binding method returns
/// `Result<T, CoreError>`, and PyO3 converts through the impl below.
pub(crate) struct CoreError(pub(crate) Error);

impl From<Error> for CoreError {
    fn from(error: Error) -> Self {
        CoreError(error)
    }
}

impl From<CoreError> for PyErr {
    fn from(error: CoreError) -> PyErr {
        // `Python::attach` rather than a threaded-through token: this runs during `?`
        // conversion where no token is in scope, and the conversion is always on a
        // thread that is about to return into Python.
        Python::attach(|py| to_py_err(py, &error.0))
    }
}

/// The result every fallible binding method answers with.
pub(crate) type PyCoreResult<T> = Result<T, CoreError>;

/// Registers the hierarchy on the module.
pub(crate) fn register(module: &Bound<'_, PyModule>) -> PyResult<()> {
    module.add("MicrovmError", module.py().get_type::<MicrovmError>())?;
    module.add("UnexpectedError", module.py().get_type::<UnexpectedError>())?;
    module.add("InvalidArgError", module.py().get_type::<InvalidArgError>())?;
    module.add("RetryableError", module.py().get_type::<RetryableError>())?;
    module.add(
        "CredentialsError",
        module.py().get_type::<CredentialsError>(),
    )?;
    module.add("ProtocolError", module.py().get_type::<ProtocolError>())?;
    module.add(
        "BuildWedgedError",
        module.py().get_type::<BuildWedgedError>(),
    )?;
    module.add("LaunchDiedError", module.py().get_type::<LaunchDiedError>())?;
    module.add(
        "WindowClosedError",
        module.py().get_type::<WindowClosedError>(),
    )?;
    module.add("PlatformError", module.py().get_type::<PlatformError>())?;
    module.add("TimeoutError", module.py().get_type::<TimeoutError>())?;
    module.add(
        "InterruptedError",
        module.py().get_type::<InterruptedError>(),
    )?;
    module.add(
        "PreconditionError",
        module.py().get_type::<PreconditionError>(),
    )?;
    module.add("ExecFailedError", module.py().get_type::<ExecFailedError>())?;
    Ok(())
}
