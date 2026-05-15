use std::sync::Arc;

use utoipa::OpenApi;
use utoipa_swagger_ui::Config;
use warp::Filter;

use crate::handlers::{
    __path_handle_health, __path_handle_kubeconfig, __path_handle_permissions,
    handle_health, handle_kubeconfig, handle_permissions,
};
use crate::requests::{ApiResponse, KubeconfigRequest, PermissionsRequest};
use crate::state::AppState;

// ── OpenAPI document ──────────────────────────────────────────────────────────

#[derive(OpenApi)]
#[openapi(
    paths(
        handle_permissions,
        handle_kubeconfig,
        handle_health,
    ),
    components(
        schemas(PermissionsRequest, KubeconfigRequest, ApiResponse)
    ),
    tags(
        (name = "Admin", description = "gitolite-admin repo management"),
        (name = "Internal", description = "Liveness / readiness probes"),
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
    // POST /permissions
    let permissions_route = warp::post()
        .and(warp::path("permissions"))
        .and(warp::path::end())
        .and(warp::body::content_length_limit(1024 * 1024))
        .and(warp::body::json())
        .and(with_state(state.clone()))
        .and_then(handle_permissions);

    // POST /kubeconfig
    let kubeconfig_route = warp::post()
        .and(warp::path("kubeconfig"))
        .and(warp::path::end())
        .and(warp::body::content_length_limit(1024 * 1024))
        .and(warp::body::json())
        .and(with_state(state.clone()))
        .and_then(handle_kubeconfig);

    // GET /healthz
    let health_route = warp::get()
        .and(warp::path("healthz"))
        .and(warp::path::end())
        .and_then(handle_health);

    // GET /api-doc.json
    let api_doc_route = warp::path("api-doc.json")
        .and(warp::get())
        .map(|| warp::reply::json(&ApiDoc::openapi()));

    // GET /swagger-ui/...
    let swagger_config = Arc::new(Config::from("/api-doc.json"));
    let swagger_ui_route = warp::path("swagger-ui")
        .and(warp::get())
        .and(warp::path::full())
        .and(warp::path::tail())
        .and(warp::any().map(move || swagger_config.clone()))
        .and_then(serve_swagger);

    let log = warp::log("gitolite_sidecar::http");

    permissions_route
        .or(kubeconfig_route)
        .or(health_route)
        .or(api_doc_route)
        .or(swagger_ui_route)
        .with(log)
}

// ── Swagger UI asset server (identical pattern to the reference) ──────────────

async fn serve_swagger(
    full_path: warp::path::FullPath,
    tail: warp::path::Tail,
    config: Arc<Config<'static>>,
) -> Result<Box<dyn warp::Reply + 'static>, warp::Rejection> {
    // Redirect bare /swagger-ui → /swagger-ui/
    if full_path.as_str() == "/swagger-ui" {
        return Ok(Box::new(warp::redirect::found(
            warp::http::Uri::from_static("/swagger-ui/"),
        )));
    }

    let path = tail.as_str();
    match utoipa_swagger_ui::serve(path, config) {
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