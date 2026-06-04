/// Async kubectl helpers used by the sidecar HTTP handlers.
///
/// The pipeline-hook binary uses blocking `std::process::Command` because it
/// runs as a git hook (no async runtime). The sidecar runs inside Tokio, so
/// we use `tokio::process::Command` here to avoid blocking the executor.
///
/// All helpers that need a kubeconfig write it to a `NamedTempFile`
/// (auto-deleted on drop) and pass it via `--kubeconfig`.
use std::io::Write as _;

use tempfile::NamedTempFile;
use tokio::io::AsyncWriteExt as _;
use tracing::{info, warn};

// ── Binary location ───────────────────────────────────────────────────────────

pub fn find_kubectl() -> std::path::PathBuf {
    const CANDIDATES: &[&str] = &[
        "/usr/local/bin/kubectl",
        "/usr/bin/kubectl",
        "/bin/kubectl",
        "/snap/bin/kubectl",
        "/opt/homebrew/bin/kubectl",
    ];
    for path in CANDIDATES {
        if std::path::Path::new(path).exists() {
            return std::path::PathBuf::from(path);
        }
    }
    std::path::PathBuf::from("kubectl")
}

// ── Kubeconfig temp-file ──────────────────────────────────────────────────────

pub fn write_kubeconfig(content: &str) -> Result<NamedTempFile, String> {
    let mut f = NamedTempFile::new()
        .map_err(|e| format!("failed to create temp kubeconfig: {e}"))?;
    f.write_all(content.as_bytes())
        .map_err(|e| format!("failed to write temp kubeconfig: {e}"))?;
    Ok(f)
}

// ── Public surface ────────────────────────────────────────────────────────────

/// `kubectl apply --server-side --force-conflicts -f -`
pub async fn kubectl_apply(kubeconfig_yaml: &str, resource_yaml: &str) -> Result<String, String> {
    let kc_file = write_kubeconfig(kubeconfig_yaml)?;
    run_kubectl(
        &[
            "--kubeconfig",
            kc_file.path().to_str().unwrap_or("/tmp/kc.yaml"),
            "apply",
            "--server-side",
            "--force-conflicts",
            "-f",
            "-",
        ],
        resource_yaml,
    )
    .await
}

/// `kubectl create -f -` — required for resources that use `generateName`.
pub async fn kubectl_create(kubeconfig_yaml: &str, resource_yaml: &str) -> Result<String, String> {
    let kc_file = write_kubeconfig(kubeconfig_yaml)?;
    run_kubectl(
        &[
            "--kubeconfig",
            kc_file.path().to_str().unwrap_or("/tmp/kc.yaml"),
            "create",
            "-f",
            "-",
        ],
        resource_yaml,
    )
    .await
}

/// Ensure a Kubernetes namespace exists (idempotent).
pub async fn ensure_namespace(kubeconfig_yaml: &str, namespace: &str) -> Result<(), String> {
    let ns_yaml = format!(
        "apiVersion: v1\nkind: Namespace\nmetadata:\n  name: {namespace}\n"
    );
    let out = kubectl_apply(kubeconfig_yaml, &ns_yaml).await?;
    info!("[kubectl] namespace {namespace}: {}", out.trim());
    Ok(())
}

/// Ensure the `ginger-token-secret` Secret exists in the namespace.
pub async fn ensure_ginger_token_secret(
    kubeconfig_yaml: &str,
    namespace: &str,
    token: &str,
) -> Result<(), String> {
    use base64::{engine::general_purpose::STANDARD, Engine};
    let token_b64 = STANDARD.encode(token.trim().as_bytes());

    let secret_yaml = format!(
        r#"apiVersion: v1
kind: Secret
metadata:
  name: ginger-token-secret
  namespace: {namespace}
type: Opaque
data:
  token: {token_b64}
"#
    );

    let out = kubectl_apply(kubeconfig_yaml, &secret_yaml).await?;
    info!("[kubectl] ginger-token-secret in {namespace}: {}", out.trim());
    Ok(())
}

