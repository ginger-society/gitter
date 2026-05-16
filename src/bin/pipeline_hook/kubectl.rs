use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::process::Command;

/// Locate the kubectl binary. Git hooks run with a minimal PATH so we probe
/// common install locations explicitly before falling back to a PATH lookup.
fn find_kubectl() -> PathBuf {
    const CANDIDATES: &[&str] = &[
        "/usr/local/bin/kubectl",
        "/usr/bin/kubectl",
        "/bin/kubectl",
        "/snap/bin/kubectl",
        "/opt/homebrew/bin/kubectl",
    ];
    for path in CANDIDATES {
        if std::path::Path::new(path).exists() {
            return PathBuf::from(path);
        }
    }
    // Last resort: rely on PATH (will produce os error 2 if truly missing)
    PathBuf::from("kubectl")
}

/// Write kubeconfig to a temp file, run kubectl with it, then clean up.
/// Returns stdout on success, Err with stderr on failure.
pub fn kubectl_apply(kubeconfig_yaml: &str, resource_yaml: &str) -> Result<String, String> {
    let kc_path = write_temp_file("ginger-gitter-kc-", ".yaml", kubeconfig_yaml)?;
    let result = run_kubectl_apply(&kc_path, resource_yaml);
    let _ = fs::remove_file(&kc_path);
    result
}

/// Ensure a Kubernetes namespace exists (idempotent).
pub fn ensure_namespace(kubeconfig_yaml: &str, namespace: &str) -> Result<(), String> {
    let kc_path = write_temp_file("ginger-gitter-kc-", ".yaml", kubeconfig_yaml)?;
    let result = run_kubectl_ensure_namespace(&kc_path, namespace);
    let _ = fs::remove_file(&kc_path);
    result
}

/// Ensure the two persistent volume claims exist in the namespace.
/// Uses `kubectl apply` with a PVC manifest so it's idempotent.
/// Ensure the cluster-level NFS PersistentVolume for the buildah cache exists.
/// Cluster-scoped (no namespace). Safe to call on every run — idempotent.
/// NFS server and path match the cluster runbook: 172.18.0.1:/srv/nfs/buildah-cache
pub fn ensure_buildah_pv(kubeconfig_yaml: &str) -> Result<(), String> {
    let pv_yaml = r#"apiVersion: v1
kind: PersistentVolume
metadata:
  name: buildah-cache-pv
spec:
  capacity:
    storage: 100Gi
  accessModes:
    - ReadWriteMany
  persistentVolumeReclaimPolicy: Retain
  storageClassName: ""
  nfs:
    server: 172.18.0.1
    path: /srv/nfs/buildah-cache
"#;

    kubectl_apply(kubeconfig_yaml, pv_yaml).map(|out| {
        println!("[ginger-gitter] buildah-cache-pv: {}", out.trim());
    })
}

/// Ensure namespace-scoped PVCs exist.
/// Call after ensure_buildah_pv so the NFS PV is ready before the PVC binds.
pub fn ensure_pvcs(kubeconfig_yaml: &str, namespace: &str) -> Result<(), String> {
    let general_pvc = format!(
        r#"apiVersion: v1
kind: PersistentVolumeClaim
metadata:
  name: general-purpose-cache-pvc
  namespace: {namespace}
spec:
  accessModes:
    - ReadWriteOnce
  resources:
    requests:
      storage: 20Gi
"#,
        namespace = namespace
    );

    let buildah_pvc = format!(
        r#"apiVersion: v1
kind: PersistentVolumeClaim
metadata:
  name: buildah-cache-pvc
  namespace: {namespace}
spec:
  accessModes:
    - ReadWriteMany
  resources:
    requests:
      storage: 100Gi
  storageClassName: ""
  volumeName: buildah-cache-pv
"#,
        namespace = namespace
    );

    let combined = format!("{}\n---\n{}", general_pvc, buildah_pvc);

    kubectl_apply(kubeconfig_yaml, &combined).map(|out| {
        println!("[ginger-gitter] PVC apply output: {}", out.trim());
    })
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
    let kc_path = write_temp_file("ginger-gitter-kc-", ".yaml", kubeconfig_yaml)?;
    let result = run_kubectl_create(&kc_path, pipeline_run_yaml);
    let _ = fs::remove_file(&kc_path);
    result
}

// ── Internal helpers ──────────────────────────────────────────────────────────

fn write_temp_file(prefix: &str, suffix: &str, content: &str) -> Result<PathBuf, String> {
    let path = std::env::temp_dir().join(format!(
        "{}{}{}",
        prefix,
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .subsec_nanos(),
        suffix
    ));
    let mut f = fs::File::create(&path)
        .map_err(|e| format!("failed to create temp kubeconfig: {}", e))?;
    f.write_all(content.as_bytes())
        .map_err(|e| format!("failed to write temp kubeconfig: {}", e))?;
    Ok(path)
}

fn run_kubectl_apply(kc_path: &PathBuf, resource_yaml: &str) -> Result<String, String> {
    let kubectl = find_kubectl();
    let mut child = Command::new(&kubectl)
        .args([
            "--kubeconfig",
            kc_path.to_str().unwrap_or("/tmp/kc.yaml"),
            "apply",
            "--server-side",
            "-f",
            "-",
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!(
            "failed to spawn kubectl (tried {}): {}",
            kubectl.display(), e
        ))?;

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
fn run_kubectl_create(kc_path: &PathBuf, resource_yaml: &str) -> Result<String, String> {
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
        .map_err(|e| format!(
            "failed to spawn kubectl (tried {}): {}",
            kubectl.display(), e
        ))?;

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

fn run_kubectl_ensure_namespace(kc_path: &PathBuf, namespace: &str) -> Result<(), String> {
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
        .map_err(|e| format!(
            "failed to spawn kubectl (tried {}): {}",
            kubectl.display(), e
        ))?;

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
        // Already exists is not fatal
        if stderr.contains("already exists") {
            println!("[ginger-gitter] Namespace {} already exists", namespace);
            return Ok(());
        }
        Err(format!(
            "kubectl apply namespace failed: {}",
            stderr.trim()
        ))
    }
}
