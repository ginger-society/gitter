use std::io::Write;
use std::path::Path;
use std::process::Command;

use tempfile::NamedTempFile;

/// Locate the kubectl binary. Git hooks run with a minimal PATH so we probe
/// common install locations explicitly before falling back to a PATH lookup.
fn find_kubectl() -> std::path::PathBuf {
    const CANDIDATES: &[&str] = &[
        "/usr/local/bin/kubectl",
        "/usr/bin/kubectl",
        "/bin/kubectl",
        "/snap/bin/kubectl",
        "/opt/homebrew/bin/kubectl",
    ];
    for path in CANDIDATES {
        if Path::new(path).exists() {
            return std::path::PathBuf::from(path);
        }
    }
    std::path::PathBuf::from("kubectl")
}

/// Write kubeconfig content to a NamedTempFile.
/// The file is automatically deleted when the returned value is dropped.
fn write_kubeconfig(content: &str) -> Result<NamedTempFile, String> {
    let mut f = NamedTempFile::new()
        .map_err(|e| format!("failed to create temp kubeconfig: {e}"))?;
    f.write_all(content.as_bytes())
        .map_err(|e| format!("failed to write temp kubeconfig: {e}"))?;
    Ok(f)
}

/// Write kubeconfig to a temp file, run kubectl apply with it, then clean up.
/// Returns stdout on success, Err with stderr on failure.
pub fn kubectl_apply(kubeconfig_yaml: &str, resource_yaml: &str) -> Result<String, String> {
    let kc_file = write_kubeconfig(kubeconfig_yaml)?;
    run_kubectl_apply(kc_file.path(), resource_yaml)
}

/// Ensure a Kubernetes namespace exists (idempotent).
pub fn ensure_namespace(kubeconfig_yaml: &str, namespace: &str) -> Result<(), String> {
    let kc_file = write_kubeconfig(kubeconfig_yaml)?;
    run_kubectl_ensure_namespace(kc_file.path(), namespace)
}

/// Ensure the cluster-level NFS PersistentVolume for the buildah cache exists.
/// Cluster-scoped (no namespace). Safe to call on every run — idempotent.
/// NFS server and path match the cluster runbook: 172.18.0.1:/srv/nfs/buildah-cache
pub fn ensure_buildah_pv(kubeconfig_yaml: &str, namespace: &str) -> Result<(), String> {
    let pv_name = format!("buildah-cache-{}-pv", namespace);
    let pv_yaml = format!(r#"apiVersion: v1
kind: PersistentVolume
metadata:
  name: {pv_name}
spec:
  capacity:
    storage: 100Gi
  accessModes:
    - ReadWriteMany
  persistentVolumeReclaimPolicy: Retain
  storageClassName: ""
  nfs:
    server: 172.18.0.1
    path: /srv/nfs/buildah-cache-{namespace}
"#,
        pv_name = pv_name,
        namespace = namespace,
    );
    kubectl_apply(kubeconfig_yaml, &pv_yaml).map(|out| {
        println!("[ginger-gitter] {}: {}", pv_name, out.trim());
    })
}

/// Ensure namespace-scoped PVCs exist and are Bound.
/// Call after ensure_buildah_pv so the NFS PV is ready before the PVC binds.
///
/// PVCs are immutable once created — we can't apply changes to a stuck Pending
/// PVC. Instead we check status: if Pending we delete and recreate it so it
/// gets a fresh binding attempt against the now-present PV.
pub fn ensure_pvcs(kubeconfig_yaml: &str, namespace: &str) -> Result<(), String> {
    ensure_single_pvc(
        kubeconfig_yaml,
        namespace,
        "general-purpose-cache-pvc",
        r#"spec:
  accessModes:
    - ReadWriteOnce
  resources:
    requests:
      storage: 20Gi"#,
    )?;

    let pv_name = format!("buildah-cache-{}-pv", namespace);
    let buildah_spec = format!(r#"spec:
  accessModes:
    - ReadWriteMany
  resources:
    requests:
      storage: 100Gi
  storageClassName: ""
  volumeName: {pv_name}"#,
        pv_name = pv_name,
    );
    ensure_single_pvc(
        kubeconfig_yaml,
        namespace,
        "buildah-cache-pvc",
        &buildah_spec,
    )?;

    Ok(())
}