/// Ensure the `creds` PVC (SSH credentials workspace) exists and is Bound.
pub async fn ensure_creds_pvc(kubeconfig_yaml: &str, namespace: &str) -> Result<(), String> {
    ensure_single_pvc(
        kubeconfig_yaml,
        namespace,
        "creds-pvc",
        r#"spec:
  accessModes:
    - ReadWriteOnce
  resources:
    requests:
      storage: 50Mi"#,
    )
    .await
}

/// Ensure the `source` PVC (cloned repo workspace) exists and is Bound.
pub async fn ensure_source_pvc(kubeconfig_yaml: &str, namespace: &str) -> Result<(), String> {
    ensure_single_pvc(
        kubeconfig_yaml,
        namespace,
        "source-pvc",
        r#"spec:
  accessModes:
    - ReadWriteOnce
  resources:
    requests:
      storage: 1Gi"#,
    )
    .await
}

/// Fetch logs for a specific step container in a TaskRun pod and return them
/// as a collected string. Uses `-f` to stream until the container exits.
///
/// `step_name` should be the bare step name (e.g. `"clone"`); the `step-`
/// prefix Tekton adds to container names is handled internally.
pub async fn fetch_step_logs(
    kubeconfig_yaml: &str,
    namespace: &str,
    taskrun_name: &str,
    step_name: &str,
) -> Result<String, String> {
    let kc_file = write_kubeconfig(kubeconfig_yaml)?;
    let kc_path = kc_file.path().to_str().unwrap_or("/tmp/kc.yaml").to_string();
    let kubectl = find_kubectl();

    // Tekton names the pod  <taskrun-name>-pod
    let pod_name = format!("{taskrun_name}-pod");
    // Tekton prefixes every container with  step-
    let container_name = if step_name.starts_with("step-") {
        step_name.to_string()
    } else {
        format!("step-{step_name}")
    };

    let output = tokio::process::Command::new(&kubectl)
        .args([
            "--kubeconfig",
            &kc_path,
            "logs",
            "-n",
            namespace,
            &pod_name,
            "-c",
            &container_name,
            "-f", // stream until container exits
        ])
        .output()
        .await
        .map_err(|e| format!("failed to spawn kubectl logs: {e}"))?;

    // kubectl logs returns exit 0 even when a container fails; a non-zero exit
    // usually means the pod/container doesn't exist yet.
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        Err(format!("kubectl logs failed: {stderr}"))
    }
}

