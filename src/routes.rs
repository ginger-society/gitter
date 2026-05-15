use std::sync::Arc;

use utoipa::OpenApi;
use utoipa_swagger_ui::Config;
use warp::Filter;

use crate::handlers::{
    __path_handle_add_member, __path_handle_health,
    __path_handle_kubeconfig, __path_handle_remove_member,
    handle_add_member, handle_health, handle_kubeconfig, handle_remove_member,
};
use crate::requests::{
    AddMemberRequest, ApiResponse, KubeconfigRequest, MemberTypeDto, RemoveMemberRequest,
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
    ),
    components(
        schemas(
            AddMemberRequest,
            RemoveMemberRequest,
            MemberTypeDto,
            KubeconfigRequest,
            ApiResponse,
        )
    ),
    tags(
        (name = "Permissions", description = "Workspace membership management — drives gitolite.conf generation"),
        (name = "Admin",       description = "kubeconfig storage"),
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
) -> impl Filter<Extract = impl warp::Reply, Error = warp::Rejection> + Clone {

    // POST /workspace/:workspace/member
    let add_member = warp::post()
        .and(warp::path("workspace"))
        .and(warp::path::param::<String>())
        .and(warp::path("member"))
        .and(warp::path::end())
        .and(warp::body::content_length_limit(64 * 1024))
        .and(warp::body::json())
        .and(with_state(state.clone()))
        .and_then(handle_add_member);

    // DELETE /workspace/:workspace/member
    let remove_member = warp::delete()
        .and(warp::path("workspace"))
        .and(warp::path::param::<String>())
        .and(warp::path("member"))
        .and(warp::path::end())
        .and(warp::body::content_length_limit(64 * 1024))
        .and(warp::body::json())
        .and(with_state(state.clone()))
        .and_then(handle_remove_member);

    // POST /kubeconfig
    let kubeconfig = warp::post()
        .and(warp::path("kubeconfig"))
        .and(warp::path::end())
        .and(warp::body::content_length_limit(1024 * 1024))
        .and(warp::body::json())
        .and(with_state(state.clone()))
        .and_then(handle_kubeconfig);

    // GET /healthz
    let health = warp::get()
        .and(warp::path("healthz"))
        .and(warp::path::end())
        .and_then(handle_health);

    // GET /api-doc.json
    let api_doc = warp::path("api-doc.json")
        .and(warp::get())
        .map(|| warp::reply::json(&ApiDoc::openapi()));

    // GET /swagger-ui/...
    let swagger_config = Arc::new(Config::from("/api-doc.json"));
    let swagger_ui = warp::path("swagger-ui")
        .and(warp::get())
        .and(warp::path::full())
        .and(warp::path::tail())
        .and(warp::any().map(move || swagger_config.clone()))
        .and_then(serve_swagger);

    let log = warp::log("gitolite_sidecar::http");

    add_member
        .or(remove_member)
        .or(kubeconfig)
        .or(health)
        .or(api_doc)
        .or(swagger_ui)
        .with(log)
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