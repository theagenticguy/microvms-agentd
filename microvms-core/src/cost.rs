//! The cost engine — populated by T-W2-3.
//!
//! Ports `clients/python/src/microvms_agentd/cost.py` and the region comparison in
//! `pricing.py`: provenance-labelled durations (COST-1), estimate-typed dollars with
//! no `Into<f64>` (COST-2), a distinct `Unpriced` variant carrying its reason
//! (COST-3), decimal money arithmetic (COST-6), and the ARM-only rate rule (COST-9).
