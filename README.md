<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="./icon-dark.png" />
    <img src="./icon-light.png" alt="Teams" width="144" />
  </picture>
</p>

<div align="center">

# Teams

</div>

A persisted, named, ordered collection of agents plus a coordination strategy, addressed as one unit via @team.

> **The public home of `ryu-teams`.** Source, builds, and releases live here —
> binaries for every platform are attached to each release.
>
> This tree is generated from the Ryu monorepo, so commits pushed here
> directly are replaced on the next sync. **Pull requests are welcome** —
> open them here and they are ported into the monorepo, then flow back out.
> Ryu as a whole: https://github.com/amajorai/ryu

## Install

**App:** [Install](ryu://apps/@ryu/teams) (opens the Ryu desktop app and asks you to confirm)

**CLI:**

```bash
ryu apps add @ryu/teams
```

**Crate:**

```bash
cargo install ryu-teams
```

Prebuilt binaries for every platform are attached to [each release](https://github.com/amajorai/ryu/releases).

## License

Apache-2.0 — see [LICENSE](./LICENSE).

## Parts

- **`backend/` (`ryu-teams`)** — an extracted Core capability crate: the SQLite `TeamStore`
  (agent-id membership only) and the `/api/teams/*` HTTP surface. **The surface is now served
  OUT-OF-PROCESS** by the `ryu-teams` sidecar bin (below) via the manifest `public_mount` — there
  is no in-process `teams_routes` merge and no `teams` cargo feature. The crate stays a
  **non-optional path-dep** only for the `@team` chat types the session loop consumes (see the
  weld below), not for the moved surface. This crate has **zero dependency on `apps/core`**: its
  only cross-cutting need, the data-dir path, is injected by the host at `TeamStore::open`.
- **`backend/src/main.rs` (`ryu-teams` bin)** — the same crate also builds a standalone
  **out-of-process sidecar** (`[[bin]] name = "ryu-teams"`): a loopback axum server that opens
  the node's `teams.db`, nests the crate's `routes()` under `/api/teams`, and gates every route
  with the Core-injected `RYU_EXT_TOKEN` bearer (fail-closed; `/health` is the one un-gated
  probe). It reuses the crate lib, so nothing is duplicated. Core spawns it via the
  `kind: local` sidecar spec in `@ryu/teams` (`RYU_TEAMS_BIN`/`RYU_TEAMS_PORT`, default
  `:8002`) and proxies `/api/teams/*` to it — exactly like `ryu-mail`.
- **`ui/`** — the self-contained Teams Companion. It lists and edits teams, manages members
  from the host's read-only agent catalog, selects coordination and lead-agent settings, and
  handles its own loading, recovery and confirmation states. Calls go through the
  capability-gated `window.ryu` bridge: `app.request` can reach only this app's sidecar,
  `registry.agents` exposes the agent catalog without credentials, and `ui.toast` reports
  completed mutations through the host notification system.
- **`@team` chat orchestration stays in Core.** The session loop consumes only the shared
  `ryu-teams-contracts` records. The app crate and its HTTP implementation remain out of
  process; no Teams app code is linked into Core.

## Manifest (Core fixture)

- **id** `@ryu/teams`, with one HTML Companion runnable. The `app:http`,
  `core:list_agents`, and `ui:toast` grants cover its own-sidecar CRUD, member picker, and
  host toast feedback. It has no document or external-service dependency.
- **sidebar** The app contributes its live team list. Rows open the Companion, where team
  selection and creation stay app-owned instead of creating partial records in the shell.
- **contributes** three `hook_events` (`team.created`, `team.updated`, `team.deleted`),
  raised from `api.rs` via `ryu-app-events` so hooks and workflows can react to a roster
  change without polling. The HTTP surface is the right place to raise them because the
  sidecar owns `teams.db` outright: Core reaches it over loopback (`teams_client.rs`), so
  every mutation, including its own `agent_builder`'s, passes through those handlers once.

## Surface

`/api/teams` (list/create) · per-team `:id` · `:id/members` and `:id/members/:agent_id`
(membership edits). Types: `TeamRecord`, `Coordination`, `CreateTeam`, `UpdateTeam`.

## Swap seam

Membership is agent-id references only, so any agent card can join any team; the
coordination strategy is a stored enum, not hardcoded behavior.
