// ── Add these two handlers to your existing handlers.rs ──────────────────────
//
// New imports needed at the top of handlers.rs:
//
//   use crate::kubectl_async::{
//       ensure_creds_pvc, ensure_ginger_token_secret, ensure_namespace,
//       ensure_source_pvc, fetch_step_logs, fetch_step_status,
//       kubectl_apply, kubectl_create,
//   };
//   use crate::requests::{
//       CreateDbTaskRunRequest, DbTaskRunLogsRequest,
//       TaskRunCreateResponse, TaskRunLogsResponse,
//   };

use std::convert::Infallible;

use ginger_shared_rs::rocket_utils::Claims;
use tracing::{error, info, warn};
use warp::http::StatusCode;

use crate::kubectl_async::{
    ensure_creds_pvc, ensure_ginger_token_secret, ensure_namespace, ensure_source_pvc,
    fetch_step_logs, fetch_step_status, kubectl_apply, kubectl_create,
};
use crate::requests::{
    CreateDbTaskRunRequest, DbTaskRunLogsRequest, TaskRunCreateResponse, TaskRunLogsResponse,
};
use crate::state::AppState;

// ── POST /taskrun/db/create ───────────────────────────────────────────────────

#[utoipa::path(
    post,
    path = "/taskrun/db/create",
    tag = "Taskrun",
    security(("bearerAuth" = [])),
    request_body(content = CreateDbTaskRunRequest, content_type = "application/json"),
    responses(
        (status = 201, description = "TaskRun created", body = TaskRunCreateResponse),
        (status = 400, description = "Validation error", body = TaskRunCreateResponse),
        (status = 401, description = "Unauthorized", body = TaskRunCreateResponse),
        (status = 500, description = "Internal error", body = TaskRunCreateResponse),
    )
)]
pub async fn handle_create_db_taskrun(
    body: CreateDbTaskRunRequest,
    // _claims: Claims,
    state: AppState,
) -> Result<warp::reply::WithStatus<warp::reply::Json>, Infallible> {
    // info!(
    //     "POST /taskrun/db/create workspace_id={} commit={} caller={}",
    //     body.workspace_id, body.commit, _claims.sub
    // );

    // ── 1. Validate ───────────────────────────────────────────────────────────
    let workspace_id = body.workspace_id.trim().to_string();
    if workspace_id.is_empty() {
        return Ok(warp::reply::with_status(
            warp::reply::json(&TaskRunCreateResponse {
                status: "error",
                taskrun_name: None,
                message: Some("workspace_id must not be empty".into()),
            }),
            StatusCode::BAD_REQUEST,
        ));
    }
    if body.models_py_content.trim().is_empty() {
        return Ok(warp::reply::with_status(
            warp::reply::json(&TaskRunCreateResponse {
                status: "error",
                taskrun_name: None,
                message: Some("models_py_content must not be empty".into()),
            }),
            StatusCode::BAD_REQUEST,
        ));
    }
    if body.db_name.trim().is_empty() {
        return Ok(warp::reply::with_status(
            warp::reply::json(&TaskRunCreateResponse {
                status: "error",
                taskrun_name: None,
                message: Some("db_name must not be empty".into()),
            }),
            StatusCode::BAD_REQUEST,
        ));
    }

    // ── Derived names (all follow existing project conventions) ───────────────
    // DB repo      : {workspace_id}-database
    // namespace    : tasks-{workspace_id}-database
    // task name    : db-migrate-{workspace_id}   (applied idempotently)
    // taskrun name : db-migrate-{workspace_id}-run-  (generateName suffix)
    let db_repo = format!("{workspace_id}-database");
    let namespace = format!("tasks-{workspace_id}-database");
    let task_name = format!("db-migrate-{workspace_id}");
    let taskrun_generate_name = format!("{task_name}-run-");
    let db_name = body.db_name.trim().to_string();

    // ── 2. Read tekton kubeconfig from the checked-out admin repo ─────────────
    info!("[taskrun] reading tekton kubeconfig …");
    let tekton_kubeconfig = {
        let repo = state.0.admin_repo.lock().await;
        let kc_path = repo.repo_path.join("kubeconfig.yaml");
        drop(repo);
        match tokio::fs::read_to_string(&kc_path).await {
            Ok(kc) => kc,
            Err(e) => {
                error!("[taskrun] tekton kubeconfig not found: {e}");
                return Ok(warp::reply::with_status(
                    warp::reply::json(&TaskRunCreateResponse {
                        status: "error",
                        taskrun_name: None,
                        message: Some(format!(
                            "tekton kubeconfig not found — upload via /tekton-kubeconfig first ({e})"
                        )),
                    }),
                    StatusCode::INTERNAL_SERVER_ERROR,
                ));
            }
        }
    };

    // ── 3. Read ginger token for this workspace ───────────────────────────────
    info!("[taskrun] reading ginger token for workspace '{workspace_id}' …");
    let ginger_token = {
        let repo = state.0.admin_repo.lock().await;
        let token_path = repo.repo_path.join("pipeline-tokens").join(&workspace_id);
        drop(repo);
        match tokio::fs::read_to_string(&token_path).await {
            Ok(t) => t.trim().to_string(),
            Err(e) => {
                error!("[taskrun] ginger token not found for '{workspace_id}': {e}");
                return Ok(warp::reply::with_status(
                    warp::reply::json(&TaskRunCreateResponse {
                        status: "error",
                        taskrun_name: None,
                        message: Some(format!(
                            "GINGER_TOKEN not found for workspace '{workspace_id}' — upload via /pipeline-token first ({e})"
                        )),
                    }),
                    StatusCode::INTERNAL_SERVER_ERROR,
                ));
            }
        }
    };

    // ── 4. Ensure namespace ───────────────────────────────────────────────────
    info!("[taskrun] ensuring namespace: {namespace}");
    if let Err(e) = ensure_namespace(&tekton_kubeconfig, &namespace).await {
        error!("[taskrun] ensure_namespace failed: {e}");
        return internal_error(e);
    }

    // ── 5. Ensure PVCs — creds (SSH keys) + source (cloned repo) ─────────────
    info!("[taskrun] ensuring creds PVC …");
    if let Err(e) = ensure_creds_pvc(&tekton_kubeconfig, &namespace).await {
        error!("[taskrun] ensure_creds_pvc failed: {e}");
        return internal_error(e);
    }

    info!("[taskrun] ensuring source PVC …");
    if let Err(e) = ensure_source_pvc(&tekton_kubeconfig, &namespace).await {
        error!("[taskrun] ensure_source_pvc failed: {e}");
        return internal_error(e);
    }

    // ── 6. Ensure ginger-token-secret ─────────────────────────────────────────
    info!("[taskrun] ensuring ginger-token-secret in {namespace} …");
    if let Err(e) = ensure_ginger_token_secret(&tekton_kubeconfig, &namespace, &ginger_token).await {
        error!("[taskrun] ensure_ginger_token_secret failed: {e}");
        return internal_error(e);
    }

    // ── 7. Apply Task definition (idempotent via server-side apply) ───────────
    let task_yaml = build_db_migrate_task(&task_name, &namespace);
    info!("[taskrun] applying Task: {task_name}");
    if let Err(e) = kubectl_apply(&tekton_kubeconfig, &task_yaml).await {
        error!("[taskrun] failed to apply Task {task_name}: {e}");
        return internal_error(format!("failed to apply Task definition: {e}"));
    }
    info!("[taskrun] ✓ Task {task_name} applied");

    // ── 8. Create TaskRun ──────────────────────────────────────────────────────
    let taskrun_yaml = build_db_migrate_taskrun(
        &taskrun_generate_name,
        &namespace,
        &task_name,
        &workspace_id,
        &db_repo,
        &db_name,
        &body.models_py_content,
        body.commit_message.as_deref().unwrap_or(""),
        body.commit,
    );

    info!("[taskrun] creating TaskRun (generateName={taskrun_generate_name}) …");
    let created_output = match kubectl_create(&tekton_kubeconfig, &taskrun_yaml).await {
        Ok(out) => out,
        Err(e) => {
            error!("[taskrun] failed to create TaskRun: {e}");
            return internal_error(format!("failed to create TaskRun: {e}"));
        }
    };

    // kubectl create prints: taskrun.tekton.dev/<name> created
    let taskrun_name = created_output
        .lines()
        .find_map(|line| {
            line.split_whitespace()
                .next()
                .and_then(|token| token.strip_prefix("taskrun.tekton.dev/"))
                .map(str::to_string)
        })
        .unwrap_or_else(|| created_output.trim().to_string());

    info!("[taskrun] ✓ TaskRun created: {taskrun_name}");

    Ok(warp::reply::with_status(
        warp::reply::json(&TaskRunCreateResponse {
            status: "ok",
            taskrun_name: Some(taskrun_name),
            message: None,
        }),
        StatusCode::CREATED,
    ))
}

