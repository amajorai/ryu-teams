//! Agent **teams**: a persisted, ordered collection of agents that can be
//! addressed as one unit (`@team` in chat) so a single message fans out to every
//! member — an extracted Core capability crate (SQLite store + `/api/teams/*`
//! HTTP surface).
//!
//! Runs OUT-OF-PROCESS, as the `[[bin]] ryu-teams` sidecar the generic loader
//! spawns; `/api/teams/*` reaches it through the manifest `public_mount`
//! (ext-proxy). This crate is the single owner of `teams.db`, and **Core does not
//! link it**.
//!
//! ZERO dependency on `apps/core`, in both directions: the store's only
//! cross-cutting need — the data-dir path — is injected by the host at
//! [`store::TeamStore::open`], and the `@team` chat orchestration stays in Core
//! (`apps/core/src/sidecar/adapters/mod.rs`) rather than being pulled in here.
//! That orchestration is a consumer of the *payloads*, not of this crate: the
//! shapes it needs — [`TeamRecord`], [`CreateTeam`], [`Coordination`] — live in the
//! shared `ryu-teams-contracts` crate that both sides link, and it reads and writes
//! them over loopback HTTP (`apps/core/src/teams_client.rs`). So there is no
//! `TeamsHost` trait for the same reason quests/clips need none: nothing in the
//! store + CRUD surface reaches back into Core.

pub mod api;
pub mod store;

pub use api::{openapi, routes, TeamsCtx};
pub use store::{Coordination, CreateTeam, TeamRecord, TeamStore, UpdateTeam};
