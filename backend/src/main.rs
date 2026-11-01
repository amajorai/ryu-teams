//! `ryu-teams` — the standalone, out-of-process agent-teams sidecar.
//!
//! Runs the extracted `ryu_teams` capability crate (the SQLite `TeamStore` + the
//! `/api/teams/*` CRUD surface, defined in `lib.rs`) as a SEPARATE PROCESS that
//! Core spawns, health-checks, and proxies to on loopback — exactly like
//! `ryu-mail`. The store and handlers live in the crate lib; this binary is only
//! the process shell around them, so the SAME crate still compiles into Core
//! in-process as a path dependency (no code is duplicated).
//!
//! The crate's [`ryu_teams::routes`] already returns a state-baked, state-less
//! `Router<()>` whose paths are RELATIVE to `/api/teams` (Core nests it at that
//! prefix in-process). This binary nests it under the same `/api/teams` prefix, so
//! the external paths are byte-identical to Core's in-process mount and the generic
//! ext-proxy forwards `/api/teams/*` to it unchanged.
//!
//! SECURITY: loopback-only bind (127.0.0.1) + a shared-secret bearer gate
//! (`RYU_EXT_TOKEN`, injected by Core at spawn and presented on the health probe +
//! every proxied hop). EVERY `/api/teams/*` route is protected — teams has NO public
//! surface (unlike mail's per-inbox-HMAC inbound webhook). The gate is FAIL-CLOSED:
//! with no token configured every protected route rejects with 401. `/health` is the
//! ONE un-gated route (loopback probe, returns no team data), so Core's pre-auth
//! health check succeeds — mirroring the finetune sidecar's top-level `/health`.
//!
//! Port: `RYU_TEAMS_PORT` env, default `7995`. Data: the app's plugin data dir via
//! `paths::data_dir` (`RYU_PLUGIN_DATA_DIR`, injected by Core at spawn), so it opens
//! `plugin-data/@ryu+teams/teams.db`.

mod paths;

use std::net::{Ipv4Addr, SocketAddr};

use axum::{
    extract::Request,
    http::{header::AUTHORIZATION, StatusCode},
    middleware::{from_fn, Next},
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use serde_json::json;

use ryu_teams::{routes, TeamStore, TeamsCtx};

/// Default loopback port for the teams sidecar (overridable via `RYU_TEAMS_PORT`).
const DEFAULT_PORT: u16 = 7995;

fn main() -> anyhow::Result<()> {
    // Take the sealing key Core delivered out of the environment before any
    // thread starts (the async runtime spawns workers).
    ryu_teams::seal_keys::capture()?;
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(run())
}

async fn run() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let port: u16 = std::env::var("RYU_TEAMS_PORT")
        .ok()
        .and_then(|p| p.trim().parse().ok())
        .unwrap_or(DEFAULT_PORT);

    // Shared-secret bearer Core injects via the generic ext-proxy loader
    // (`RYU_EXT_TOKEN`) — the per-plugin minted secret it stamps on every proxied
    // hop + the health probe. The protected `/api/teams/*` routes require it.
    let token = std::env::var("RYU_EXT_TOKEN")
        .ok()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty());
    if token.is_some() {
        tracing::info!(
            "ryu-teams: protected /api/teams/* routes require the injected shared-secret bearer"
        );
    } else {
        tracing::warn!(
            "ryu-teams: no RYU_EXT_TOKEN set; protected /api/teams/* routes are FAIL-CLOSED (reject all). Core injects this token when it spawns the sidecar."
        );
    }

    let store = TeamStore::open(paths::data_dir().join("teams.db"))?;

    // The crate router (paths relative to `/api/teams`) nested under the external
    // prefix, with the shared-secret gate layered over the whole nest — teams has no
    // public route. `from_fn` closes over the resolved token so no extra state field
    // is needed.
    //
    // `/openapi.json` rides INSIDE that same gate, at the SERVER ROOT. Core fetches
    // `http://127.0.0.1:<port>/openapi.json` on this sidecar's first Healthy edge and
    // lowers every operation it finds into searchable LLM tools, so routing this one
    // endpoint is what makes the whole `/api/teams` surface callable by an agent
    // (`ryu_teams::api::openapi()` was dead code until now — only tests read it).
    //
    // Root, not under `/api/teams`: Core tries the root FIRST and only falls back to
    // the mount-prefixed form, and keeping the document off the mount keeps it out of
    // the manifest's declared `http.routes[]` — anything declared there is reachable
    // through the generic ext-proxy, and the schema is Core's to read, not an app
    // surface. Inside the gate, not next to the un-gated `/health`: Core stamps the
    // injected `RYU_EXT_TOKEN` on the fetch (the Python sidecars already require the
    // bearer for everything but `/health`), so the gate costs the fetcher nothing —
    // while un-gated it would disclose this app's entire internal API surface to any
    // other process on loopback.
    let teams_api = routes(TeamsCtx::new(store.clone()));

    // `/health` sits OUTSIDE the gated nest so the loopback health probe succeeds
    // before auth. It asserts the store is readable (a cheap `list`) and returns no
    // team data.
    let health_store = store;
    let health_route = Router::new().route(
        "/health",
        get(move || {
            let store = health_store.clone();
            async move { health(store).await }
        }),
    );
    let app = app_router(health_route, teams_api, token);

    // LOOPBACK ONLY (belt) + shared-secret bearer (suspenders): Core is the auth
    // front and re-stamps the bearer on the proxied hop.
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!("ryu-teams sidecar listening on http://{addr}");

    axum::serve(listener, app).await?;
    Ok(())
}

/// Loopback health probe: asserts the store is readable (a cheap `list`) so health
/// also confirms DB readiness, not just process liveness. Un-gated and data-free.
async fn health(store: TeamStore) -> Response {
    match store.list().await {
        Ok(teams) => (
            StatusCode::OK,
            Json(json!({
                "ok": true,
                "teamCount": teams.len(),
                "sealing": store.seal_health(),
            })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "ok": false, "error": e.to_string() })),
        )
            .into_response(),
    }
}

/// The served router: the un-gated `health_route` merged with `/api/teams/*` and the
/// root `/openapi.json`, both behind [`require_teams_token`]. Split out of `main` so the gate
/// wiring itself (what is protected, what is not) is under test.
fn app_router(health_route: Router, api: Router, token: Option<String>) -> Router {
    let protected = Router::new()
        .nest("/api/teams", api)
        .route(
            "/openapi.json",
            get(|| async { Json(ryu_teams::api::openapi()) }),
        )
        .layer(from_fn(move |req: Request, next: Next| {
            let expected = token.clone();
            async move { require_teams_token(req, next, expected.as_deref()).await }
        }));
    health_route.merge(protected)
}

/// Shared-secret bearer gate for the proxied `/api/teams/*` surface. Core stays the
/// auth front — it runs `require_auth`, then re-stamps `Authorization: Bearer
/// <RYU_EXT_TOKEN>` on the loopback hop — so a request that did NOT come through Core
/// (any other local process on a shared host) is rejected with 401.
///
/// **Fail-closed:** `expected == None`/empty (no token configured) rejects every
/// request rather than falling open, so a bare-run or misconfigured sidecar never
/// serves team data unauthenticated.
async fn require_teams_token(req: Request, next: Next, expected: Option<&str>) -> Response {
    let provided = req
        .headers()
        .get(AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    if ryu_sidecar_runtime::token_ok(provided, expected) {
        next.run(req).await
    } else {
        (StatusCode::UNAUTHORIZED, "unauthorized").into_response()
    }
}

#[cfg(test)]
#[path = "gate_tests.rs"]
mod gate_tests;
