//! HTTP API for agent teams (`/api/teams/*`): CRUD over team config records plus
//! incremental member add/remove (the desktop drag-an-agent-into-a-team gesture).
//!
//! A team is a persisted, ordered collection of agent ids plus a coordination
//! strategy; this surface owns only the persistence. The `@team` chat orchestration
//! that *interprets* the strategy (`route_team_chat_stream` / `run_team_reply_text`)
//! stays in Core's session loop — it is welded to the streaming chat path, the
//! agent registry, and conversation persistence, so it consumes this crate's
//! [`TeamRecord`]/[`Coordination`] types rather than living here.
//!
//! The router is built with its own state ([`TeamsCtx`]) inside this crate so it
//! returns a state-less, mergeable `Router<()>`. The routes are declared relative
//! to `/api/teams` (Core nests this service at that prefix behind the Teams-App
//! gate), while the OpenAPI annotations keep the full external paths.
//!
//! This surface is also where the app's **hook events** are raised. It is the right
//! (and only correct) place: the sidecar owns `teams.db` outright and Core reaches
//! it over loopback HTTP (`teams_client::TeamsClient`) rather than opening the DB,
//! so every real roster change — desktop gesture, API caller, or Core's own
//! `agent_builder` — passes through these handlers exactly once.

use axum::{
    extract::{Path, State},
    http::StatusCode,
    routing::{delete, get, post},
    Json, Router,
};
use serde_json::json;

use crate::store::{CreateTeam, TeamRecord, TeamStore, UpdateTeam};

/// This app's manifest id. Core re-checks on every emit that the caller *is* the
/// plugin an event is namespaced to, so this must stay byte-identical to the `id`
/// field of `apps-store/teams/manifest.json`.
const PLUGIN_ID: &str = "@ryu/teams";

/// The events declared in that manifest's `contributes.hook_events`. Kept as
/// constants next to the id above so the `<plugin id>/<name>` rule Core enforces is
/// checkable at a glance instead of being spread over the handlers.
const EVENT_TEAM_CREATED: &str = "@ryu/teams#team.created";
const EVENT_TEAM_UPDATED: &str = "@ryu/teams#team.updated";
const EVENT_TEAM_DELETED: &str = "@ryu/teams#team.deleted";

/// Router state for the teams HTTP surface: the [`TeamStore`] (cheap to clone,
/// `Arc` inside). The same store instance is shared with Core's `@team` chat
/// orchestration and the `agent_builder` tool via `ServerState.teams`.
#[derive(Clone)]
pub struct TeamsCtx {
    pub store: TeamStore,
    /// Raises this app's declared events so hooks and workflows can react to a team
    /// changing without polling `/api/teams`. Built from the environment inside
    /// [`TeamsCtx::new`] rather than passed in, so the constructor signature — and
    /// therefore the sidecar's single call site — is unchanged; outside Core it is
    /// unhosted and every emit is a no-op.
    events: ryu_app_events::EventEmitter,
}

impl TeamsCtx {
    pub fn new(store: TeamStore) -> Self {
        Self {
            store,
            events: ryu_app_events::EventEmitter::from_env(PLUGIN_ID),
        }
    }
}

/// The payload every team event carries: the record exactly as
/// `GET /api/teams/{id}` returns it, so a subscriber binds to one shape and needs no
/// follow-up fetch — which for `team.deleted` would find nothing anyway.
///
/// Serializing a [`TeamRecord`] cannot realistically fail; the id-only fallback is
/// there so a hypothetical failure degrades the payload instead of dropping the
/// event, since a consumer that never fires is the failure mode worth avoiding.
fn team_event_payload(team: &TeamRecord) -> serde_json::Value {
    serde_json::to_value(team).unwrap_or_else(|_| json!({ "id": team.id }))
}

/// Build the `/api/teams/*` router with its own state baked in, returning a
/// state-less `Router<()>` the host nests at `/api/teams` behind the App gate.
pub fn routes(ctx: TeamsCtx) -> Router<()> {
    Router::new()
        .route("/", get(list_teams).post(create_team))
        .route("/:id", get(get_team).patch(update_team).delete(delete_team))
        .route("/:id/members", post(add_team_member))
        .route("/:id/members/:agent_id", delete(remove_team_member))
        .with_state(ctx)
}

/// The OpenAPI sub-document for the teams surface, merged into Core's spec when
/// the `teams` feature is enabled.
pub fn openapi() -> utoipa::openapi::OpenApi {
    <TeamsApiDoc as utoipa::OpenApi>::openapi()
}

#[derive(utoipa::OpenApi)]
#[openapi(paths(
    list_teams,
    create_team,
    get_team,
    update_team,
    delete_team,
    add_team_member,
    remove_team_member,
))]
struct TeamsApiDoc;

#[utoipa::path(
    get,
    path = "/api/teams",
    tag = "Teams",
    summary = "List agent teams",
    responses((status = 200, description = "OK", body = serde_json::Value))
)]
async fn list_teams(State(ctx): State<TeamsCtx>) -> (StatusCode, Json<serde_json::Value>) {
    match ctx.store.list().await {
        Ok(teams) => (StatusCode::OK, Json(json!({ "teams": teams }))),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        ),
    }
}

#[utoipa::path(
    post,
    path = "/api/teams",
    tag = "Teams",
    summary = "Create a team",
    request_body = serde_json::Value,
    responses((status = 200, description = "OK", body = serde_json::Value))
)]
async fn create_team(
    State(ctx): State<TeamsCtx>,
    Json(input): Json<CreateTeam>,
) -> (StatusCode, Json<serde_json::Value>) {
    if input.name.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "team name is required" })),
        );
    }
    match ctx.store.create(input).await {
        Ok(team) => {
            // Fired on the persist, which is the only path that mints a team id — so
            // exactly once per team, and never for a create that failed.
            ctx.events
                .emit(EVENT_TEAM_CREATED, team_event_payload(&team))
                .await;
            (StatusCode::CREATED, Json(json!({ "team": team })))
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        ),
    }
}