// ── POST /taskrun/db/logs ─────────────────────────────────────────────────────

#[utoipa::path(
    post,
    path = "/taskrun/db/logs",
    tag = "Taskrun",
    security(("bearerAuth" = [])),
    request_body(content = DbTaskRunLogsRequest, content_type = "application/json"),
    responses(
        (status = 200, description = "Logs and step status", body = TaskRunLogsResponse),
        (status = 400, description = "Validation error", body = TaskRunLogsResponse),
        (status = 401, description = "Unauthorized", body = TaskRunLogsResponse),
        (status = 500, description = "Internal error", body = TaskRunLogsResponse),
    )
)]
pub async fn handle_db_taskrun_logs(
    body: DbTaskRunLogsRequest,
    // _claims: Claims,
    state: AppState,
) -> Result<warp::reply::WithStatus<warp::reply::Json>, Infallible> {
    // info!(
    //     "POST /taskrun/db/logs taskrun={} step={} caller={}",
    //     body.taskrun_name, body.step_name, _claims.sub
    // );

    // ── Validate ──────────────────────────────────────────────────────────────
    let taskrun_name = body.taskrun_name.trim().to_string();
    let step_name = body.step_name.trim().to_string();

    if taskrun_name.is_empty() || step_name.is_empty() {
        return Ok(warp::reply::with_status(
            warp::reply::json(&TaskRunLogsResponse {
                logs: String::new(),
                status: "error: taskrun_name and step_name must not be empty".into(),
            }),
            StatusCode::BAD_REQUEST,
        ));
    }

    // Derive workspace_id and namespace from the TaskRun name.
    // Convention: db-migrate-{workspace_id}-run-{suffix}
    // Strip the known prefix and suffix to recover workspace_id.
    let namespace = derive_namespace_from_taskrun(&taskrun_name);
    info!("[taskrun/logs] derived namespace: {namespace}");

    // ── Read tekton kubeconfig ────────────────────────────────────────────────
    let tekton_kubeconfig = {
        let repo = state.0.admin_repo.lock().await;
        let kc_path = repo.repo_path.join("kubeconfig.yaml");
        drop(repo);
        match tokio::fs::read_to_string(&kc_path).await {
            Ok(kc) => kc,
            Err(e) => {
                error!("[taskrun/logs] tekton kubeconfig not found: {e}");
                return Ok(warp::reply::with_status(
                    warp::reply::json(&TaskRunLogsResponse {
                        logs: String::new(),
                        status: format!("error: tekton kubeconfig not found ({e})"),
                    }),
                    StatusCode::INTERNAL_SERVER_ERROR,
                ));
            }
        }
    };

    // ── Fetch logs and status concurrently ────────────────────────────────────
    // Logs stream until the container exits (`-f`), status is a single kubectl
    // get. Running them concurrently means we don't wait for one to finish
    // before starting the other.
    let (logs_result, status_result) = tokio::join!(
        fetch_step_logs(&tekton_kubeconfig, &namespace, &taskrun_name, &step_name),
        fetch_step_status(&tekton_kubeconfig, &namespace, &taskrun_name, &step_name),
    );

    let logs = match logs_result {
        Ok(l) => l,
        Err(e) => {
            warn!("[taskrun/logs] log fetch error: {e}");
            format!("error fetching logs: {e}")
        }
    };

    let status = match status_result {
        Ok(s) => s,
        Err(e) => {
            warn!("[taskrun/logs] status fetch error: {e}");
            format!("error fetching status: {e}")
        }
    };

    info!(
        "[taskrun/logs] ✓ returned {} log bytes, status={}",
        logs.len(),
        status
    );

    Ok(warp::reply::with_status(
        warp::reply::json(&TaskRunLogsResponse { logs, status }),
        StatusCode::OK,
    ))
}