/// Create a PVC if it doesn't exist, or recreate it if it's stuck in Pending.
/// Bound PVCs are left completely untouched.
fn ensure_single_pvc(
    kubeconfig_yaml: &str,
    namespace: &str,
    name: &str,
    spec_yaml: &str,
) -> Result<(), String> {
    let kubectl = find_kubectl();
    // Single temp file reused for all kubectl calls in this function.
    // Dropped (and deleted) automatically at end of scope.
    let kc_file = write_kubeconfig(kubeconfig_yaml)?;
    let kc_path = kc_file.path().to_str().unwrap_or("/tmp/kc.yaml");

    // Check current status
    let status_out = Command::new(&kubectl)
        .args([
            "--kubeconfig", kc_path,
            "get", "pvc", name,
            "-n", namespace,
            "-o", "jsonpath={.status.phase}",
            "--ignore-not-found",
        ])
        .output()
        .map_err(|e| format!("failed to check PVC {}: {}", name, e))?;

    let phase = String::from_utf8_lossy(&status_out.stdout).trim().to_string();

    match phase.as_str() {
        "Bound" => {
            println!("[ginger-gitter] PVC {} already Bound — skipping", name);
            return Ok(());
        }
        "Pending" => {
            println!(
                "[ginger-gitter] PVC {} stuck in Pending — deleting for recreation",
                name
            );
            let _ = Command::new(&kubectl)
                .args([
                    "--kubeconfig", kc_path,
                    "delete", "pvc", name,
                    "-n", namespace,
                    "--wait=false",
                ])
                .output();
        }
        "" => {
            println!("[ginger-gitter] PVC {} not found — creating", name);
        }
        other => {
            println!("[ginger-gitter] PVC {} status: {} — recreating", name, other);
            let _ = Command::new(&kubectl)
                .args([
                    "--kubeconfig", kc_path,
                    "delete", "pvc", name,
                    "-n", namespace,
                    "--wait=false",
                ])
                .output();
        }
    }

    let pvc_yaml = format!(
        "apiVersion: v1
kind: PersistentVolumeClaim
metadata:
  name: {name}
  namespace: {namespace}
{spec_yaml}
",
        name = name,
        namespace = namespace,
        spec_yaml = spec_yaml,
    );

    let mut child = Command::new(&kubectl)
        .args([
            "--kubeconfig", kc_path,
            "create", "-f", "-",
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("failed to spawn kubectl create pvc: {}", e))?;

    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(pvc_yaml.as_bytes())
            .map_err(|e| format!("failed to write PVC yaml: {}", e))?;
    }

    let out = child.wait_with_output()
        .map_err(|e| format!("kubectl create pvc wait failed: {}", e))?;

    if out.status.success() {
        println!("[ginger-gitter] ✓ PVC {} created", name);
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&out.stderr);
        if stderr.contains("already exists") {
            println!("[ginger-gitter] PVC {} already exists (race) — ok", name);
            return Ok(());
        }
        Err(format!("failed to create PVC {}: {}", name, stderr.trim()))
    }
}

/// Ensure the `ginger-token-secret` Secret exists in the namespace.
/// The token is the raw string from the admin repo's pipeline-tokens/<workspace>.
pub fn ensure_ginger_token_secret(
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
"#,
        namespace = namespace,
        token_b64 = token_b64
    );

    kubectl_apply(kubeconfig_yaml, &secret_yaml).map(|out| {
        println!("[ginger-gitter] Secret apply output: {}", out.trim());
    })
}

