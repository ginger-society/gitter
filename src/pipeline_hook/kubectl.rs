/// Blocking Kubernetes helpers used by the pipeline hook binary.
///
/// The git-hook binary (`ginger-gitter-pipeline-hook`) has no async runtime,
/// so every helper here creates a single-threaded Tokio runtime and blocks on
/// the async kube calls. This keeps the public API synchronous while using the
/// same `kube` / `k8s-openapi` stack as the sidecar.

use std::collections::BTreeMap;

use k8s_openapi::api::core::v1::{
    Namespace, PersistentVolumeClaim, PersistentVolumeClaimSpec, PersistentVolume,
    VolumeResourceRequirements, Secret,
};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use kube::api::{Api, DeleteParams, Patch, PatchParams, PostParams};
use kube::config::{KubeConfigOptions, Kubeconfig};
use kube::{Client, Config as KubeConfig, ResourceExt};

// ── Runtime helper ────────────────────────────────────────────────────────────

pub fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("failed to build tokio runtime")
}

// ── Client construction ───────────────────────────────────────────────────────

async fn client_from_kubeconfig_async(kubeconfig_yaml: &str) -> Result<Client, String> {
    let kc: Kubeconfig = serde_yaml::from_str(kubeconfig_yaml)
        .map_err(|e| format!("failed to parse kubeconfig: {e}"))?;
    let opts = KubeConfigOptions::default();
    let cfg = KubeConfig::from_custom_kubeconfig(kc, &opts)
        .await
        .map_err(|e| format!("failed to build kube config: {e}"))?;
    Client::try_from(cfg).map_err(|e| format!("failed to create kube client: {e}"))
}

fn client_from_kubeconfig(kubeconfig_yaml: &str) -> Result<Client, String> {
    rt().block_on(client_from_kubeconfig_async(kubeconfig_yaml))
}

// ── Public surface (all blocking) ─────────────────────────────────────────────

/// Apply a raw YAML string via server-side apply.
/// Returns stdout-like confirmation string on success.
pub fn kubectl_apply(kubeconfig_yaml: &str, resource_yaml: &str) -> Result<String, String> {
    rt().block_on(apply_dynamic(kubeconfig_yaml, resource_yaml))
}

/// Ensure a Kubernetes namespace exists (idempotent).
pub fn ensure_namespace(kubeconfig_yaml: &str, namespace: &str) -> Result<(), String> {
    rt().block_on(async {
        let client = client_from_kubeconfig_async(kubeconfig_yaml).await?;
        let api: Api<Namespace> = Api::all(client);

        let ns = Namespace {
            metadata: ObjectMeta {
                name: Some(namespace.to_string()),
                ..Default::default()
            },
            ..Default::default()
        };

        let params = PatchParams::apply("ginger-gitter").force();
        api.patch(namespace, &params, &Patch::Apply(&ns))
            .await
            .map_err(|e| format!("failed to ensure namespace {namespace}: {e}"))?;

        println!("[ginger-gitter] Namespace {namespace} ensured");
        Ok(())
    })
}

/// Ensure the cluster-level NFS PersistentVolume for the buildah cache exists.
pub fn ensure_buildah_pv(kubeconfig_yaml: &str, namespace: &str) -> Result<(), String> {
    let pv_name = format!("buildah-cache-{namespace}-pv");

    rt().block_on(async {
        let client = client_from_kubeconfig_async(kubeconfig_yaml).await?;
        let api: Api<PersistentVolume> = Api::all(client);

        // Check if it already exists
        if api.get_opt(&pv_name).await.map_err(|e| e.to_string())?.is_some() {
            println!("[ginger-gitter] PV {pv_name} already exists — skipping");
            return Ok(());
        }

        let pv_yaml = format!(
            r#"apiVersion: v1
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
    path: /srv/nfs/buildah-cache
"#
        );

        apply_dynamic(kubeconfig_yaml, &pv_yaml).await.map(|out| {
            println!("[ginger-gitter] {pv_name}: {out}");
        })
    })
}