// ── Task / TaskRun YAML builders ──────────────────────────────────────────────

/// Build the Task definition YAML.
///
/// Params passed through to the migration step:
///   models_py_content — the models JSON/py content
///   commit_message    — git commit message (empty string if not provided)
///   commit            — "true" or "false"; controls whether the step commits
///   db_repo           — derived repo name, e.g. acme-database
///
/// The Task is applied idempotently (server-side apply) so it is safe to call
/// on every request and will pick up definition changes automatically.
fn build_db_migrate_task(task_name: &str, namespace: &str) -> String {
    format!(
        r#"apiVersion: tekton.dev/v1beta1
kind: Task
metadata:
  name: {task_name}
  namespace: {namespace}
spec:
  params:
    - name: workspace_id
      type: string
    - name: db_repo
      type: string
    - name: db_name
      type: string
    - name: models_py_content_b64
      type: string
    - name: commit_message_b64
      type: string
      default: ""
    - name: commit
      type: string
      default: "false"
  workspaces:
    - name: creds
    - name: source
    - name: src
  steps:
    - name: init-credentials
      image: gingersociety/tekton-task-ginger:latest
      imagePullPolicy: Always
      env:
        - name: GINGER_TOKEN
          valueFrom:
            secretKeyRef:
              name: ginger-token-secret
              key: token
      script: |
        #!/bin/bash
        set -e
        /usr/local/bin/copy-credentials-to-workspace.sh

    - name: clone
      image: gingersociety/tekton-task-gitter:latest
      imagePullPolicy: Always
      env:
        - name: GINGER_TOKEN
          valueFrom:
            secretKeyRef:
              name: ginger-token-secret
              key: token
      script: |
        #!/bin/bash
        set -e
        /usr/local/bin/mount-git-credentials.sh
        git config --global init.defaultBranch main
        git clone git@source.gingersociety.org:$(params.db_repo).git /workspace/source/repo
        echo "Repository $(params.db_repo) cloned into /workspace/source/repo"

    - name: migrate
      image: gingersociety/db-compose-runtime:latest
      imagePullPolicy: Always
      securityContext:
        privileged: true
      env:
        - name: GINGER_TOKEN
          valueFrom:
            secretKeyRef:
              name: ginger-token-secret
              key: token
      script: |
        #!/bin/bash
        set -e

        # Layout:
        #   /workspace/source/repo            — cloned db repo root
        #   /workspace/source/repo/<DB_NAME>/ — app folder from repo (models.py, migrations/, schema.json)
        #   /app/                             — full Django project baked into the image (manage.py here)
        #   /app/src/                         — the single app folder the image's Django project uses
        DB_NAME=$(params.db_name)
        REPO_DIR=/workspace/source/repo
        APP_DIR=$REPO_DIR/$DB_NAME

        echo "=== DB Migration Step ==="
        echo "workspace_id : $(params.workspace_id)"
        echo "db_repo      : $(params.db_repo)"
        echo "db_name      : $DB_NAME"
        echo "commit       : $(params.commit)"

        # ── Decode base64-encoded free-form params ────────────────────────────
        COMMIT_MESSAGE=$(echo "$(params.commit_message_b64)" | base64 -d)
        echo "commit_msg   : $COMMIT_MESSAGE"
        MODELS_CONTENT=$(echo "$(params.models_py_content_b64)" | base64 -d)

        # ── 1. Write schema.json to workspace ────────────────────────────────
        mkdir -p $APP_DIR/migrations
        echo "Writing schema.json ..."
        printf '%s' "$MODELS_CONTENT" > /workspace/src/models.json
        echo "models.json written ($(wc -c < /workspace/src/models.json) bytes)"

        # ── 2. Prepare /app/src — clear stale generated files, copy existing migrations
        echo "Preparing /app/src ..."
        rm -rf /app/src/migrations || true
        rm -f  /app/src/admin.py   || true

        if [ -d "$APP_DIR/migrations" ]; then
          echo "Copying existing migrations from repo into /app/src/ ..."
          cp -r $APP_DIR/migrations /app/src/
        else
          echo "No existing migrations found — starting fresh."
          mkdir -p /app/src/migrations
        fi

        # ── 3. Render models.py + admin.py into /app/src ──────────────────────
        echo "Running ginger-db render-from-file ..."
        ginger-db render-from-file           --path /workspace/src/models.json           --out  /app/src/
        echo "Render complete."

        # ── 4. Run makemigrations using the image's manage.py ─────────────────
        echo "Running makemigrations ..."
        cd /app
        python manage.py makemigrations --verbosity 3
        echo "makemigrations complete."

        # ── 5. Copy generated migrations + rendered files back to workspace ────
        echo "Syncing back to workspace ..."
        rm -rf $APP_DIR/migrations || true
        cp -r /app/src/migrations $APP_DIR/
        cp    /app/src/models.py  $APP_DIR/models.py
        cp    /app/src/admin.py   $APP_DIR/admin.py  || true
        cp    /workspace/src/models.json $APP_DIR/schema.json
        echo "Sync complete."

    - name: commit-and-push
      image: gingersociety/tekton-task-gitter:latest
      imagePullPolicy: Always
      env:
        - name: GINGER_TOKEN
          valueFrom:
            secretKeyRef:
              name: ginger-token-secret
              key: token
      script: |
        #!/bin/bash
        set -e

        DB_NAME=$(params.db_name)
        REPO_DIR=/workspace/source/repo

        if [ "$(params.commit)" = "true" ]; then
          echo "Committing and pushing ..."
          /usr/local/bin/mount-git-credentials.sh
          cd $REPO_DIR
          git config user.name  "GingerBot"
          git config user.email "bot@gingersociety.org"
          COMMIT_MESSAGE=$(echo "$(params.commit_message_b64)" | base64 -d)
          git add $DB_NAME/migrations/                   $DB_NAME/models.py                   $DB_NAME/admin.py                   $DB_NAME/schema.json
          git commit -m "$COMMIT_MESSAGE" || echo "No changes to commit."
          git push origin main
          echo "Changes pushed successfully."
        else
          echo "commit=false — skipping commit and push."
        fi

        echo "=== Migration step finished ==="
"#,
        task_name = task_name,
        namespace = namespace,
    )
}

