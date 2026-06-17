use std::collections::HashMap;

use serde_yaml::{Mapping, Value};

// ── Constants ─────────────────────────────────────────────────────────────────

const INJECTED_WORKSPACES: &[&str] = &[
    "creds",
    "source",
    "general-purpose-cache",
    "buildah-cache",
];

/// System params injected into every Pipeline's spec.params.
/// Must match exactly what build_pipeline_run writes into the PipelineRun params.
const SYSTEM_PARAMS: &[(&str, &str)] = &[
    ("gl_user",    "string"),
    ("gl_repo",    "string"),
    ("gl_refname", "string"),
    ("gl_new_rev", "string"),
    ("image_tag",  "string"),
];

/// Placeholder the user writes in secretKeyRef.name to reference the
/// deployment-target kubeconfig secret. Replaced at transform time with the
/// real branch-scoped secret name so Kubernetes never sees a Tekton variable
/// expression in a field it validates before Tekton can substitute it.
pub const DEPLOYMENT_TARGET_PLACEHOLDER: &str = "ginger-gitter/deployment-target";

// ── Public entry points ───────────────────────────────────────────────────────

/// Transform a user-written Task YAML:
/// - inject namespace only if not already specified in the YAML
/// - replace deployment-target placeholder with real secret name
/// - inject workspaces into spec.workspaces
/// - inject GINGER_TOKEN env into every step
pub fn transform_task(
    yaml: &str,
    namespace: &str,
    deployment_target_secret: &str,
) -> Result<String, String> {
    // Placeholder replacement is intentionally a plain string operation —
    // it needs to work regardless of quoting style before we parse the YAML.
    let yaml = yaml.replace(DEPLOYMENT_TARGET_PLACEHOLDER, deployment_target_secret);

    let mut doc = parse(&yaml)?;

    // Only inject namespace when the task author hasn't specified one.
    set_namespace_if_missing(&mut doc, namespace);
    inject_task_workspaces(&mut doc);
    inject_ginger_token_env(&mut doc);

    serialize(&doc)
}

/// Transform a user-written Pipeline YAML:
/// - inject namespace (always — pipelines are always scoped to the derived namespace)
/// - inject system params into spec.params
/// - inject workspaces into spec.workspaces and each task's workspaces
/// - prepend init-credentials → clone tasks
/// - set runAfter: [clone] on the first user task
pub fn transform_pipeline(
    yaml: &str,
    namespace: &str,
    gl_repo: &str,
) -> Result<String, String> {
    let mut doc = parse(yaml)?;

    // Pipelines always get the derived namespace — no conditional here.
    set_namespace(&mut doc, namespace);
    inject_system_params(&mut doc);
    inject_pipeline_workspaces(&mut doc);
    inject_builtin_pipeline_tasks(&mut doc, gl_repo);

    serialize(&doc)
}