/// Ensure namespace-scoped PVCs exist and are Bound.
pub fn ensure_pvcs(kubeconfig_yaml: &str, namespace: &str) -> Result<(), String> {
    let pv_name = format!("buildah-cache-{namespace}-pv");

    rt().block_on(async {
        // general-purpose-cache-pvc (ReadWriteOnce, 20Gi)
        ensure_single_pvc_async(
            kubeconfig_yaml,
            namespace,
            "general-purpose-cache-pvc",
            "20Gi",
            vec!["ReadWriteOnce".to_string()],
            None,
            None,
        )
        .await?;

        // buildah-cache-pvc (ReadWriteMany, 100Gi, bound to the NFS PV)
        ensure_single_pvc_async(
            kubeconfig_yaml,
            namespace,
            "buildah-cache-pvc",
            "100Gi",
            vec!["ReadWriteMany".to_string()],
            Some(String::new()),     // storageClassName: ""
            Some(pv_name),
        )
        .await
    })
}

/// Ensure the `ginger-token-secret` Secret exists in the namespace.
pub fn ensure_ginger_token_secret(
    kubeconfig_yaml: &str,
    namespace: &str,
    token: &str,
) -> Result<(), String> {
    rt().block_on(async {
        let client = client_from_kubeconfig_async(kubeconfig_yaml).await?;
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

        let params = PatchParams::apply("ginger-gitter").force();
        api.patch("ginger-token-secret", &params, &Patch::Apply(&secret))
            .await
            .map_err(|e| format!("failed to ensure ginger-token-secret in {namespace}: {e}"))?;

        println!("[ginger-gitter] Secret ginger-token-secret ensured in {namespace}");
        Ok(())
    })
}

/// Ensure a deployment-target kubeconfig Secret exists in the namespace.
/// No-op when `deployment_kubeconfig` is None.
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
                "[ginger-gitter] No deployment target kubeconfig — skipping secret '{secret_name}'"
            );
            return Ok(());
        }
    };

    let kc_bytes = kc.as_bytes().to_vec();

    rt().block_on(async {
        let client = client_from_kubeconfig_async(tekton_kubeconfig).await?;
        let api: Api<Secret> = Api::namespaced(client, namespace);

        let mut labels = BTreeMap::new();
        labels.insert(
            "ginger-gitter/secret-type".to_string(),
            "deployment-target".to_string(),
        );

        let secret = Secret {
            metadata: ObjectMeta {
                name: Some(secret_name.to_string()),
                namespace: Some(namespace.to_string()),
                labels: Some(labels),
                ..Default::default()
            },
            type_: Some("Opaque".to_string()),
            data: Some(BTreeMap::from([(
                "kubeconfig.yaml".to_string(),
                k8s_openapi::ByteString(kc_bytes),
            )])),
            ..Default::default()
        };

        let params = PatchParams::apply("ginger-gitter").force();
        api.patch(secret_name, &params, &Patch::Apply(&secret))
            .await
            .map_err(|e| {
                format!("failed to ensure deployment target secret '{secret_name}': {e}")
            })?;

        println!(
            "[ginger-gitter] ✓ Deployment target secret '{secret_name}' applied"
        );
        Ok(())
    })
}

/// Create a PipelineRun resource (uses generateName — cannot use apply).
/// Returns the kubectl-style output: `pipelinerun.tekton.dev/<name>  created`.
pub fn create_pipeline_run(
    kubeconfig_yaml: &str,
    pipeline_run_yaml: &str,
) -> Result<String, String> {
    rt().block_on(create_dynamic(kubeconfig_yaml, pipeline_run_yaml))
}

// ── Internal ──────────────────────────────────────────────────────────────────

/// Server-side apply for any resource kind via the dynamic API.
async fn apply_dynamic(kubeconfig_yaml: &str, resource_yaml: &str) -> Result<String, String> {
    let client = client_from_kubeconfig_async(kubeconfig_yaml).await?;

    let value: serde_json::Value = serde_yaml::from_str(resource_yaml)
        .map_err(|e| format!("resource YAML parse error: {e}"))?;

    let api_version = value["apiVersion"]
        .as_str()
        .ok_or("missing apiVersion")?
        .to_string();
    let kind = value["kind"].as_str().ok_or("missing kind")?.to_string();
    let name = value["metadata"]["name"]
        .as_str()
        .ok_or("missing metadata.name")?
        .to_string();
    let namespace_val = value["metadata"]["namespace"]
        .as_str()
        .map(str::to_string);

    let (group, version) = parse_api_version(&api_version);
    let ar = kube::discovery::ApiResource {
        group,
        version,
        api_version: api_version.clone(),
        kind: kind.clone(),
        plural: kind_to_plural(&kind),
    };

    let mut dobj = kube::core::DynamicObject::new(&name, &ar);
    dobj.data = value;

    let params = PatchParams::apply("ginger-gitter").force();

    let result_name = if let Some(ns) = namespace_val {
        let api: Api<kube::core::DynamicObject> = Api::namespaced_with(client, &ns, &ar);
        api.patch(&name, &params, &Patch::Apply(&dobj))
            .await
            .map_err(|e| format!("apply failed for {kind}/{name}: {e}"))?
            .name_any()
    } else {
        let api: Api<kube::core::DynamicObject> = Api::all_with(client, &ar);
        api.patch(&name, &params, &Patch::Apply(&dobj))
            .await
            .map_err(|e| format!("apply failed for {kind}/{name}: {e}"))?
            .name_any()
    };

    println!("[ginger-gitter] {kind}/{result_name} applied");
    Ok(format!("{kind}/{result_name} applied"))
}