/// Build the TaskRun YAML.
///
/// All migration params are injected here so the Task definition stays generic
/// and reusable. PVCs use `volumeClaimTemplate` so each run gets its own
/// ephemeral volumes that Tekton cleans up after completion.
#[allow(clippy::too_many_arguments)]
fn build_db_migrate_taskrun(
    generate_name: &str,
    namespace: &str,
    task_name: &str,
    workspace_id: &str,
    db_repo: &str,
    db_name: &str,
    models_py_content: &str,
    commit_message: &str,
    commit: bool,
) -> String {
    use base64::{engine::general_purpose::STANDARD, Engine};

    // Base64-encode all free-form string content before embedding in YAML.
    // This makes each param value a plain alphanumeric scalar with no quoting,
    // no block scalar indentation rules, and no risk of YAML parse errors
    // regardless of what JSON/special characters the content contains.
    // The migrate step decodes with `base64 -d` before using the values.
    let models_b64 = STANDARD.encode(models_py_content.as_bytes());
    let commit_msg_b64 = STANDARD.encode(commit_message.as_bytes());

    format!(
        r#"apiVersion: tekton.dev/v1beta1
kind: TaskRun
metadata:
  generateName: {generate_name}
  namespace: {namespace}
  labels:
    ginger-gitter/workspace: "{workspace_id}"
    ginger-gitter/db-repo: "{db_repo}"
spec:
  taskRef:
    name: {task_name}
  params:
    - name: workspace_id
      value: "{workspace_id}"
    - name: db_repo
      value: "{db_repo}"
    - name: db_name
      value: "{db_name}"
    - name: commit_message_b64
      value: "{commit_msg_b64}"
    - name: commit
      value: "{commit}"
    - name: models_py_content_b64
      value: "{models_b64}"
  workspaces:
    - name: creds
      volumeClaimTemplate:
        spec:
          accessModes: [ReadWriteOnce]
          resources:
            requests:
              storage: 50Mi
    - name: source
      volumeClaimTemplate:
        spec:
          accessModes: [ReadWriteOnce]
          resources:
            requests:
              storage: 1Gi
    - name: src
      emptyDir: {{}}
"#,
        generate_name = generate_name,
        namespace = namespace,
        task_name = task_name,
        workspace_id = workspace_id,
        db_repo = db_repo,
        db_name = db_name,
        commit_msg_b64 = commit_msg_b64,
        commit = commit,
        models_b64 = models_b64,
    )
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Derive the namespace from a TaskRun name.
///
/// Convention: `db-migrate-{workspace_id}-run-{suffix}`
/// Namespace  : `tasks-{workspace_id}-database`
///
/// We strip the leading `db-migrate-` and the trailing `-run-{suffix}` to
/// recover the workspace_id.
fn derive_namespace_from_taskrun(taskrun_name: &str) -> String {
    let without_prefix = taskrun_name
        .strip_prefix("db-migrate-")
        .unwrap_or(taskrun_name);

    // Everything up to the last `-run-` segment is the workspace_id.
    let workspace_id = if let Some(idx) = without_prefix.rfind("-run-") {
        &without_prefix[..idx]
    } else {
        without_prefix
    };

    format!("tasks-{workspace_id}-database")
}

fn internal_error(
    msg: impl Into<String>,
) -> Result<warp::reply::WithStatus<warp::reply::Json>, Infallible> {
    Ok(warp::reply::with_status(
        warp::reply::json(&TaskRunCreateResponse {
            status: "error",
            taskrun_name: None,
            message: Some(msg.into()),
        }),
        StatusCode::INTERNAL_SERVER_ERROR,
    ))
}

// ── routes.rs additions ───────────────────────────────────────────────────────
//
// Add inside routes::build(), alongside the existing routes:
//
//   use crate::auth_helpers::with_auth;
//   use crate::handlers::{
//       __path_handle_create_db_taskrun, __path_handle_db_taskrun_logs,
//       handle_create_db_taskrun, handle_db_taskrun_logs,
//   };
//
//   // POST /taskrun/db/create
//   let create_db_taskrun = warp::post()
//       .and(warp::path("taskrun"))
//       .and(warp::path("db"))
//       .and(warp::path("create"))
//       .and(warp::path::end())
//       .and(warp::body::content_length_limit(1024 * 1024)) // 1 MB — models content can be large
//       .and(warp::body::json())
//       .and(with_auth())           // ← Claims (Authorization: Bearer <jwt>)
//       .and(with_state(state.clone()))
//       .and_then(handle_create_db_taskrun);
//
//   // POST /taskrun/db/logs
//   let db_taskrun_logs = warp::post()
//       .and(warp::path("taskrun"))
//       .and(warp::path("db"))
//       .and(warp::path("logs"))
//       .and(warp::path::end())
//       .and(warp::body::content_length_limit(64 * 1024))
//       .and(warp::body::json())
//       .and(with_auth())           // ← Claims (Authorization: Bearer <jwt>)
//       .and(with_state(state.clone()))
//       .and_then(handle_db_taskrun_logs);
//
// Then add both to the `.or()` chain.
//
// OpenAPI doc in routes.rs — add to paths():
//   handle_create_db_taskrun, handle_db_taskrun_logs
//
// Add to components(schemas(...)):
//   CreateDbTaskRunRequest, DbTaskRunLogsRequest,
//   TaskRunCreateResponse, TaskRunLogsResponse
//
// Add tag:
//   (name = "Taskrun", description = "On-demand DB migration TaskRun triggers")
//
// main.rs — add module:
//   mod kubectl_async;