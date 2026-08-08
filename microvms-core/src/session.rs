//! The in-VM client — populated by T-W2-5.
//!
//! Talks to `agentd` through the endpoint proxy: the minted proxy token read from the
//! auth-token header map and both proxy headers on every request (TRAP-7), minting
//! inside the retry path below the sixty-minute ceiling (TRAP-9), and the byte-offset
//! cursor that makes an interrupted output stream resumable.
