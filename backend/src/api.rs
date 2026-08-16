//! HTTP API for agent teams (`/api/teams/*`): CRUD over team config records plus
//! incremental member add/remove (the desktop drag-an-agent-into-a-team gesture).
//!
//! A team is a persisted, ordered collection of agent ids plus a coordination
//! strategy; this surface owns only the persistence. The `@team` chat orchestration
//! that *interprets* the strategy (`route_team_chat_stream` / `run_team_reply_text`)
//! stays in Core's session loop — it is welded to the streaming chat path, the
//! agent registry, and conversation persistence, so rather than living here it
//! consumes the `TeamRecord` / `Coordination` shapes this surface serves. Those
//! shapes are not this crate's: they live in the shared `ryu-teams-contracts`
//! crate (re-exported through [`crate::store`]), which is precisely what lets Core
//! read them without linking any of this.
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

use crate::store::{CoordinationSchema, CreateTeam, TeamRecord, TeamStore, UpdateTeam};

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
#[openapi(
    paths(
        list_teams,
        create_team,
        get_team,
        update_team,
        delete_team,
        add_team_member,
        remove_team_member,
    ),
    components(schemas(CreateTeamBody, UpdateTeam, AddTeamMemberRequest, CoordinationSchema,))
)]
struct TeamsApiDoc;

/// The wire shape of `POST /api/teams`, mirrored from [`CreateTeam`] purely so the
/// OpenAPI document can describe it.
///
/// The handler still deserializes the real [`CreateTeam`]; this type is never
/// constructed. It exists because [`CreateTeam`] lives in `ryu-teams-contracts`,
/// which is `serde`-only on purpose — Core links it and must not inherit `utoipa`
/// through it — so the contract itself cannot carry a `ToSchema` derive. Without a
/// mirror the create tool Core derives from this document would reach the model with
/// no arguments at all.
// `create_body_schema_mirrors_the_real_wire_type` pins the two together by comparing
// this schema's properties against `CreateTeam`'s serialized keys, so a field added
// to the contract fails the tests here rather than going undocumented.
#[derive(Debug, serde::Deserialize, utoipa::ToSchema)]
pub struct CreateTeamBody {
    /// Display name for the team. Required and non-blank; this is what `@team`
    /// addressing shows.
    pub name: String,
    /// What the team is for. Free text, shown in the UI.
    #[serde(default)]
    pub description: Option<String>,
    /// Member agent ids, in order. Order is meaningful for `round-robin` (turn
    /// order) and is the default lead for `debate-synthesis`.
    #[serde(default)]
    pub members: Vec<String>,
    /// How members respond when the team is called. Defaults to `broadcast`.
    // Inlined so the four values are literal in the argument schema rather than a
    // `$ref` Core's importer would not follow this deep.
    #[serde(default)]
    #[schema(inline)]
    pub coordination: CoordinationSchema,
    /// The synthesizer for `debate-synthesis` (and the classifier for `router`).
    /// Falls back to the first member when unset.
    #[serde(default)]
    pub lead_agent_id: Option<String>,
}

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
    // `CreateTeamBody` documents the shape; the handler deserializes the real
    // `CreateTeam` from `ryu-teams-contracts`, which cannot carry a `ToSchema` derive.
    request_body = CreateTeamBody,
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
    request_body = UpdateTeam,
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
#[derive(serde::Deserialize, utoipa::ToSchema)]
struct AddTeamMemberRequest {
    /// Id of the agent to append to the team's ordered member list. Adding an agent
    /// that is already a member succeeds and changes nothing.
    agent_id: String,
}

#[utoipa::path(
    post,
    path = "/api/teams/{id}/members",
    tag = "Teams",
    summary = "Add a member agent to a team",
    params(("id" = String, Path)),
    request_body = AddTeamMemberRequest,
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

#[cfg(test)]
mod tests {

    // ── OpenAPI document ───────────────────────────────────────────────────────

    /// This app's own manifest, read at compile time. The route contract lives there,
    /// so the invariants below compare the document against the real declaration
    /// rather than against a second list that could drift from it.
    fn openapi_manifest() -> serde_json::Value {
        serde_json::from_str(include_str!("../../manifest.json")).expect("valid JSON")
    }

    /// The manifest sidecar whose HTTP surface this router serves: the one that
    /// declares an `http.mount`. Selected BY mount rather than by index because an app
    /// may declare a second, mountless sidecar (finetune already does), and
    /// `sidecars[0]` would then quietly start asserting against the wrong process.
    fn mounted_sidecar() -> serde_json::Value {
        openapi_manifest()["sidecars"]
            .as_array()
            .expect("sidecars must be an array")
            .iter()
            .find(|s| s["http"]["mount"].is_string())
            .expect("one sidecar must declare an http.mount")
            .clone()
    }