/// Fetch the termination reason for a specific step from a TaskRun's status.
///
/// Returns values like `"Succeeded"`, `"Failed"`, `"Running"`, `"Unknown"`.
pub async fn fetch_step_status(
    kubeconfig_yaml: &str,
    namespace: &str,
    taskrun_name: &str,
    step_name: &str,
) -> Result<String, String> {
    let kc_file = write_kubeconfig(kubeconfig_yaml)?;
    let kc_path = kc_file.path().to_str().unwrap_or("/tmp/kc.yaml").to_string();
    let kubectl = find_kubectl();

    let output = tokio::process::Command::new(&kubectl)
        .args([
            "--kubeconfig",
            &kc_path,
            "get",
            "taskrun",
            taskrun_name,
            "-n",
            namespace,
            "-o",
            "json",
        ])
        .output()
        .await
        .map_err(|e| format!("failed to spawn kubectl get taskrun: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(format!("kubectl get taskrun failed: {stderr}"));
    }

    let json: serde_json::Value = serde_json::from_slice(&output.stdout)
        .map_err(|e| format!("failed to parse taskrun JSON: {e}"))?;

    // Strip the `step-` prefix when matching — the status uses the bare name.
    let bare_name = step_name
        .strip_prefix("step-")
        .unwrap_or(step_name);

    let status = json["status"]["steps"]
        .as_array()
        .and_then(|steps| {
            steps
                .iter()
                .find(|s| s["name"].as_str() == Some(bare_name))
        })
        .and_then(|step| {
            // Prefer terminated.reason; fall back to running/waiting signals.
            step["terminated"]["reason"]
                .as_str()
                .or_else(|| {
                    if step["running"].is_object() {
                        Some("Running")
                    } else {
                        None
                    }
                })
        })
        .unwrap_or("Unknown")
        .to_string();

    Ok(status)
}

// ── Internal ──────────────────────────────────────────────────────────────────

async fn ensure_single_pvc(
    kubeconfig_yaml: &str,
    namespace: &str,
    name: &str,
    spec_yaml: &str,
) -> Result<(), String> {
    let kubectl = find_kubectl();
    let kc_file = write_kubeconfig(kubeconfig_yaml)?;
    let kc_path = kc_file.path().to_str().unwrap_or("/tmp/kc.yaml").to_string();

    let status_out = tokio::process::Command::new(&kubectl)
        .args([
            "--kubeconfig",
            &kc_path,
            "get",
            "pvc",
            name,
            "-n",
            namespace,
            "-o",
            "jsonpath={.status.phase}",
            "--ignore-not-found",
        ])
        .output()
        .await
        .map_err(|e| format!("failed to check PVC {name}: {e}"))?;

    let phase = String::from_utf8_lossy(&status_out.stdout).trim().to_string();

    match phase.as_str() {
        "Bound" => {
            info!("[kubectl] PVC {name} already Bound — skipping");
            return Ok(());
        }
        "Pending" => {
            warn!("[kubectl] PVC {name} stuck in Pending — deleting for recreation");
            let _ = tokio::process::Command::new(&kubectl)
                .args([
                    "--kubeconfig",
                    &kc_path,
                    "delete",
                    "pvc",
                    name,
                    "-n",
                    namespace,
                    "--wait=false",
                ])
                .output()
                .await;
        }
        "" => info!("[kubectl] PVC {name} not found — creating"),
        other => {
            warn!("[kubectl] PVC {name} status: {other} — recreating");
            let _ = tokio::process::Command::new(&kubectl)
                .args([
                    "--kubeconfig",
                    &kc_path,
                    "delete",
                    "pvc",
                    name,
                    "-n",
                    namespace,
                    "--wait=false",
                ])
                .output()
                .await;
        }
    }

    let pvc_yaml = format!(
        "apiVersion: v1\nkind: PersistentVolumeClaim\nmetadata:\n  name: {name}\n  namespace: {namespace}\n{spec_yaml}\n",
    );

    match run_kubectl(&["--kubeconfig", &kc_path, "create", "-f", "-"], &pvc_yaml).await {
        Ok(_) => {
            info!("[kubectl] ✓ PVC {name} created");
            Ok(())
        }
        Err(e) if e.contains("already exists") => {
            info!("[kubectl] PVC {name} already exists (race) — ok");
            Ok(())
        }
        Err(e) => Err(format!("failed to create PVC {name}: {e}")),
    }
}

/// Spawn kubectl, write `stdin_data` concurrently with draining stdout/stderr,
/// return stdout on success or a descriptive error string.
async fn run_kubectl(args: &[&str], stdin_data: &str) -> Result<String, String> {
    let kubectl = find_kubectl();
    let mut child = tokio::process::Command::new(&kubectl)
        .args(args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("failed to spawn kubectl (tried {}): {e}", kubectl.display()))?;

    let stdin_bytes = stdin_data.as_bytes().to_vec();
    let mut stdin_handle = child.stdin.take().expect("stdin is piped");
    let write_task = tokio::spawn(async move {
        stdin_handle.write_all(&stdin_bytes).await
    });

    let output = child
        .wait_with_output()
        .await
        .map_err(|e| format!("kubectl wait failed: {e}"))?;

    if let Ok(Err(e)) = write_task.await {
        warn!("[kubectl] stdin write error (non-fatal): {e}");
    }

    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        Err(format!(
            "kubectl {} failed (exit {}): {}",
            args.first().unwrap_or(&""),
            output.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}