/// Ensure a deployment-target kubeconfig Secret exists in the namespace.
/// Named after the branch so concurrent pipelines on different branches
/// never clash: deployment-target-dev-alice, deployment-target-main, etc.
/// Safe to call on every push — overwrites with the latest kubeconfig.
/// When kubeconfig is None (no environment provisioned yet) this is a no-op.
pub fn ensure_deployment_target_secret(
    tekton_kubeconfig: &str,
    namespace: &str,
    secret_name: &str,
    deployment_kubeconfig: &Option<String>,
) -> Result<(), String> {
    let kc = match deployment_kubeconfig {
        Some(kc) => kc,
        None => {
            println!(
                "[ginger-gitter] No deployment target kubeconfig — skipping secret '{}'",
                secret_name
            );
            return Ok(());
        }
    };

    use base64::{engine::general_purpose::STANDARD, Engine};
    let kc_b64 = STANDARD.encode(kc.as_bytes());

    let secret_yaml = format!(
        r#"apiVersion: v1
kind: Secret
metadata:
  name: {secret_name}
  namespace: {namespace}
  labels:
    ginger-gitter/secret-type: deployment-target
type: Opaque
data:
  kubeconfig.yaml: {kc_b64}
"#,
        secret_name = secret_name,
        namespace = namespace,
        kc_b64 = kc_b64,
    );

    kubectl_apply(tekton_kubeconfig, &secret_yaml).map(|out| {
        println!(
            "[ginger-gitter] ✓ Deployment target secret '{}' applied: {}",
            secret_name,
            out.trim()
        );
    })
}

/// Apply a PipelineRun and return the created resource name.
pub fn create_pipeline_run(kubeconfig_yaml: &str, pipeline_run_yaml: &str) -> Result<String, String> {
    let kc_file = write_kubeconfig(kubeconfig_yaml)?;
    run_kubectl_create(kc_file.path(), pipeline_run_yaml)
}

// ── Internal helpers ──────────────────────────────────────────────────────────

fn run_kubectl_apply(kc_path: &Path, resource_yaml: &str) -> Result<String, String> {
    let kubectl = find_kubectl();
    let mut child = Command::new(&kubectl)
        .args([
            "--kubeconfig",
            kc_path.to_str().unwrap_or("/tmp/kc.yaml"),
            "apply",
            "--server-side",
            "--force-conflicts",
            "-f",
            "-",
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("failed to spawn kubectl (tried {}): {}", kubectl.display(), e))?;

    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(resource_yaml.as_bytes())
            .map_err(|e| format!("failed to write to kubectl stdin: {}", e))?;
    }

    let output = child
        .wait_with_output()
        .map_err(|e| format!("kubectl wait failed: {}", e))?;

    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        Err(format!(
            "kubectl apply failed (exit {}): {}",
            output.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

/// `kubectl create` is used for PipelineRun because it uses generateName —
/// `apply` does not support generateName.
fn run_kubectl_create(kc_path: &Path, resource_yaml: &str) -> Result<String, String> {
    let kubectl = find_kubectl();
    let mut child = Command::new(&kubectl)
        .args([
            "--kubeconfig",
            kc_path.to_str().unwrap_or("/tmp/kc.yaml"),
            "create",
            "-f",
            "-",
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("failed to spawn kubectl (tried {}): {}", kubectl.display(), e))?;

    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(resource_yaml.as_bytes())
            .map_err(|e| format!("failed to write to kubectl stdin: {}", e))?;
    }

    let output = child
        .wait_with_output()
        .map_err(|e| format!("kubectl create wait failed: {}", e))?;

    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        Err(format!(
            "kubectl create failed (exit {}): {}",
            output.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

fn run_kubectl_ensure_namespace(kc_path: &Path, namespace: &str) -> Result<(), String> {
    let ns_yaml = format!(
        "apiVersion: v1\nkind: Namespace\nmetadata:\n  name: {}\n",
        namespace
    );

    let kubectl = find_kubectl();
    let mut child = Command::new(&kubectl)
        .args([
            "--kubeconfig",
            kc_path.to_str().unwrap_or("/tmp/kc.yaml"),
            "apply",
            "-f",
            "-",
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("failed to spawn kubectl (tried {}): {}", kubectl.display(), e))?;

    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(ns_yaml.as_bytes())
            .map_err(|e| format!("failed to write namespace yaml: {}", e))?;
    }

    let output = child
        .wait_with_output()
        .map_err(|e| format!("kubectl namespace wait failed: {}", e))?;

    if output.status.success() {
        println!("[ginger-gitter] Namespace {} ensured", namespace);
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("already exists") {
            println!("[ginger-gitter] Namespace {} already exists", namespace);
            return Ok(());
        }
        Err(format!("kubectl apply namespace failed: {}", stderr.trim()))
    }
}