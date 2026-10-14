//! The hot path: launch frame in, signed buy bytes out, with no allocation, lock wait, log or task
//! spawn in between. Detection stays where it is (`processors::preprocessed`, `shred_feed`); this
//! takes over from the moment a frame arrives. See DECISIONS.md (2026-10-07) for the plan.

pub mod affinity;
pub mod backup;
pub mod bids;
pub mod bundle10;
pub mod clients;
#[cfg(test)]
mod alloc_audit;
pub mod clock;
pub mod cu;
pub mod decide;
pub mod decode;
pub mod distance;
pub mod engine;
pub mod fee;
pub mod lanes;
pub mod live;
pub mod nonce_pool;
pub mod nonces;
pub mod parity;
pub mod pda;
pub mod presign;
pub mod sell;
pub mod positions;
pub mod race;
pub mod shreds;
pub mod spsc;
pub mod stamps;
pub mod template;
#[doc(hidden)]
pub mod testkit;
pub mod verify;