#[utoipa::path(
    get,
    path = "/api/teams/{id}",
    tag = "Teams",
    summary = "Get a team by id",
    params(("id" = String, Path)),
    responses((status = 200, description = "OK", body = serde_json::Value))
)]
async fn get_team(
    State(ctx): State<TeamsCtx>,
    Path(id): Path<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    match ctx.store.get(&id).await {
        Ok(Some(team)) => (StatusCode::OK, Json(json!({ "team": team }))),
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": format!("team '{id}' not found") })),
        ),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        ),
    }
}

#[utoipa::path(
    patch,
    path = "/api/teams/{id}",
    tag = "Teams",
    summary = "Update a team",
    params(("id" = String, Path)),
    request_body = serde_json::Value,
    responses((status = 200, description = "OK", body = serde_json::Value))
)]
async fn update_team(
    State(ctx): State<TeamsCtx>,
    Path(id): Path<String>,
    Json(patch): Json<UpdateTeam>,
) -> (StatusCode, Json<serde_json::Value>) {
    match ctx.store.update(&id, patch).await {
        Ok(Some(team)) => {
            // The patch is already folded into the returned record, so the event
            // carries the post-change team rather than a diff a consumer would have
            // to re-apply.
            ctx.events
                .emit(EVENT_TEAM_UPDATED, team_event_payload(&team))
                .await;
            (StatusCode::OK, Json(json!({ "team": team })))
        }
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": format!("team '{id}' not found") })),
        ),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        ),
    }
}

#[utoipa::path(
    delete,
    path = "/api/teams/{id}",
    tag = "Teams",
    summary = "Delete a team",
    params(("id" = String, Path)),
    responses((status = 200, description = "OK", body = serde_json::Value))
)]
async fn delete_team(
    State(ctx): State<TeamsCtx>,
    Path(id): Path<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    // Read the team BEFORE deleting it: once the row is gone, the name and roster a
    // subscriber needs in order to clean up after the team are unrecoverable, and
    // this event is the last place they can come from.
    let doomed = ctx.store.get(&id).await.ok().flatten();
    match ctx.store.delete(&id).await {
        Ok(true) => {
            ctx.events
                .emit(
                    EVENT_TEAM_DELETED,
                    doomed
                        .as_ref()
                        .map_or_else(|| json!({ "id": id }), team_event_payload),
                )
                .await;
            (StatusCode::OK, Json(json!({ "success": true })))
        }
        Ok(false) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": format!("team '{id}' not found") })),
        ),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        ),
    }
}

/// Body for `POST /api/teams/:id/members` — add one agent to the team. Used by
/// the desktop's drag-an-agent-into-a-team gesture.
#[derive(serde::Deserialize)]
struct AddTeamMemberRequest {
    agent_id: String,
}

#[utoipa::path(
    post,
    path = "/api/teams/{id}/members",
    tag = "Teams",
    summary = "Add a member agent to a team",
    params(("id" = String, Path)),
    request_body = serde_json::Value,
    responses((status = 200, description = "OK", body = serde_json::Value))
)]
async fn add_team_member(
    State(ctx): State<TeamsCtx>,
    Path(id): Path<String>,
    Json(body): Json<AddTeamMemberRequest>,
) -> (StatusCode, Json<serde_json::Value>) {
    // Both member endpoints are idempotent, so the roster is read BEFORE the call to
    // tell a real join from a re-add that changed nothing — waking every subscriber
    // for a no-op is how an event stops being trusted. A failed pre-read leaves
    // `before` at `None` and therefore emits: an extra fire is recoverable, a
    // swallowed membership change is not.
    let before = ctx.store.get(&id).await.ok().flatten().map(|t| t.members);
    match ctx.store.add_member(&id, &body.agent_id).await {
        Ok(Some(team)) => {
            if before.as_deref() != Some(team.members.as_slice()) {
                ctx.events
                    .emit(EVENT_TEAM_UPDATED, team_event_payload(&team))
                    .await;
            }
            (StatusCode::OK, Json(json!({ "team": team })))
        }
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": format!("team '{id}' not found") })),
        ),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        ),
    }
}

#[utoipa::path(
    delete,
    path = "/api/teams/{id}/members/{agent_id}",
    tag = "Teams",
    summary = "Remove a member from a team",
    params(("id" = String, Path), ("agent_id" = String, Path)),
    responses((status = 200, description = "OK", body = serde_json::Value))
)]
async fn remove_team_member(
    State(ctx): State<TeamsCtx>,
    Path((id, agent_id)): Path<(String, String)>,
) -> (StatusCode, Json<serde_json::Value>) {
    // Same pre-read as `add_team_member`: removing an agent that was never on the
    // team succeeds and changes nothing, which is not a roster change.
    let before = ctx.store.get(&id).await.ok().flatten().map(|t| t.members);
    match ctx.store.remove_member(&id, &agent_id).await {
        Ok(Some(team)) => {
            if before.as_deref() != Some(team.members.as_slice()) {
                ctx.events
                    .emit(EVENT_TEAM_UPDATED, team_event_payload(&team))
                    .await;
            }
            (StatusCode::OK, Json(json!({ "team": team })))
        }
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": format!("team '{id}' not found") })),
        ),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        ),
    }
}
