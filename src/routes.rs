use std::sync::Arc;

use utoipa::OpenApi;
use utoipa_swagger_ui::Config;
use warp::Filter;

use crate::auth_helpers::{with_api_auth, with_auth, with_isc_auth};
use crate::auth_schemas::SecurityAddon;
use crate::error::handle_rejection;
use crate::handlers::{
    __path_handle_add_member, __path_handle_health,
    __path_handle_kubeconfig, __path_handle_remove_member,
    __path_handle_update_pipeline_token, __path_handle_update_tekton_kubeconfig,
    handle_add_member, handle_health, handle_kubeconfig, handle_remove_member,
    handle_update_pipeline_token, handle_update_tekton_kubeconfig,
};
use crate::handler_create_db_taskrun::{
    __path_handle_create_db_taskrun, __path_handle_db_taskrun_logs,
    handle_create_db_taskrun, handle_db_taskrun_logs,
};
use crate::requests::{
    AddMemberRequest, ApiResponse, CreateDbTaskRunRequest, DbTaskRunLogsRequest,
    KubeconfigRequest, MemberTypeDto, RemoveMemberRequest, TaskRunCreateResponse,
    TaskRunLogsResponse, UpdatePipelineTokenRequest, UpdateTektonKubeconfigRequest,
};
use crate::state::AppState;
use crate::repo_handler::{
    __path_handle_file_content, __path_handle_org_diff, __path_handle_org_commits,
    BranchCommit, DiffStatus, FileContentRequest, FileContentResponse, FileDiff,
    OrgBranchCommitsResponse, OrgCommitsRequest, OrgDiffRequest, OrgDiffResponse,
    RepoBranchCommits, RepoDiff,
    handle_file_content, handle_org_diff, handle_org_commits,
};

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
        handle_create_db_taskrun,
        handle_db_taskrun_logs,
        handle_file_content,
        handle_org_diff,
        handle_org_commits,
    ),
    components(schemas(
        AddMemberRequest,
        RemoveMemberRequest,
        MemberTypeDto,
        KubeconfigRequest,
        ApiResponse,
        UpdateTektonKubeconfigRequest,
        UpdatePipelineTokenRequest,
        CreateDbTaskRunRequest,
        TaskRunCreateResponse,
        DbTaskRunLogsRequest,
        TaskRunLogsResponse,
        FileContentRequest,
        FileContentResponse,
        OrgDiffRequest,
        OrgDiffResponse,
        FileDiff,
        DiffStatus,
        RepoDiff,
        OrgCommitsRequest,
        OrgBranchCommitsResponse,
        RepoBranchCommits,
        BranchCommit,
    )),
    modifiers(&SecurityAddon),
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
) -> impl Filter<Extract = impl warp::Reply, Error = std::convert::Infallible> + Clone + Send + Sync
{
    let cors = warp::cors()
        .allow_any_origin()
        .allow_headers(vec!["content-type", "authorization"])
        .allow_methods(vec!["GET", "POST", "PUT", "DELETE", "OPTIONS"]);

    // POST /workspace/:workspace/member  — Auth: ISC
    let add_member = warp::post()
        .and(warp::path("workspace"))
        .and(warp::path::param::<String>())
        .and(warp::path("member"))
        .and(warp::path::end())
        .and(warp::body::content_length_limit(64 * 1024))
        .and(warp::body::json())
        .and(with_isc_auth())
        .and(with_state(state.clone()))
        .and_then(handle_add_member);

    // DELETE /workspace/:workspace/member  — Auth: ISC
    let remove_member = warp::delete()
        .and(warp::path("workspace"))
        .and(warp::path::param::<String>())
        .and(warp::path("member"))
        .and(warp::path::end())
        .and(warp::body::content_length_limit(64 * 1024))
        .and(warp::body::json())
        .and(with_isc_auth())
        .and(with_state(state.clone()))
        .and_then(handle_remove_member);

    // POST /kubeconfig  — Auth: API
    let kubeconfig = warp::post()
        .and(warp::path("kubeconfig"))
        .and(warp::path::end())
        .and(warp::body::content_length_limit(1024 * 1024))
        .and(warp::body::json())
        .and(with_api_auth())
        .and(with_state(state.clone()))
        .and_then(handle_kubeconfig);

    // POST /tekton-kubeconfig  — Auth: API
    let tekton_kubeconfig = warp::post()
        .and(warp::path("tekton-kubeconfig"))
        .and(warp::path::end())
        .and(warp::body::content_length_limit(1024 * 1024))
        .and(warp::body::json())
        .and(with_api_auth())
        .and(with_state(state.clone()))
        .and_then(handle_update_tekton_kubeconfig);

    // POST /pipeline-token  — Auth: API
    let pipeline_token = warp::post()
        .and(warp::path("pipeline-token"))
        .and(warp::path::end())
        .and(warp::body::content_length_limit(64 * 1024))
        .and(warp::body::json())
        .and(with_api_auth())
        .and(with_state(state.clone()))
        .and_then(handle_update_pipeline_token);

    // POST /taskrun/db/create
    let create_db_taskrun = warp::post()
        .and(warp::path("taskrun"))
        .and(warp::path("db"))
        .and(warp::path("create"))
        .and(warp::path::end())
        .and(warp::body::content_length_limit(64 * 1024))
        .and(warp::body::json())
        // .and(with_auth())
        .and(with_state(state.clone()))
        .and_then(handle_create_db_taskrun);

    // POST /taskrun/db/logs
    let db_taskrun_logs = warp::post()
        .and(warp::path("taskrun"))
        .and(warp::path("db"))
        .and(warp::path("logs"))
        .and(warp::path::end())
        .and(warp::body::content_length_limit(64 * 1024))
        .and(warp::body::json())
        // .and(with_auth())
        .and(with_state(state.clone()))
        .and_then(handle_db_taskrun_logs);

    // POST /repo/file
    let file_content = warp::post()
        .and(warp::path("repo"))
        .and(warp::path("file"))
        .and(warp::path::end())
        .and(warp::body::content_length_limit(64 * 1024))
        .and(warp::body::json())
        .and(with_state(state.clone()))
        .and_then(handle_file_content);

    // POST /org/diff
    let org_diff = warp::post()
        .and(warp::path("org"))
        .and(warp::path("diff"))
        .and(warp::path::end())
        .and(warp::body::content_length_limit(64 * 1024))
        .and(warp::body::json())
        .and(with_state(state.clone()))
        .and_then(handle_org_diff);

    // POST /org/commits
    let org_commits = warp::post()
        .and(warp::path("org"))
        .and(warp::path("commits"))
        .and(warp::path::end())
        .and(warp::body::content_length_limit(64 * 1024))
        .and(warp::body::json())
        .and(with_state(state.clone()))
        .and_then(handle_org_commits);

    // GET /healthz  — no auth, k8s liveness probe
    let health = warp::get()
        .and(warp::path("healthz"))
        .and(warp::path::end())
        .and_then(handle_health);

    // GET /api-doc.json  — no auth
    let api_doc = warp::path("api-doc.json")
        .and(warp::get())
        .map(|| warp::reply::json(&ApiDoc::openapi()));

    // GET /swagger-ui/...  — no auth
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
        .or(tekton_kubeconfig)
        .or(pipeline_token)
        .or(create_db_taskrun)
        .or(db_taskrun_logs)
        .or(file_content)
        .or(org_diff)
        .or(org_commits)
        .or(health)
        .or(api_doc)
        .or(swagger_ui)
        .with(log)
        .with(cors)
        .recover(handle_rejection)
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