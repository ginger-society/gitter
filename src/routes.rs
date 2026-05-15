use warp::Filter;

use crate::handlers::{handle_health, handle_kubeconfig, handle_permissions};
use crate::state::AppState;

/// Inject AppState into a filter.
fn with_state(
    state: AppState,
) -> impl Filter<Extract = (AppState,), Error = std::convert::Infallible> + Clone {
    warp::any().map(move || state.clone())
}

pub fn build(
    state: AppState,
) -> impl Filter<Extract = impl warp::Reply, Error = warp::Rejection> + Clone {
    // POST /permissions
    let permissions = warp::post()
        .and(warp::path("permissions"))
        .and(warp::path::end())
        .and(warp::body::content_length_limit(1024 * 1024)) // 1 MB
        .and(warp::body::json())
        .and(with_state(state.clone()))
        .and_then(handle_permissions);

    // POST /kubeconfig
    let kubeconfig = warp::post()
        .and(warp::path("kubeconfig"))
        .and(warp::path::end())
        .and(warp::body::content_length_limit(1024 * 1024)) // 1 MB
        .and(warp::body::json())
        .and(with_state(state.clone()))
        .and_then(handle_kubeconfig);

    // GET /healthz
    let health = warp::get()
        .and(warp::path("healthz"))
        .and(warp::path::end())
        .and_then(handle_health);

    let log = warp::log("gitolite_sidecar::http");

    permissions.or(kubeconfig).or(health).with(log)
}