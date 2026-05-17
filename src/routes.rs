use std::sync::Arc;

use utoipa::OpenApi;
use utoipa_swagger_ui::Config;
use warp::Filter;

use crate::auth_helpers::{with_api_auth, with_isc_auth};
use crate::auth_schemas::SecurityAddon;
use crate::error::handle_rejection;
use crate::handlers::{
    __path_handle_add_member, __path_handle_health,
    __path_handle_kubeconfig, __path_handle_remove_member,
    __path_handle_update_pipeline_token, __path_handle_update_tekton_kubeconfig,
    handle_add_member, handle_health, handle_kubeconfig, handle_remove_member,
    handle_update_pipeline_token, handle_update_tekton_kubeconfig,
};
use crate::requests::{
    AddMemberRequest, ApiResponse, KubeconfigRequest, MemberTypeDto, RemoveMemberRequest,
    UpdatePipelineTokenRequest, UpdateTektonKubeconfigRequest,
};
use crate::state::AppState;

// ── OpenAPI document ──────────────────────────────────────────────────────────

#[derive(OpenApi)]
#[openapi(
    paths(
        handle_add_member,
        handle_remove_member,
        handle_kubeconfig,
        handle_health,
        handle_update_tekton_kubeconfig,
        handle_update_pipeline_token,
    ),
    components(schemas(
        AddMemberRequest,
        RemoveMemberRequest,
        MemberTypeDto,
        KubeconfigRequest,
        ApiResponse,
        UpdateTektonKubeconfigRequest,
        UpdatePipelineTokenRequest,
    )),
    modifiers(&SecurityAddon),
    tags(
        (name = "Permissions", description = "Workspace membership — drives gitolite.conf generation"),
        (name = "Admin",       description = "Kubeconfig and token storage"),
        (name = "Internal",    description = "Liveness / readiness probes"),
    )
)]
pub struct ApiDoc;

// ── Filter helpers ────────────────────────────────────────────────────────────

fn with_state(
    state: AppState,
) -> impl Filter<Extract = (AppState,), Error = std::convert::Infallible> + Clone {
    warp::any().map(move || state.clone())
}

// ── Route assembly ────────────────────────────────────────────────────────────

pub fn build(
    state: AppState,
) -> impl Filter<Extract = impl warp::Reply, Error = std::convert::Infallible> + Clone + Send + Sync {

    // POST /workspace/:workspace/member
    // Auth: ISC — called by the provisioner service, not humans
    let add_member = warp::post()
        .and(warp::path("workspace"))
        .and(warp::path::param::<String>())
        .and(warp::path("member"))
        .and(warp::path::end())
        .and(warp::body::content_length_limit(64 * 1024))
        .and(warp::body::json())
        .and(with_isc_auth())   // ← ISCClaims
        .and(with_state(state.clone()))
        .and_then(handle_add_member);

    // DELETE /workspace/:workspace/member
    // Auth: ISC
    let remove_member = warp::delete()
        .and(warp::path("workspace"))
        .and(warp::path::param::<String>())
        .and(warp::path("member"))
        .and(warp::path::end())
        .and(warp::body::content_length_limit(64 * 1024))
        .and(warp::body::json())
        .and(with_isc_auth())   // ← ISCClaims
        .and(with_state(state.clone()))
        .and_then(handle_remove_member);

    // POST /kubeconfig
    // Auth: API — called by operators / cluster tooling with an API token
    let kubeconfig = warp::post()
        .and(warp::path("kubeconfig"))
        .and(warp::path::end())
        .and(warp::body::content_length_limit(1024 * 1024))
        .and(warp::body::json())
        .and(with_api_auth())   // ← APIClaims
        .and(with_state(state.clone()))
        .and_then(handle_kubeconfig);

    // POST /tekton-kubeconfig
    // Auth: API — operator-facing
    let tekton_kubeconfig = warp::post()
        .and(warp::path("tekton-kubeconfig"))
        .and(warp::path::end())
        .and(warp::body::content_length_limit(1024 * 1024))
        .and(warp::body::json())
        .and(with_api_auth())   // ← APIClaims
        .and(with_state(state.clone()))
        .and_then(handle_update_tekton_kubeconfig);

    // POST /pipeline-token
    // Auth: API — operator-facing
    let pipeline_token = warp::post()
        .and(warp::path("pipeline-token"))
        .and(warp::path::end())
        .and(warp::body::content_length_limit(64 * 1024))
        .and(warp::body::json())
        .and(with_api_auth())   // ← APIClaims
        .and(with_state(state.clone()))
        .and_then(handle_update_pipeline_token);

    // GET /healthz — no auth, liveness probe must be reachable by k8s
    let health = warp::get()
        .and(warp::path("healthz"))
        .and(warp::path::end())
        .and_then(handle_health);

    // GET /api-doc.json — no auth
    let api_doc = warp::path("api-doc.json")
        .and(warp::get())
        .map(|| warp::reply::json(&ApiDoc::openapi()));

    // GET /swagger-ui/... — no auth
    let swagger_config = Arc::new(Config::from("/api-doc.json"));
    let swagger_ui = warp::path("swagger-ui")
        .and(warp::get())
        .and(warp::path::full())
        .and(warp::path::tail())
        .and(warp::any().map(move || swagger_config.clone()))
        .and_then(serve_swagger);

    let log = warp::log("gitolite_sidecar::http");

    let routes = add_member
        .or(remove_member)
        .or(kubeconfig)
        .or(tekton_kubeconfig)
        .or(pipeline_token)
        .or(health)
        .or(api_doc)
        .or(swagger_ui)
        .with(log);

    routes.recover(handle_rejection)
}

// ── Swagger UI asset server ───────────────────────────────────────────────────

async fn serve_swagger(
    full_path: warp::path::FullPath,
    tail: warp::path::Tail,
    config: Arc<Config<'static>>,
) -> Result<Box<dyn warp::Reply + 'static>, warp::Rejection> {
    if full_path.as_str() == "/swagger-ui" {
        return Ok(Box::new(warp::redirect::found(
            warp::http::Uri::from_static("/swagger-ui/"),
        )));
    }
    match utoipa_swagger_ui::serve(tail.as_str(), config) {
        Ok(Some(file)) => Ok(Box::new(
            warp::http::Response::builder()
                .header("Content-Type", file.content_type)
                .body(file.bytes),
        )),
        Ok(None) => Ok(Box::new(warp::http::StatusCode::NOT_FOUND)),
        Err(error) => Ok(Box::new(
            warp::http::Response::builder()
                .status(warp::http::StatusCode::INTERNAL_SERVER_ERROR)
                .body(error.to_string()),
        )),
    }
}