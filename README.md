# ryu-teams

Agent teams for Ryu — a persisted, named, ordered collection of agents plus a coordination strategy, addressed as one unit via @team.

> **The public home of `ryu-teams`.** Source, builds, and releases live here —
> binaries for every platform are attached to each release.
>
> This tree is generated from the Ryu monorepo, so commits pushed here
> directly are replaced on the next sync. **Pull requests are welcome** —
> open them here and they are ported into the monorepo, then flow back out.
> Ryu as a whole: https://github.com/amajorai/ryu

## Install

- Binary: `ryu-teams` from the [Ryu releases](https://github.com/amajorai/ryu/releases).
- Crate: `cargo install ryu-teams`.

## License

Apache-2.0 — see [LICENSE](./LICENSE).

---

# Teams

Agent **teams**: a persisted, named, ordered collection of agents plus a coordination
strategy, addressable as one unit (`@team` in chat) so a single message fans out to every
member.

## Parts

- **`backend/` (`ryu-teams`)** — an extracted Core capability crate: the SQLite `TeamStore`
  (agent-id membership only) and the `/api/teams/*` HTTP surface. **The surface is now served
  OUT-OF-PROCESS** by the `ryu-teams` sidecar bin (below) via the manifest `public_mount` — there
  is no in-process `teams_routes` merge and no `teams` cargo feature. The crate stays a
  **non-optional path-dep** only for the `@team` chat types the session loop consumes (see the
  weld below), not for the moved surface. This crate has **zero dependency on `apps/core`**: its
  only cross-cutting need — the data-dir path — is injected by the host at `TeamStore::open`.
- **`backend/src/main.rs` (`ryu-teams` bin)** — the same crate also builds a standalone
  **out-of-process sidecar** (`[[bin]] name = "ryu-teams"`): a loopback axum server that opens
  the node's `teams.db`, nests the crate's `routes()` under `/api/teams`, and gates every route
  with the Core-injected `RYU_EXT_TOKEN` bearer (fail-closed; `/health` is the one un-gated
  probe). It reuses the crate lib, so nothing is duplicated. Core spawns it via the
  `kind: local` sidecar spec in `com.ryu.teams` (`RYU_TEAMS_BIN`/`RYU_TEAMS_PORT`, default
  `:7994`) and proxies `/api/teams/*` to it — exactly like `ryu-mail`.
- **No companion UI.** Teams surface through Core's own Library/desktop pages; there is no
  `ui/` here.
- **`@team` chat orchestration stays in Core.** The fan-out is welded to Core's streaming
  session loop (`apps/core/src/sidecar/adapters/mod.rs`), which consumes this crate's
  `TeamRecord` / `Coordination` types. That coupling is inseparable from the chat kernel, so
  — unlike quests/clips/meetings — there is **no `TeamsHost` trait**: nothing in the moved
  store + CRUD surface reaches back into Core.

## Manifest (Core fixture)

- **id** `com.ryu.teams`, no runnables, no `permission_grants`. It is a governance shell
  over the in-crate store — no documents, no external dependencies.

## Surface

`/api/teams` (list/create) · per-team `:id` · `:id/members` and `:id/members/:agent_id`
(membership edits). Types: `TeamRecord`, `Coordination`, `CreateTeam`, `UpdateTeam`.

## Swap seam

Membership is agent-id references only, so any agent card can join any team; the
coordination strategy is a stored enum, not hardcoded behavior.