/// Build a complete PipelineRun YAML for a triggered pipeline.
///
/// System params (gl_user, gl_repo, etc.) are single-quoted — they are simple
/// alphanumeric/path strings that never contain single quotes.
///
/// User-supplied params (vault, values, arbitrary JSON, etc.) are serialized as
/// YAML literal block scalars (`|`). This means ANY content — JSON with `{:}'"`
/// characters, shell expressions with `$`, multi-line strings — is treated as a
/// plain string by the YAML parser with zero escaping required. The block scalar
/// appends a trailing newline, which callers should handle (e.g. `printf '%s'`
/// rather than `echo`, or `| tr -d '\n'` when writing to a file).
pub fn build_pipeline_run(
    pipeline_name: &str,
    namespace: &str,
    user_params: &HashMap<String, String>,
    gl_user: &str,
    gl_repo: &str,
    gl_refname: &str,
    gl_new_rev: &str,
) -> String {
    let image_tag = gl_refname
        .strip_prefix("refs/heads/")
        .unwrap_or(gl_refname);

    let ref_label  = sanitize_label(image_tag);
    let repo_label = sanitize_label(gl_repo);
    let sha_label  = &gl_new_rev[..8.min(gl_new_rev.len())];

    let mut params_yaml = String::new();

    // System params: simple strings, safe to single-quote.
    for (k, v) in &[
        ("gl_user",    gl_user),
        ("gl_repo",    gl_repo),
        ("gl_refname", gl_refname),
        ("gl_new_rev", gl_new_rev),
        ("image_tag",  image_tag),
    ] {
        params_yaml.push_str(&format!("    - name: {}\n      value: '{}'\n", k, v));
    }

    // User params: may contain JSON, shell expressions, or any special
    // characters. Use a YAML literal block scalar (`|`) so the YAML parser
    // never interprets the content — no escaping needed for any character.
    // Continuation lines must be indented by 8 spaces to stay inside the
    // block scalar (2 for list item + 6 for value indentation).
    for (k, v) in user_params {
        let indented = v.replace('\n', "\n        ");
        params_yaml.push_str(&format!(
            "    - name: {}\n      value: |\n        {}\n",
            k, indented
        ));
    }

    format!(
        r#"apiVersion: tekton.dev/v1beta1
kind: PipelineRun
metadata:
  generateName: {pipeline_name}-run-
  namespace: {namespace}
  labels:
    ginger-gitter/repo: "{repo_label}"
    ginger-gitter/branch: "{ref_label}"
    ginger-gitter/sha: "{sha_label}"
  annotations:
    ginger-gitter/repo: "{gl_repo}"
    ginger-gitter/ref: "{gl_refname}"
    ginger-gitter/sha: "{gl_new_rev}"
spec:
  pipelineRef:
    name: {pipeline_name}
  params:
{params_yaml}  workspaces:
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
    - name: general-purpose-cache
      persistentVolumeClaim:
        claimName: general-purpose-cache-pvc
    - name: buildah-cache
      persistentVolumeClaim:
        claimName: buildah-cache-pvc
"#,
        pipeline_name = pipeline_name,
        namespace = namespace,
        repo_label = repo_label,
        ref_label = ref_label,
        sha_label = sha_label,
        gl_repo = gl_repo,
        gl_refname = gl_refname,
        gl_new_rev = gl_new_rev,
        params_yaml = params_yaml,
    )
}