    /// A manifest route (relative to the mount, in axum's `:param` form) rewritten
    /// into the form the OpenAPI document uses (absolute, in `{param}` form).
    ///
    /// The two forms differ ON PURPOSE — the router registers paths relative to the
    /// mount because Core nests it there, while the `#[utoipa::path]` annotations carry
    /// the absolute EXTERNAL path a caller actually hits. Normalise here; do not
    /// "align" either side.
    fn doc_path_for(mount: &str, route: &str) -> String {
        let joined = if route == "/" {
            mount.to_owned()
        } else {
            format!("{mount}{route}")
        };
        joined
            .split('/')
            .map(|seg| match seg.strip_prefix(':') {
                Some(name) => format!("{{{name}}}"),
                None => seg.to_owned(),
            })
            .collect::<Vec<_>>()
            .join("/")
    }

    #[test]
    fn openapi_doc_is_served_and_non_empty() {
        // The doc is no longer dead code: Core fetches it to derive tools.
        assert!(!super::openapi().paths.paths.is_empty());
    }

    #[test]
    fn every_declared_route_appears_in_the_openapi_doc() {
        // The direction that decides tool yield. Core's `ext_api::lower` keeps only the
        // document operations the manifest ALSO declares, so a declared route with no
        // `#[utoipa::path]` annotation is a tool that silently never exists — nothing
        // errors, the agent simply cannot call it. (The other direction is harmless: an
        // annotated path the manifest does not declare is dropped by the same filter.)
        let sidecar = mounted_sidecar();
        let mount = sidecar["http"]["mount"].as_str().expect("an http.mount");
        let doc = super::openapi();
        for route in sidecar["http"]["routes"]
            .as_array()
            .expect("routes must be an array")
        {
            let path = route["path"].as_str().expect("a route path");
            let expected = doc_path_for(mount, path);
            assert!(
                doc.paths.paths.contains_key(&expected),
                "'{path}' is declared in manifest.json but the OpenAPI document has no \
                 '{expected}' operation — Core derives no tool for it"
            );
        }
    }

    // ── Request-body schemas ───────────────────────────────────────────────────
    //
    // Core derives a write tool's ARGUMENTS from the operation's `requestBody`
    // schema. `request_body = serde_json::Value` documents an untyped body, so the
    // tool reaches the model with nothing it can fill in — discoverable and
    // uncallable. These tests pin the retrofit that replaced it.

    use super::{CoordinationSchema, CreateTeam};
    use serde_json::Value;

    fn doc_json() -> Value {
        serde_json::to_value(super::openapi()).expect("the document serializes")
    }