/// Create any resource kind via the dynamic API (for generateName resources).
async fn create_dynamic(kubeconfig_yaml: &str, resource_yaml: &str) -> Result<String, String> {
    let client = client_from_kubeconfig_async(kubeconfig_yaml).await?;

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
        .ok_or_else(|| "missing metadata.namespace".to_string())?;

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
    println!("[ginger-gitter] {kind}/{created_name} created");

    // Return kubectl-style output so callers that parse it still work
    Ok(format!(
        "{}.{}/{}  created",
        kind.to_lowercase(),
        "tekton.dev",
        created_name
    ))
}

async fn ensure_single_pvc_async(
    kubeconfig_yaml: &str,
    namespace: &str,
    name: &str,
    storage: &str,
    access_modes: Vec<String>,
    storage_class_name: Option<String>,
    volume_name: Option<String>,
) -> Result<(), String> {
    let client = client_from_kubeconfig_async(kubeconfig_yaml).await?;
    let api: Api<PersistentVolumeClaim> = Api::namespaced(client.clone(), namespace);

    match api.get_opt(name).await {
        Ok(Some(pvc)) => {
            let phase = pvc
                .status
                .as_ref()
                .and_then(|s| s.phase.as_deref())
                .unwrap_or("");
            match phase {
                "Bound" => {
                    println!("[ginger-gitter] PVC {name} already Bound — skipping");
                    return Ok(());
                }
                "Pending" => {
                    println!("[ginger-gitter] PVC {name} stuck in Pending — deleting for recreation");
                    api.delete(name, &DeleteParams::default())
                        .await
                        .map_err(|e| format!("failed to delete stuck PVC {name}: {e}"))?;
                }
                other => {
                    println!("[ginger-gitter] PVC {name} status: {other} — recreating");
                    api.delete(name, &DeleteParams::default())
                        .await
                        .map_err(|e| format!("failed to delete PVC {name}: {e}"))?;
                }
            }
        }
        Ok(None) => println!("[ginger-gitter] PVC {name} not found — creating"),
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
            println!("[ginger-gitter] ✓ PVC {name} created");
            Ok(())
        }
        Err(kube::Error::Api(ae)) if ae.code == 409 => {
            println!("[ginger-gitter] PVC {name} already exists (race) — ok");
            Ok(())
        }
        Err(e) => Err(format!("failed to create PVC {name}: {e}")),
    }
}

/// Split `apiVersion` into `(group, version)`.
fn parse_api_version(api_version: &str) -> (String, String) {
    if let Some((g, v)) = api_version.split_once('/') {
        (g.to_string(), v.to_string())
    } else {
        (String::new(), api_version.to_string())
    }
}

/// Best-effort plural form for kinds used in this project.
fn kind_to_plural(kind: &str) -> String {
    match kind {
        "Namespace"             => "namespaces".to_string(),
        "Secret"                => "secrets".to_string(),
        "PersistentVolume"      => "persistentvolumes".to_string(),
        "PersistentVolumeClaim" => "persistentvolumeclaims".to_string(),
        "Pipeline"              => "pipelines".to_string(),
        "PipelineRun"           => "pipelineruns".to_string(),
        "Task"                  => "tasks".to_string(),
        "TaskRun"               => "taskruns".to_string(),
        "Pod"                   => "pods".to_string(),
        other                   => format!("{}s", other.to_lowercase()),
    }
}