/// Build the fixed init-credentials Task YAML for a namespace.
pub fn builtin_init_credentials_task(namespace: &str) -> String {
    format!(r#"apiVersion: tekton.dev/v1beta1
kind: Task
metadata:
  name: init-credentials
  namespace: {namespace}
spec:
  workspaces:
    - name: creds
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
"#, namespace = namespace)
}

/// Build the fixed clone Task YAML for a namespace.
pub fn builtin_clone_task(namespace: &str) -> String {
    format!(r#"apiVersion: tekton.dev/v1beta1
kind: Task
metadata:
  name: clone
  namespace: {namespace}
spec:
  params:
    - name: repo
      type: string
  workspaces:
    - name: creds
    - name: source
  steps:
    - name: clone
      image: gingersociety/tekton-task-gitter:latest
      imagePullPolicy: Always
      script: |
        #!/bin/bash
        set -e
        /usr/local/bin/mount-git-credentials.sh
        git config --global init.defaultBranch main
        git clone git@source.gingersociety.org:$(params.repo).git /workspace/source/repo
        echo "Repository cloned successfully."
"#, namespace = namespace)
}

// ── serde_yaml transformers ───────────────────────────────────────────────────

/// Always overwrite metadata.namespace (used for Pipelines and builtins).
fn set_namespace(doc: &mut Value, namespace: &str) {
    doc["metadata"]["namespace"] = val(namespace);
}

/// Inject metadata.namespace only when the document does not already have one.
/// This preserves an explicit namespace written by the task author.
fn set_namespace_if_missing(doc: &mut Value, namespace: &str) {
    let already_set = doc["metadata"]["namespace"]
        .as_str()
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false);

    if !already_set {
        doc["metadata"]["namespace"] = val(namespace);
    }
}

fn inject_system_params(doc: &mut Value) {
    let declared = str_seq_names(&doc["spec"]["params"]);

    let existing: Vec<Value> = doc["spec"]["params"]
        .as_sequence()
        .cloned()
        .unwrap_or_default();

    let injected: Vec<Value> = SYSTEM_PARAMS
        .iter()
        .filter(|(name, _)| !declared.contains(&name.to_string()))
        .map(|(name, typ)| mapping(&[("name", val(name)), ("type", val(typ))]))
        .collect();

    doc["spec"]["params"] = Value::Sequence([existing, injected].concat());
}

fn inject_task_workspaces(doc: &mut Value) {
    let declared = str_seq_names(&doc["spec"]["workspaces"]);

    let existing: Vec<Value> = doc["spec"]["workspaces"]
        .as_sequence()
        .cloned()
        .unwrap_or_default();

    let injected: Vec<Value> = INJECTED_WORKSPACES
        .iter()
        .filter(|&&w| !declared.contains(&w.to_string()))
        .map(|&w| mapping(&[("name", val(w))]))
        .collect();

    doc["spec"]["workspaces"] = Value::Sequence([existing, injected].concat());
}

fn inject_pipeline_workspaces(doc: &mut Value) {
    // Inject into spec.workspaces
    let declared = str_seq_names(&doc["spec"]["workspaces"]);

    let existing: Vec<Value> = doc["spec"]["workspaces"]
        .as_sequence()
        .cloned()
        .unwrap_or_default();

    let injected: Vec<Value> = INJECTED_WORKSPACES
        .iter()
        .filter(|&&w| !declared.contains(&w.to_string()))
        .map(|&w| mapping(&[("name", val(w))]))
        .collect();

    doc["spec"]["workspaces"] = Value::Sequence([existing, injected].concat());

    // Inject workspace bindings into every task entry
    if let Some(tasks) = doc["spec"]["tasks"].as_sequence_mut() {
        for task in tasks.iter_mut() {
            inject_task_workspace_bindings(task);
        }
    }
}

/// Inject workspace bindings (name + workspace) into a single pipeline task entry.
fn inject_task_workspace_bindings(task: &mut Value) {
    let declared = str_seq_names(&task["workspaces"]);

    let existing: Vec<Value> = task["workspaces"]
        .as_sequence()
        .cloned()
        .unwrap_or_default();

    let injected: Vec<Value> = INJECTED_WORKSPACES
        .iter()
        .filter(|&&w| !declared.contains(&w.to_string()))
        .map(|&w| mapping(&[("name", val(w)), ("workspace", val(w))]))
        .collect();

    task["workspaces"] = Value::Sequence([existing, injected].concat());
}

/// Prepend init-credentials and clone tasks, fix runAfter on the first user task.
fn inject_builtin_pipeline_tasks(doc: &mut Value, gl_repo: &str) {
    let mut user_tasks: Vec<Value> = doc["spec"]["tasks"]
        .as_sequence()
        .cloned()
        .unwrap_or_default();

    // Set runAfter: [clone] on the first user task and inject its workspace bindings
    if let Some(first) = user_tasks.first_mut() {
        first["runAfter"] = Value::Sequence(vec![val("clone")]);
        inject_task_workspace_bindings(first);
    }
    // Inject workspace bindings on remaining user tasks too
    for task in user_tasks.iter_mut().skip(1) {
        inject_task_workspace_bindings(task);
    }

    let init_creds: Value = mapping(&[
        ("name", val("init-credentials")),
        ("taskRef", mapping(&[("name", val("init-credentials"))])),
        ("workspaces", Value::Sequence(vec![
            mapping(&[("name", val("creds")), ("workspace", val("creds"))]),
        ])),
    ]);

    let clone_task: Value = mapping(&[
        ("name", val("clone")),
        ("taskRef", mapping(&[("name", val("clone"))])),
        ("runAfter", Value::Sequence(vec![val("init-credentials")])),
        ("params", Value::Sequence(vec![
            mapping(&[("name", val("repo")), ("value", val(gl_repo))]),
        ])),
        ("workspaces", Value::Sequence(vec![
            mapping(&[("name", val("creds")),   ("workspace", val("creds"))]),
            mapping(&[("name", val("source")),  ("workspace", val("source"))]),
        ])),
    ]);

    let all_tasks: Vec<Value> = [vec![init_creds, clone_task], user_tasks].concat();
    doc["spec"]["tasks"] = Value::Sequence(all_tasks);
}

/// Inject GINGER_TOKEN secretKeyRef env into every step that doesn't have it.
fn inject_ginger_token_env(doc: &mut Value) {
    let steps = match doc["spec"]["steps"].as_sequence_mut() {
        Some(s) => s,
        None => return,
    };

    let token_env: Value = mapping(&[
        ("name", val("GINGER_TOKEN")),
        ("valueFrom", mapping(&[
            ("secretKeyRef", mapping(&[
                ("name", val("ginger-token-secret")),
                ("key",  val("token")),
            ])),
        ])),
    ]);

    for step in steps.iter_mut() {
        let already = step["env"]
            .as_sequence()
            .unwrap_or(&vec![])
            .iter()
            .any(|e| e["name"].as_str() == Some("GINGER_TOKEN"));

        if already {
            continue;
        }

        let mut env: Vec<Value> = step["env"]
            .as_sequence()
            .cloned()
            .unwrap_or_default();

        // Insert at position 0 so GINGER_TOKEN is always first
        env.insert(0, token_env.clone());
        step["env"] = Value::Sequence(env);
    }
}

// ── Utilities ─────────────────────────────────────────────────────────────────

/// Sanitize a string for use as a Kubernetes label value.
/// Labels must be alphanumeric + [-_.], ≤63 chars, start/end with alphanumeric.
pub fn sanitize_label(s: &str) -> String {
    let cleaned: String = s
        .chars()
        .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' || c == '.' { c } else { '-' })
        .collect();
    let trimmed = cleaned.trim_matches('-');
    trimmed[..63.min(trimmed.len())]
        .trim_matches('-')
        .to_string()
}

/// Parse YAML string into a serde_yaml Value.
fn parse(yaml: &str) -> Result<Value, String> {
    serde_yaml::from_str(yaml).map_err(|e| format!("YAML parse error: {}", e))
}

/// Serialize a serde_yaml Value back to a YAML string.
fn serialize(doc: &Value) -> Result<String, String> {
    serde_yaml::to_string(doc).map_err(|e| format!("YAML serialize error: {}", e))
}

/// Construct a serde_yaml Value::String.
fn val(s: &str) -> Value {
    Value::String(s.to_string())
}

/// Construct a serde_yaml Value::Mapping from a slice of (key, value) pairs.
fn mapping(pairs: &[(&str, Value)]) -> Value {
    let mut m = Mapping::new();
    for (k, v) in pairs {
        m.insert(val(k), v.clone());
    }
    Value::Mapping(m)
}

/// Collect the `name` strings from a sequence of `{name: ...}` mappings.
fn str_seq_names(seq: &Value) -> Vec<String> {
    seq.as_sequence()
        .unwrap_or(&vec![])
        .iter()
        .filter_map(|item| item["name"].as_str().map(|s| s.to_string()))
        .collect()
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    const TASK_YAML: &str = r#"
apiVersion: tekton.dev/v1beta1
kind: Task
metadata:
  name: build-and-push
spec:
  params:
    - name: image_tag
      type: string
  steps:
    - name: build
      image: gingersociety/tekton-task-buildah:latest
      script: |
        #!/bin/bash
        echo hello
"#;

    const TASK_YAML_WITH_NAMESPACE: &str = r#"
apiVersion: tekton.dev/v1beta1
kind: Task
metadata:
  name: build-and-push
  namespace: my-custom-namespace
spec:
  params:
    - name: image_tag
      type: string
  steps:
    - name: build
      image: gingersociety/tekton-task-buildah:latest
      script: |
        #!/bin/bash
        echo hello
"#;

    const PIPELINE_YAML: &str = r#"
apiVersion: tekton.dev/v1beta1
kind: Pipeline
metadata:
  name: build-pipeline
  annotations:
    x-gitter-enabled: "true"
    x-gitter-task-trigger-branch: '["refs/heads/main"]'
spec:
  params:
    - name: image_tag
      type: string
  tasks:
    - name: build-and-push
      taskRef:
        name: build-and-push
      params:
        - name: image_tag
          value: $(params.image_tag)
"#;

    #[test]
    fn test_task_namespace_injected_when_missing() {
        let out = transform_task(TASK_YAML, "tasks-my-repo", "deployment-target-main").unwrap();
        let doc: Value = serde_yaml::from_str(&out).unwrap();
        assert_eq!(doc["metadata"]["namespace"].as_str(), Some("tasks-my-repo"));
    }

    #[test]
    fn test_task_namespace_preserved_when_already_set() {
        let out = transform_task(TASK_YAML_WITH_NAMESPACE, "tasks-my-repo", "deployment-target-main").unwrap();
        let doc: Value = serde_yaml::from_str(&out).unwrap();
        // The author's explicit namespace must be kept, not overwritten.
        assert_eq!(doc["metadata"]["namespace"].as_str(), Some("my-custom-namespace"));
    }

    #[test]
    fn test_task_workspaces_injected() {
        let out = transform_task(TASK_YAML, "tasks-my-repo", "deployment-target-main").unwrap();
        let doc: Value = serde_yaml::from_str(&out).unwrap();
        let ws_names: Vec<&str> = doc["spec"]["workspaces"]
            .as_sequence().unwrap()
            .iter()
            .filter_map(|w| w["name"].as_str())
            .collect();
        for &ws in INJECTED_WORKSPACES {
            assert!(ws_names.contains(&ws), "missing workspace: {}", ws);
        }
    }

    #[test]
    fn test_task_workspaces_injected_even_when_namespace_preserved() {
        // Workspaces must still be injected regardless of whether namespace was preserved.
        let out = transform_task(TASK_YAML_WITH_NAMESPACE, "tasks-my-repo", "deployment-target-main").unwrap();
        let doc: Value = serde_yaml::from_str(&out).unwrap();
        let ws_names: Vec<&str> = doc["spec"]["workspaces"]
            .as_sequence().unwrap()
            .iter()
            .filter_map(|w| w["name"].as_str())
            .collect();
        for &ws in INJECTED_WORKSPACES {
            assert!(ws_names.contains(&ws), "missing workspace: {}", ws);
        }
    }

    #[test]
    fn test_task_ginger_token_injected() {
        let out = transform_task(TASK_YAML, "tasks-my-repo", "deployment-target-main").unwrap();
        let doc: Value = serde_yaml::from_str(&out).unwrap();
        let envs = &doc["spec"]["steps"][0]["env"];
        let names: Vec<&str> = envs.as_sequence().unwrap()
            .iter()
            .filter_map(|e| e["name"].as_str())
            .collect();
        assert!(names.contains(&"GINGER_TOKEN"), "GINGER_TOKEN not injected: {:?}", names);
    }

    #[test]
    fn test_task_deployment_placeholder_replaced() {
        let yaml = TASK_YAML.replace(
            "image: gingersociety/tekton-task-buildah:latest",
            "image: gingersociety/tekton-task-buildah:latest\n      env:\n        - name: KUBECONFIG\n          valueFrom:\n            secretKeyRef:\n              name: ginger-gitter/deployment-target\n              key: kubeconfig.yaml",
        );
        let out = transform_task(&yaml, "ns", "deployment-target-dev-alice").unwrap();
        assert!(out.contains("deployment-target-dev-alice"));
        assert!(!out.contains("ginger-gitter/deployment-target"));
    }

    #[test]
    fn test_pipeline_namespace_always_overwritten() {
        // Pipelines always get the derived namespace, even if one is set.
        let yaml_with_ns = PIPELINE_YAML.replace(
            "name: build-pipeline",
            "name: build-pipeline\n  namespace: some-other-namespace",
        );
        let out = transform_pipeline(&yaml_with_ns, "tasks-my-repo", "my-repo").unwrap();
        let doc: Value = serde_yaml::from_str(&out).unwrap();
        assert_eq!(doc["metadata"]["namespace"].as_str(), Some("tasks-my-repo"));
    }

    #[test]
    fn test_pipeline_namespace_injected() {
        let out = transform_pipeline(PIPELINE_YAML, "tasks-my-repo", "my-repo").unwrap();
        let doc: Value = serde_yaml::from_str(&out).unwrap();
        assert_eq!(doc["metadata"]["namespace"].as_str(), Some("tasks-my-repo"));
    }

    #[test]
    fn test_pipeline_system_params_injected() {
        let out = transform_pipeline(PIPELINE_YAML, "tasks-my-repo", "my-repo").unwrap();
        let doc: Value = serde_yaml::from_str(&out).unwrap();
        let param_names: Vec<&str> = doc["spec"]["params"]
            .as_sequence().unwrap()
            .iter()
            .filter_map(|p| p["name"].as_str())
            .collect();
        // image_tag declared by user, system params injected, no duplicates
        assert!(param_names.contains(&"image_tag"));
        assert!(param_names.contains(&"gl_user"));
        assert!(param_names.contains(&"gl_repo"));
        assert_eq!(param_names.iter().filter(|&&n| n == "image_tag").count(), 1,
            "image_tag duplicated");
    }

    #[test]
    fn test_pipeline_builtin_tasks_prepended() {
        let out = transform_pipeline(PIPELINE_YAML, "tasks-my-repo", "my-repo").unwrap();
        let doc: Value = serde_yaml::from_str(&out).unwrap();
        let tasks = doc["spec"]["tasks"].as_sequence().unwrap();
        assert_eq!(tasks[0]["name"].as_str(), Some("init-credentials"));
        assert_eq!(tasks[1]["name"].as_str(), Some("clone"));
        assert_eq!(tasks[2]["name"].as_str(), Some("build-and-push"));
    }

    #[test]
    fn test_pipeline_first_user_task_run_after_clone() {
        let out = transform_pipeline(PIPELINE_YAML, "tasks-my-repo", "my-repo").unwrap();
        let doc: Value = serde_yaml::from_str(&out).unwrap();
        let tasks = doc["spec"]["tasks"].as_sequence().unwrap();
        let run_after = tasks[2]["runAfter"].as_sequence().unwrap();
        assert_eq!(run_after[0].as_str(), Some("clone"));
    }

    #[test]
    fn test_sanitize_label() {
        assert_eq!(sanitize_label("refs/heads/dev-alice"), "refs-heads-dev-alice");
        assert_eq!(sanitize_label("dev-alice"), "dev-alice");
        assert_eq!(sanitize_label("-bad-"), "bad");
        assert_eq!(sanitize_label(&"a".repeat(100)), "a".repeat(63));
    }

    #[test]
    fn test_build_pipeline_run_user_params_json_safe() {
        let mut user_params = HashMap::new();
        user_params.insert(
            "vault".to_string(),
            r#"{"JWT_SECRET_KEY":"1234","AWS_ACCESS_KEY_ID":"AKIAYS2NQ4JLIY3WFIVE"}"#.to_string(),
        );
        user_params.insert(
            "values".to_string(),
            r#"{"HOSTING_FQDN":"$HOSTING_FQDN","shared":{"DATABASE_PASSWORD":"shell(kubectl get secret pg-postgresql -o jsonpath='{.data.postgres-password}' | base64 -d)"}}"#.to_string(),
        );

        let yaml = build_pipeline_run(
            "debug-pipeline",
            "tasks-ginger-society-iac",
            &user_params,
            "alice",
            "ginger-society/ginger-society-iac",
            "refs/heads/main",
            "abc12345def67890",
        );

        // Must parse as valid YAML without error
        let parsed: serde_yaml::Value = serde_yaml::from_str(&yaml)
            .expect("build_pipeline_run produced invalid YAML");

        // Find vault param and verify value round-trips correctly
        let params = parsed["spec"]["params"].as_sequence().unwrap();
        let vault_param = params.iter().find(|p| p["name"].as_str() == Some("vault")).unwrap();
        let vault_val = vault_param["value"].as_str().unwrap().trim();
        assert!(vault_val.contains("JWT_SECRET_KEY"), "vault value not preserved: {}", vault_val);
        assert!(vault_val.contains("AKIAYS2NQ4JLIY3WFIVE"), "vault value not preserved: {}", vault_val);
    }
}