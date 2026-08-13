//! Turns one accepted controller command into a slot or runtime decision.
//!
//! This module tree owns the path from submission preflight through slot
//! placement and runtime handoff. It also applies registry replies and physical
//! completion signals to the matching slot.
//!
//! ```text
//! Submit command
//!      │
//!      ▼
//! preflight and slot policy
//!      ├── rejected ──► optional watched outcome and cleanup
//!      ├── queued ──► slot queue
//!      └── selected ──► registry handoff
//!
//! registry handoff
//!      ├── registry queue full ──► capacity pump ──► registry decision
//!      └── command sent ──► registry decision
//!
//! registry decision
//!      ├── rejected ──► next queued item
//!      └── accepted ──► physical completion ──► next queued item
//! ```
//!
//! `submission` and `placement` own preflight and policy decisions.
//! `capacity` and `handoff` form the registry boundary.
//! `results` and `advance` apply authoritative results and continue queued work.
//! `watcher` and `cleanup` protect outcome and user-value ownership.
//!
//! Slot changes run in the serialized controller loop. A registry reply confirms
//! or rejects admission. Physical completion releases an accepted owner. Events
//! only report these decisions.
//!
//! Rejected user values leave slot locks before destructor cleanup starts.

mod advance;
mod capacity;
mod cleanup;
mod handoff;
mod placement;
mod results;
mod submission;
mod watcher;