    /// The JSON-schema node for one operation's request body, or `None` when the
    /// operation declares no body at all.
    fn request_body_schema<'a>(doc: &'a Value, path: &str, method: &str) -> Option<&'a Value> {
        let escaped = path.replace('/', "~1");
        doc.pointer(&format!(
            "/paths/{escaped}/{method}/requestBody/content/application~1json/schema"
        ))
    }

    #[test]
    fn post_routes_document_their_request_body() {
        let doc = doc_json();
        let schema = request_body_schema(&doc, "/api/teams", "post")
            .expect("POST /api/teams declares a request body");
        assert!(
            schema.get("$ref").is_some() || schema.get("properties").is_some(),
            "a derived write tool would have no arguments: {schema}"
        );
    }

    #[test]
    fn every_request_body_ref_resolves_against_components() {
        // The assertion above is necessary but not sufficient: a `$ref` to a type that
        // was never registered under `components.schemas` looks identical in the
        // operation and still yields ZERO arguments once Core resolves it. So walk
        // every operation and resolve for real. This is also what catches
        // `request_body = Option<T>`, which renders an unresolvable `oneOf` wrapper.
        let doc = doc_json();
        for (path, methods) in doc["paths"].as_object().expect("paths is an object") {
            for (method, op) in methods.as_object().expect("an operation map") {
                let Some(schema) = op.pointer("/requestBody/content/application~1json/schema")
                else {
                    continue;
                };
                let Some(reference) = schema.get("$ref").and_then(Value::as_str) else {
                    assert!(
                        schema.get("properties").is_some(),
                        "{method} {path} has a request body that is neither a $ref nor an \
                         object with properties — the derived tool gets no arguments: {schema}"
                    );
                    continue;
                };
                let name = reference
                    .strip_prefix("#/components/schemas/")
                    .unwrap_or_else(|| panic!("{method} {path}: unexpected $ref '{reference}'"));
                let target = doc
                    .pointer(&format!("/components/schemas/{name}"))
                    .unwrap_or_else(|| {
                        panic!(
                            "{method} {path} refs '{name}' but it is missing from \
                             components.schemas — add it to components(schemas(..))"
                        )
                    });
                assert!(
                    target.get("properties").is_some(),
                    "{method} {path} resolves to '{name}', which exposes no properties: {target}"
                );
                // And nothing INSIDE it may be a pointer either. Core resolves a `$ref`
                // one level into a schema, so a ref under `properties.x.items` or inside
                // a `oneOf` reaches the model as an opaque pointer — the same
                // zero-arguments failure, just one level down where the top-level checks
                // above cannot see it. Every nested type here is `#[schema(inline)]`d
                // precisely so this holds.
                assert!(
                    !target.to_string().contains("$ref"),
                    "{method} {path} → '{name}' carries a nested $ref Core cannot follow: {}",
                    serde_json::to_string_pretty(target).unwrap()
                );
            }
        }
    }

    #[test]
    fn a_nested_enum_argument_is_self_describing() {
        // `coordination` is the argument that decides how the team behaves. If it
        // reached the model as an opaque `$ref` the agent could create a team but never
        // choose a strategy, so assert the four values are literally present on BOTH
        // write bodies.
        let doc = doc_json();
        for schema_name in ["CreateTeamBody", "UpdateTeam"] {
            let node = doc
                .pointer(&format!(
                    "/components/schemas/{schema_name}/properties/coordination"
                ))
                .unwrap_or_else(|| panic!("{schema_name} documents a `coordination` property"));
            let rendered = node.to_string();
            for value in ["broadcast", "round-robin", "debate-synthesis", "router"] {
                assert!(
                    rendered.contains(value),
                    "{schema_name}.coordination omits '{value}': {node:#}"
                );
            }
            assert!(
                !rendered.contains("$ref"),
                "{schema_name}.coordination is a pointer Core cannot follow this deep: {node:#}"
            );
        }
    }

    #[test]
    fn create_body_schema_mirrors_the_real_wire_type() {
        // `CreateTeamBody` documents what the handler actually deserializes into
        // `CreateTeam`. `CreateTeam` carries no `skip_serializing_if`, so its serialized
        // keys ARE its full field set — comparing against them catches a field added to
        // the contract that would otherwise go undocumented (and therefore unusable by
        // the agent).
        let contract = serde_json::to_value(CreateTeam::default()).unwrap();
        let mut expected: Vec<&str> = contract
            .as_object()
            .expect("CreateTeam serializes to an object")
            .keys()
            .map(String::as_str)
            .collect();
        expected.sort_unstable();

        let doc = doc_json();
        let properties = doc
            .pointer("/components/schemas/CreateTeamBody/properties")
            .and_then(Value::as_object)
            .expect("CreateTeamBody documents its properties");
        let mut documented: Vec<&str> = properties.keys().map(String::as_str).collect();
        documented.sort_unstable();

        assert_eq!(
            documented, expected,
            "CreateTeamBody has drifted from the CreateTeam contract it mirrors"
        );
    }

    #[test]
    fn coordination_schema_mirrors_the_real_wire_type() {
        // Every value the document offers must be one the write path actually accepts.
        // `Coordination` deserializes strictly on the write shape, so a mirror variant
        // the contract dropped would be a value the model happily sends and the API
        // rejects with a 400.
        for mirror in [
            CoordinationSchema::Broadcast,
            CoordinationSchema::RoundRobin,
            CoordinationSchema::DebateSynthesis,
            CoordinationSchema::Router,
        ] {
            let wire = serde_json::to_value(mirror).unwrap();
            let text = wire.as_str().expect("a kebab-case string");
            assert_eq!(
                crate::store::Coordination::from_str_lenient(text).as_str(),
                text,
                "'{text}' is documented but the real Coordination does not know it"
            );
            serde_json::from_value::<CreateTeam>(serde_json::json!({
                "name": "T",
                "coordination": text,
            }))
            .unwrap_or_else(|e| panic!("the write shape rejects documented value '{text}': {e}"));
        }
    }

    #[test]
    fn body_field_docs_reach_the_schema_as_argument_descriptions() {
        // Field doc comments are lifted verbatim into `description`, which is the text
        // the model actually reads when deciding how to fill an argument.
        let doc = doc_json();
        let lead = doc
            .pointer("/components/schemas/UpdateTeam/properties/lead_agent_id/description")
            .and_then(Value::as_str)
            .expect("the `lead_agent_id` argument is described");
        assert!(
            lead.contains("null"),
            "the description must explain how to CLEAR the lead: {lead}"
        );
    }

    #[test]
    fn body_less_routes_declare_no_request_body() {
        // Delete and member-removal take only path parameters. Documenting a body for
        // them would invent an argument the handler ignores.
        let doc = doc_json();
        for (path, method) in [
            ("/api/teams/{id}", "delete"),
            ("/api/teams/{id}/members/{agent_id}", "delete"),
        ] {
            assert!(
                request_body_schema(&doc, path, method).is_none(),
                "{method} {path} must document no request body"
            );
            let escaped = path.replace('/', "~1");
            assert!(
                doc.pointer(&format!("/paths/{escaped}/{method}/parameters"))
                    .is_some(),
                "{method} {path} still documents its path parameters"
            );
        }
    }
}
