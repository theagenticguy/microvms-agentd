//! The control-plane client — populated by T-W2-4.
//!
//! SigV4-signed rest-json against `lambda.<region>.amazonaws.com`: image build and
//! wait with the stall probe (TRAP-1, TRAP-2), connector ARN derivation (TRAP-4), the
//! run-hook payload ceiling (TRAP-5), the launch wait's terminal-state branch
//! (TRAP-8), and the shell-auth operation this client never calls (TRAP-11).
