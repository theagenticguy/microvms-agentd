//! The `Sandbox` lifecycle facade — populated by T-W3-6.
//!
//! Composes `control` and `session` into the surface the CLI and the bindings use:
//! build, run, suspend, resume, terminate, the launch-time suspended window
//! (STATE-12), and teardown in the order that does not leak — log groups last, since
//! the service recreates a group deleted before its image.
