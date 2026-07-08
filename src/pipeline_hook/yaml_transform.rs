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
    ("repo",       "string"),
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
    // This is safe for any kind — metadata.namespace is a universal field.
    set_namespace_if_missing(&mut doc, namespace);

    // spec.workspaces and spec.steps are specific to a real Tekton `Task`
    // object and are NOT part of the RemoteTask CRD schema (which only
    // declares capability/script/cleanup/env). Injecting them into a
    // RemoteTask doc makes the API server's server-side-apply typed-patch
    // computation fail with "field not declared in schema", since it can't
    // represent an undeclared field. A standalone RemoteTask gets its
    // workspace/credential wiring from the CustomRun's `creds` workspace
    // binding instead (see customrun.rs), so it needs neither injection.
    let kind = doc["kind"].as_str().unwrap_or("");
    if kind == "Task" {
        inject_task_workspaces(&mut doc);
        inject_ginger_token_env(&mut doc);
    } else {
        println!(
            "[ginger-gitter] '{}' — skipping Task-specific workspace/env injection (kind: {})",
            namespace, kind
        );
    }

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
) -> Result<String, String> {
    let mut doc = parse(yaml)?;

    // Pipelines always get the derived namespace — no conditional here.
    set_namespace(&mut doc, namespace);
    inject_system_params(&mut doc);
    inject_pipeline_workspaces(&mut doc);
    inject_builtin_pipeline_tasks(&mut doc);

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
        ("repo",       gl_repo),
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
fn inject_builtin_pipeline_tasks(doc: &mut Value) {
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
            mapping(&[
                ("name", val("repo")),
                ("value", val("$(params.repo)")),   // was: val(gl_repo)
            ]),
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
