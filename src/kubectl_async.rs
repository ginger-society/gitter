/// Async Kubernetes helpers used by the sidecar HTTP handlers.
///
/// Uses the `kube` + `k8s-openapi` crates instead of shelling out to kubectl.
/// All helpers accept a raw kubeconfig YAML string and construct a scoped
/// `kube::Client` for every call so that cross-namespace / cross-cluster
/// operations remain isolated.

use std::collections::BTreeMap;

use k8s_openapi::api::core::v1::{
    Namespace, PersistentVolumeClaim, PersistentVolumeClaimSpec,
    VolumeResourceRequirements, Secret,
};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use kube::api::{Api, PostParams, Patch, PatchParams};
use kube::config::{KubeConfigOptions, Kubeconfig};
use kube::{Client, Config as KubeConfig, ResourceExt};
use tracing::{info, warn};

// ── Client construction ───────────────────────────────────────────────────────

/// Build a `kube::Client` from a raw kubeconfig YAML string.
pub async fn client_from_kubeconfig(kubeconfig_yaml: &str) -> Result<Client, String> {
    let kc: Kubeconfig = serde_yaml::from_str(kubeconfig_yaml)
        .map_err(|e| format!("failed to parse kubeconfig: {e}"))?;
    let opts = KubeConfigOptions::default();
    let cfg = KubeConfig::from_custom_kubeconfig(kc, &opts)
        .await
        .map_err(|e| format!("failed to build kube config: {e}"))?;
    Client::try_from(cfg).map_err(|e| format!("failed to create kube client: {e}"))
}

// ── Public surface ────────────────────────────────────────────────────────────

/// Ensure a Kubernetes namespace exists (idempotent server-side apply).
pub async fn ensure_namespace(kubeconfig_yaml: &str, namespace: &str) -> Result<(), String> {
    let client = client_from_kubeconfig(kubeconfig_yaml).await?;
    let api: Api<Namespace> = Api::all(client);

    let ns = Namespace {
        metadata: ObjectMeta {
            name: Some(namespace.to_string()),
            ..Default::default()
        },
        ..Default::default()
    };

    let params = PatchParams::apply("ginger-gitter-sidecar").force();
    api.patch(namespace, &params, &Patch::Apply(&ns))
        .await
        .map_err(|e| format!("failed to ensure namespace {namespace}: {e}"))?;

    info!("[kube] namespace {namespace} ensured");
    Ok(())
}

/// Ensure the `ginger-token-secret` Secret exists in the namespace.
pub async fn ensure_ginger_token_secret(
    kubeconfig_yaml: &str,
    namespace: &str,
    token: &str,
) -> Result<(), String> {
    let client = client_from_kubeconfig(kubeconfig_yaml).await?;
    let api: Api<Secret> = Api::namespaced(client, namespace);

    let token_bytes = token.trim().as_bytes().to_vec();

    let secret = Secret {
        metadata: ObjectMeta {
            name: Some("ginger-token-secret".to_string()),
            namespace: Some(namespace.to_string()),
            ..Default::default()
        },
        type_: Some("Opaque".to_string()),
        data: Some(BTreeMap::from([(
            "token".to_string(),
            k8s_openapi::ByteString(token_bytes),
        )])),
        ..Default::default()
    };

    let params = PatchParams::apply("ginger-gitter-sidecar").force();
    api.patch("ginger-token-secret", &params, &Patch::Apply(&secret))
        .await
        .map_err(|e| format!("failed to ensure ginger-token-secret in {namespace}: {e}"))?;

    info!("[kube] ginger-token-secret in {namespace} ensured");
    Ok(())
}

/// Ensure the `creds` PVC (SSH credentials workspace) exists.
pub async fn ensure_creds_pvc(kubeconfig_yaml: &str, namespace: &str) -> Result<(), String> {
    ensure_single_pvc(
        kubeconfig_yaml,
        namespace,
        "creds-pvc",
        "50Mi",
        vec!["ReadWriteOnce".to_string()],
        None,
        None,
    )
    .await
}

/// Ensure the `source` PVC (cloned repo workspace) exists.
pub async fn ensure_source_pvc(kubeconfig_yaml: &str, namespace: &str) -> Result<(), String> {
    ensure_single_pvc(
        kubeconfig_yaml,
        namespace,
        "source-pvc",
        "1Gi",
        vec!["ReadWriteOnce".to_string()],
        None,
        None,
    )
    .await
}

/// Fetch logs for a specific step container in a TaskRun pod.
///
/// Uses the kube streaming logs API with `follow=true` to stream until the
/// container exits. Returns the collected output as a String.
pub async fn fetch_step_logs(
    kubeconfig_yaml: &str,
    namespace: &str,
    taskrun_name: &str,
    step_name: &str,
) -> Result<String, String> {
    use futures::io::AsyncBufReadExt;
    use futures::StreamExt;
    use kube::api::LogParams;

    let client = client_from_kubeconfig(kubeconfig_yaml).await?;
    let pods: Api<k8s_openapi::api::core::v1::Pod> = Api::namespaced(client, namespace);

    // Tekton names the pod  <taskrun-name>-pod
    let pod_name = format!("{taskrun_name}-pod");
    // Tekton prefixes every step container with  step-
    let container_name = if step_name.starts_with("step-") {
        step_name.to_string()
    } else {
        format!("step-{step_name}")
    };

    let params = LogParams {
        container: Some(container_name),
        follow: true,
        ..Default::default()
    };

    // log_stream returns impl futures::AsyncBufRead — read lines until EOF
    let stream = pods
        .log_stream(&pod_name, &params)
        .await
        .map_err(|e| format!("failed to open log stream for {pod_name}: {e}"))?;

    let mut output = String::new();
    let mut lines = stream.lines();
    while let Some(line) = lines.next().await {
        match line {
            Ok(l) => {
                output.push_str(&l);
                output.push('\n');
            }
            Err(e) => {
                warn!("[kube] log stream error: {e}");
                break;
            }
        }
    }

    Ok(output)
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
    use kube::api::GetParams;

    // TaskRun is a CRD — use the dynamic API via serde_json::Value
    let client = client_from_kubeconfig(kubeconfig_yaml).await?;

    let gvr = kube::discovery::ApiResource {
        group: "tekton.dev".to_string(),
        version: "v1beta1".to_string(),
        api_version: "tekton.dev/v1beta1".to_string(),
        kind: "TaskRun".to_string(),
        plural: "taskruns".to_string(),
    };

    let api: Api<kube::core::DynamicObject> =
        Api::namespaced_with(client, namespace, &gvr);

    let obj = api
        .get_with(taskrun_name, &GetParams::default())
        .await
        .map_err(|e| format!("failed to get taskrun {taskrun_name}: {e}"))?;

    let bare_name = step_name.strip_prefix("step-").unwrap_or(step_name);

    let status = obj.data["status"]["steps"]
        .as_array()
        .and_then(|steps| {
            steps
                .iter()
                .find(|s| s["name"].as_str() == Some(bare_name))
        })
        .and_then(|step| {
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

/// Apply a raw YAML string using server-side apply.
/// Parses the YAML as a `DynamicObject` so it works for any resource kind.
pub async fn kubectl_apply(kubeconfig_yaml: &str, resource_yaml: &str) -> Result<String, String> {
    let client = client_from_kubeconfig(kubeconfig_yaml).await?;

    // Parse the resource YAML to extract group/version/kind/name/namespace
    let value: serde_json::Value = serde_yaml::from_str(resource_yaml)
        .map_err(|e| format!("resource YAML parse error: {e}"))?;

    let api_version = value["apiVersion"]
        .as_str()
        .ok_or("missing apiVersion")?
        .to_string();
    let kind = value["kind"]
        .as_str()
        .ok_or("missing kind")?
        .to_string();
    let name = value["metadata"]["name"]
        .as_str()
        .ok_or("missing metadata.name")?
        .to_string();
    let namespace_val = value["metadata"]["namespace"]
        .as_str()
        .map(str::to_string);

    let (group, version) = parse_api_version(&api_version);

    let ar = kube::discovery::ApiResource {
        group: group.clone(),
        version: version.clone(),
        api_version: api_version.clone(),
        kind: kind.clone(),
        plural: kind_to_plural(&kind),
    };

    let mut dobj = kube::core::DynamicObject::new(&name, &ar);
    dobj.data = value.clone();

    let params = PatchParams::apply("ginger-gitter-sidecar").force();

    let result = if let Some(ns) = namespace_val {
        let api: Api<kube::core::DynamicObject> = Api::namespaced_with(client, &ns, &ar);
        api.patch(&name, &params, &Patch::Apply(&dobj))
            .await
            .map_err(|e| format!("server-side apply failed for {kind}/{name}: {e}"))?
    } else {
        let api: Api<kube::core::DynamicObject> = Api::all_with(client, &ar);
        api.patch(&name, &params, &Patch::Apply(&dobj))
            .await
            .map_err(|e| format!("server-side apply failed for {kind}/{name}: {e}"))?
    };

    let result_name = result.name_any();
    info!("[kube] applied {kind}/{result_name}");
    Ok(format!("{kind}/{result_name} applied"))
}

/// Create a resource from raw YAML — required for resources using `generateName`.
/// Returns the created resource name.
pub async fn kubectl_create(kubeconfig_yaml: &str, resource_yaml: &str) -> Result<String, String> {
    let client = client_from_kubeconfig(kubeconfig_yaml).await?;

    let value: serde_json::Value = serde_yaml::from_str(resource_yaml)
        .map_err(|e| format!("resource YAML parse error: {e}"))?;

    let api_version = value["apiVersion"]
        .as_str()
        .ok_or("missing apiVersion")?
        .to_string();
    let kind = value["kind"].as_str().ok_or("missing kind")?.to_string();
    let namespace_val = value["metadata"]["namespace"]
        .as_str()
        .map(str::to_string)
        .ok_or("missing metadata.namespace")?;

    let (group, version) = parse_api_version(&api_version);

    let ar = kube::discovery::ApiResource {
        group,
        version,
        api_version,
        kind: kind.clone(),
        plural: kind_to_plural(&kind),
    };

    let mut dobj = kube::core::DynamicObject::new("", &ar);
    dobj.data = value;

    let api: Api<kube::core::DynamicObject> =
        Api::namespaced_with(client, &namespace_val, &ar);

    let created = api
        .create(&PostParams::default(), &dobj)
        .await
        .map_err(|e| format!("create failed for {kind}: {e}"))?;

    let created_name = created.name_any();
    info!("[kube] created {kind}/{created_name}");
    // Mimic kubectl output format so callers that parse it still work
    Ok(format!("{}.{}/{}  created", kind.to_lowercase(), "tekton.dev", created_name))
}

// ── Internal ──────────────────────────────────────────────────────────────────

async fn ensure_single_pvc(
    kubeconfig_yaml: &str,
    namespace: &str,
    name: &str,
    storage: &str,
    access_modes: Vec<String>,
    storage_class_name: Option<String>,
    volume_name: Option<String>,
) -> Result<(), String> {
    let client = client_from_kubeconfig(kubeconfig_yaml).await?;
    let api: Api<PersistentVolumeClaim> = Api::namespaced(client.clone(), namespace);

    // Check current phase
    match api.get_opt(name).await {
        Ok(Some(pvc)) => {
            let phase = pvc
                .status
                .as_ref()
                .and_then(|s| s.phase.as_deref())
                .unwrap_or("");
            match phase {
                "Bound" => {
                    info!("[kube] PVC {name} already Bound — skipping");
                    return Ok(());
                }
                "Pending" => {
                    warn!("[kube] PVC {name} stuck in Pending — deleting for recreation");
                    api.delete(name, &Default::default())
                        .await
                        .map_err(|e| format!("failed to delete stuck PVC {name}: {e}"))?;
                }
                other => {
                    warn!("[kube] PVC {name} status: {other} — recreating");
                    api.delete(name, &Default::default())
                        .await
                        .map_err(|e| format!("failed to delete PVC {name}: {e}"))?;
                }
            }
        }
        Ok(None) => info!("[kube] PVC {name} not found — creating"),
        Err(e) => return Err(format!("failed to check PVC {name}: {e}")),
    }

    let mut requests = BTreeMap::new();
    requests.insert("storage".to_string(), Quantity(storage.to_string()));

    let pvc = PersistentVolumeClaim {
        metadata: ObjectMeta {
            name: Some(name.to_string()),
            namespace: Some(namespace.to_string()),
            ..Default::default()
        },
        spec: Some(PersistentVolumeClaimSpec {
            access_modes: Some(access_modes),
            resources: Some(VolumeResourceRequirements {
                requests: Some(requests),
                ..Default::default()
            }),
            storage_class_name,
            volume_name,
            ..Default::default()
        }),
        ..Default::default()
    };

    let api2: Api<PersistentVolumeClaim> = Api::namespaced(client, namespace);
    match api2.create(&PostParams::default(), &pvc).await {
        Ok(_) => {
            info!("[kube] ✓ PVC {name} created");
            Ok(())
        }
        Err(kube::Error::Api(ae)) if ae.code == 409 => {
            info!("[kube] PVC {name} already exists (race) — ok");
            Ok(())
        }
        Err(e) => Err(format!("failed to create PVC {name}: {e}")),
    }
}

/// Split `apiVersion` into `(group, version)`.
/// `"v1"` → `("", "v1")`, `"apps/v1"` → `("apps", "v1")`, `"tekton.dev/v1beta1"` → `("tekton.dev", "v1beta1")`.
fn parse_api_version(api_version: &str) -> (String, String) {
    if let Some((g, v)) = api_version.split_once('/') {
        (g.to_string(), v.to_string())
    } else {
        (String::new(), api_version.to_string())
    }
}

/// Best-effort plural form. Covers the resource types used in this project.
fn kind_to_plural(kind: &str) -> String {
    match kind {
        "Namespace"               => "namespaces".to_string(),
        "Secret"                  => "secrets".to_string(),
        "PersistentVolume"        => "persistentvolumes".to_string(),
        "PersistentVolumeClaim"   => "persistentvolumeclaims".to_string(),
        "Pipeline"                => "pipelines".to_string(),
        "PipelineRun"             => "pipelineruns".to_string(),
        "Task"                    => "tasks".to_string(),
        "TaskRun"                 => "taskruns".to_string(),
        "Pod"                     => "pods".to_string(),
        other                     => format!("{}s", other.to_lowercase()),
    }
}