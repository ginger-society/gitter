use std::collections::HashMap;

/// The fixed set of workspaces every task/pipeline gets.
const INJECTED_WORKSPACES: &[&str] = &["creds", "source", "general-purpose-cache", "buildah-cache"];

/// Built-in tasks prepended to every pipeline before the user's tasks.
const BUILTIN_TASKS: &[(&str, &str)] = &[
    ("init-credentials", "init-credentials"),
    ("clone", "clone"),
];

// ── Public entry points ───────────────────────────────────────────────────────

/// Transform a user-written Task YAML: inject namespace, workspaces, GINGER_TOKEN env.
pub fn transform_task(yaml: &str, namespace: &str) -> Result<String, String> {
    let yaml = inject_namespace(yaml, namespace)?;
    let yaml = inject_task_workspaces(&yaml)?;
    let yaml = inject_ginger_token_env(&yaml)?;
    Ok(yaml)
}

/// Transform a user-written Pipeline YAML: inject namespace, workspaces, prepend
/// init-credentials → clone tasks, fix runAfter on the first user task.
/// System params injected into every Pipeline's spec.params.
/// Must match exactly what build_pipeline_run writes into the PipelineRun params.
const SYSTEM_PARAMS: &[(&str, &str)] = &[
    ("gl_user",    "string"),
    ("gl_repo",    "string"),
    ("gl_refname", "string"),
    ("gl_new_rev", "string"),
    ("image_tag",  "string"),
];

pub fn transform_pipeline(yaml: &str, namespace: &str, gl_repo: &str) -> Result<String, String> {
    let yaml = inject_namespace(yaml, namespace)?;
    let yaml = inject_system_params(&yaml)?;
    let yaml = inject_pipeline_workspaces(&yaml)?;
    let yaml = inject_builtin_pipeline_tasks(&yaml, gl_repo)?;
    Ok(yaml)
}

/// Build a complete PipelineRun YAML for a triggered pipeline.
pub fn build_pipeline_run(
    pipeline_name: &str,
    namespace: &str,
    user_params: &HashMap<String, String>,
    gl_user: &str,
    gl_repo: &str,
    gl_refname: &str,
    gl_new_rev: &str,
) -> String {
    // image_tag = branch name (e.g. "dev-vriksh-feat1" or "main").
    // This is the system-managed tag: the developer supplies only the image
    // name in their task; ginger-gitter provides the tag here.
    let image_tag = gl_refname
        .strip_prefix("refs/heads/")
        .unwrap_or(gl_refname)
        .to_string();

    let mut params_yaml = String::new();

    // System params always injected — these feed $(params.image_tag) etc.
    for (k, v) in &[
        ("gl_user",   gl_user),
        ("gl_repo",   gl_repo),
        ("gl_refname", gl_refname),
        ("gl_new_rev", gl_new_rev),
        ("image_tag",  image_tag.as_str()),
    ] {
        params_yaml.push_str(&format!("    - name: {}\n      value: \"{}\"\n", k, v));
    }

    // Any additional caller-supplied params (currently none, reserved for future use)
    for (k, v) in user_params {
        params_yaml.push_str(&format!("    - name: {}\n      value: \"{}\"\n", k, v));
    }

    // Labels must match (([A-Za-z0-9][-A-Za-z0-9_.]*)?[A-Za-z0-9])? and be <=63 chars.
    // Strip refs/heads/ prefix then replace any remaining illegal chars with '-'.
    let ref_label  = sanitize_label(
        gl_refname.strip_prefix("refs/heads/").unwrap_or(gl_refname)
    );
    let repo_label = sanitize_label(gl_repo);
    let sha_label  = &gl_new_rev[..8.min(gl_new_rev.len())];

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

/// Sanitize a string for use as a Kubernetes label value.
/// Replaces any character that is not alphanumeric, '-', '_', or '.' with '-',
/// then trims leading/trailing '-' and truncates to 63 characters.
fn sanitize_label(s: &str) -> String {
    let cleaned: String = s
        .chars()
        .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' || c == '.' { c } else { '-' })
        .collect();
    let trimmed = cleaned.trim_matches('-');
    // Truncate to 63 chars, trim any trailing '-' that truncation may expose
    let truncated = &trimmed[..63.min(trimmed.len())];
    truncated.trim_matches('-').to_string()
}

/// Build the fixed init-credentials Task YAML for a namespace.
pub fn builtin_init_credentials_task(namespace: &str) -> String {
    format!(
        r#"apiVersion: tekton.dev/v1beta1
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
"#,
        namespace = namespace
    )
}

/// Build the fixed clone Task YAML for a namespace.
pub fn builtin_clone_task(namespace: &str) -> String {
    format!(
        r#"apiVersion: tekton.dev/v1beta1
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
"#,
        namespace = namespace
    )
}

// ── Internal helpers ──────────────────────────────────────────────────────────

/// Inject system params into spec.params, skipping any already declared by the user.
fn inject_system_params(yaml: &str) -> Result<String, String> {
    // Collect param names already declared by the user
    let mut declared: Vec<String> = Vec::new();
    let mut in_spec_params = false;

    for line in yaml.lines() {
        let trimmed = line.trim();
        let indent = line.len() - line.trim_start().len();

        if trimmed == "params:" && indent == 2 {
            in_spec_params = true;
            continue;
        }
        if in_spec_params {
            // Exit on next sibling key at indent 2
            if indent <= 2 && !trimmed.is_empty() && !trimmed.starts_with('-') {
                break;
            }
            if let Some(rest) = trimmed.strip_prefix("- name:") {
                declared.push(rest.trim().to_string());
            }
        }
    }

    let to_add: Vec<(&str, &str)> = SYSTEM_PARAMS
        .iter()
        .copied()
        .filter(|(name, _)| !declared.iter().any(|d| d == name))
        .collect();

    if to_add.is_empty() {
        return Ok(yaml.to_string());
    }

    let new_params = to_add
        .iter()
        .map(|(name, typ)| format!("    - name: {}
      type: {}", name, typ))
        .collect::<Vec<_>>()
        .join("
");

    // Append into existing params: block, or insert before tasks:
    inject_into_spec_block(yaml, "params:", &new_params, "tasks:")
}

/// Inject or replace `namespace:` under `metadata:`.
fn inject_namespace(yaml: &str, namespace: &str) -> Result<String, String> {
    let mut lines: Vec<String> = yaml.lines().map(|l| l.to_string()).collect();
    let mut in_metadata = false;
    let mut namespace_injected = false;
    let mut metadata_index: Option<usize> = None;

    for (i, line) in lines.iter().enumerate() {
        let trimmed = line.trim();
        if trimmed == "metadata:" {
            in_metadata = true;
            metadata_index = Some(i);
            continue;
        }
        if in_metadata {
            // leaving metadata block (next top-level key)
            if !line.starts_with(' ') && !line.starts_with('\t') && !trimmed.is_empty() {
                break;
            }
            if trimmed.starts_with("namespace:") {
                // Replace existing
                let indent = leading_spaces(line);
                lines[i] = format!("{}namespace: {}", indent, namespace);
                namespace_injected = true;
                break;
            }
        }
    }

    if !namespace_injected {
        if let Some(mi) = metadata_index {
            // Insert after metadata: line, finding the right indent from next line
            let indent = if mi + 1 < lines.len() {
                leading_spaces(&lines[mi + 1])
            } else {
                "  ".to_string()
            };
            lines.insert(mi + 1, format!("{}namespace: {}", indent, namespace));
        } else {
            return Err("No metadata: block found in YAML".to_string());
        }
    }

    Ok(lines.join("\n"))
}

/// Inject `workspaces:` list into a Task spec if not already present.
fn inject_task_workspaces(yaml: &str) -> Result<String, String> {
    // Collect which workspaces already declared
    let existing = collect_declared_workspaces(yaml);
    let to_add: Vec<&str> = INJECTED_WORKSPACES
        .iter()
        .copied()
        .filter(|&w| !existing.iter().any(|e| e == w))
        .collect();

    if to_add.is_empty() {
        return Ok(yaml.to_string());
    }

    let ws_yaml = to_add
        .iter()
        .map(|w| format!("    - name: {}", w))
        .collect::<Vec<_>>()
        .join("\n");

    // Find existing workspaces: block and append, or inject before steps:
    inject_into_spec_block(yaml, "workspaces:", &ws_yaml, "steps:")
}

/// Inject `workspaces:` list into a Pipeline spec if not already present,
/// and ensure workspace entries in tasks reference them.
fn inject_pipeline_workspaces(yaml: &str) -> Result<String, String> {
    let existing = collect_declared_spec_workspaces(yaml);
    let to_add: Vec<&str> = INJECTED_WORKSPACES
        .iter()
        .copied()
        .filter(|&w| !existing.iter().any(|e| e == w))
        .collect();

    let mut result = yaml.to_string();

    if !to_add.is_empty() {
        let ws_yaml = to_add
            .iter()
            .map(|w| format!("    - name: {}", w))
            .collect::<Vec<_>>()
            .join("\n");
        result = inject_into_spec_block(&result, "workspaces:", &ws_yaml, "tasks:")?;
    }

    // Now inject workspace bindings into each task entry in the pipeline
    result = inject_pipeline_task_workspace_bindings(&result)?;

    Ok(result)
}

/// For each `- name: <task>` block inside `tasks:` of a Pipeline, add workspace
/// bindings if not already present.
fn inject_pipeline_task_workspace_bindings(yaml: &str) -> Result<String, String> {
    let lines: Vec<&str> = yaml.lines().collect();
    let mut result: Vec<String> = Vec::new();
    let mut in_pipeline_tasks = false;
    let mut in_task_entry = false;
    let mut task_entry_indent = 0usize;
    let mut task_has_workspaces = false;
    let mut pending_workspace_injection: Option<(usize, String)> = None; // (indent, yaml)

    let ws_names: Vec<&str> = INJECTED_WORKSPACES.to_vec();

    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        let trimmed = line.trim();
        let indent = line.len() - line.trim_start().len();

        // Detect `  tasks:` at spec level (indent 2)
        if trimmed == "tasks:" && indent == 2 {
            in_pipeline_tasks = true;
            result.push(line.to_string());
            i += 1;
            continue;
        }

        if in_pipeline_tasks {
            // Top-level key exits pipeline tasks
            if !line.starts_with(' ') && !line.starts_with('\t') && !trimmed.is_empty() {
                // flush any pending injection first
                if let Some((ind, ws_block)) = pending_workspace_injection.take() {
                    if !task_has_workspaces {
                        result.push(ws_block);
                    }
                }
                in_pipeline_tasks = false;
                in_task_entry = false;
                result.push(line.to_string());
                i += 1;
                continue;
            }

            // New task entry: `    - name:`
            if trimmed.starts_with("- name:") && indent == 4 {
                // Flush previous task's pending injection
                if in_task_entry {
                    if let Some((_ind, ws_block)) = pending_workspace_injection.take() {
                        if !task_has_workspaces {
                            result.push(ws_block);
                        }
                    }
                }
                in_task_entry = true;
                task_entry_indent = indent;
                task_has_workspaces = false;

                // Build workspace binding block for this task
                let ws_binding = ws_names
                    .iter()
                    .map(|w| {
                        format!(
                            "      - name: {}\n        workspace: {}",
                            w, w
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                let ws_block = format!("      workspaces:\n{}", ws_binding);
                pending_workspace_injection = Some((indent, ws_block));

                result.push(line.to_string());
                i += 1;
                continue;
            }

            if in_task_entry {
                // Detect if workspaces: already declared for this task
                if trimmed == "workspaces:" && indent == 6 {
                    task_has_workspaces = true;
                    pending_workspace_injection = None;
                }
                // Detect runAfter:, params:, taskRef: — inject workspaces before them if not done
                if (trimmed.starts_with("runAfter:")
                    || trimmed.starts_with("params:")
                    || trimmed.starts_with("taskRef:"))
                    && indent == 6
                    && !task_has_workspaces
                {
                    if let Some((_ind, ws_block)) = pending_workspace_injection.take() {
                        result.push(ws_block);
                        task_has_workspaces = true;
                    }
                }
            }
        }

        result.push(line.to_string());
        i += 1;
    }

    // End of file flush
    if in_pipeline_tasks {
        if let Some((_ind, ws_block)) = pending_workspace_injection.take() {
            if !task_has_workspaces {
                result.push(ws_block);
            }
        }
    }

    Ok(result.join("\n"))
}

/// Prepend init-credentials and clone tasks to the pipeline's tasks list,
/// and fix the first user task's runAfter to point to clone.
fn inject_builtin_pipeline_tasks(yaml: &str, gl_repo: &str) -> Result<String, String> {
    let lines: Vec<&str> = yaml.lines().collect();
    let mut result: Vec<String> = Vec::new();
    let mut tasks_block_start: Option<usize> = None;
    let mut first_user_task_dash: Option<usize> = None;

    // Find `  tasks:` and the first `    - name:` inside it
    let mut in_tasks = false;
    for (i, line) in lines.iter().enumerate() {
        let trimmed = line.trim();
        let indent = line.len() - line.trim_start().len();

        if trimmed == "tasks:" && indent == 2 {
            in_tasks = true;
            tasks_block_start = Some(i);
            continue;
        }
        if in_tasks {
            if !line.starts_with(' ') && !line.starts_with('\t') && !trimmed.is_empty() {
                break;
            }
            if trimmed.starts_with("- name:") && indent == 4 && first_user_task_dash.is_none() {
                first_user_task_dash = Some(i);
                break;
            }
        }
    }

    let tasks_line = tasks_block_start.ok_or("No tasks: block found in pipeline YAML")?;
    let first_user_line = first_user_task_dash.ok_or("No task entries found in pipeline YAML")?;

    // Determine the name of the first user task for runAfter injection
    let first_user_task_name = lines[first_user_line]
        .trim()
        .strip_prefix("- name:")
        .map(|s| s.trim().to_string())
        .unwrap_or_default();

    // Build builtin task YAML blocks
    let builtin_block = format!(
        r#"    - name: init-credentials
      taskRef:
        name: init-credentials
      workspaces:
        - name: creds
          workspace: creds

    - name: clone
      taskRef:
        name: clone
      runAfter:
        - init-credentials
      params:
        - name: repo
          value: "{gl_repo}"
      workspaces:
        - name: creds
          workspace: creds
        - name: source
          workspace: source

"#,
        gl_repo = gl_repo
    );

    // Reconstruct: output lines up to and including `tasks:`, then inject builtins,
    // then the rest — but fix the first user task's runAfter.
    for (i, line) in lines.iter().enumerate() {
        if i == tasks_line {
            result.push(line.to_string());
            result.push(builtin_block.clone());
            continue;
        }

        // On the first user task, ensure runAfter: [clone] is set
        if i == first_user_line {
            result.push(line.to_string());
            // peek ahead to see if runAfter already follows
            let next_lines: Vec<&str> = lines[i + 1..].iter().copied().take(5).collect();
            let has_run_after = next_lines.iter().any(|l| l.trim().starts_with("runAfter:"));
            if !has_run_after {
                result.push("      runAfter:".to_string());
                result.push("        - clone".to_string());
            }
            continue;
        }

        // Remove any existing runAfter that points somewhere other than clone
        // for the first user task (replace it)
        if i > first_user_line {
            let trimmed = line.trim();
            let indent = line.len() - line.trim_start().len();
            // Only within the first user task block (indent >= 6)
            if indent == 8 && trimmed.starts_with("- ") {
                // This is a runAfter value for the first task — check context
                // We rely on the replacement above; skip old runAfter entries
                // only if they immediately follow a runAfter: we just replaced
            }
        }

        result.push(line.to_string());
    }

    Ok(result.join("\n"))
}

/// Inject `GINGER_TOKEN` env into every `steps:` block of a Task.
fn inject_ginger_token_env(yaml: &str) -> Result<String, String> {
    // Check if already present
    if yaml.contains("GINGER_TOKEN") {
        return Ok(yaml.to_string());
    }

    let lines: Vec<&str> = yaml.lines().collect();
    let mut result: Vec<String> = Vec::new();
    let mut in_steps = false;
    let mut in_step_entry = false;
    let mut step_env_injected = false;
    let mut pending_inject_before_script = false;

    for (i, line) in lines.iter().enumerate() {
        let trimmed = line.trim();
        let indent = line.len() - line.trim_start().len();

        if trimmed == "steps:" && indent == 2 {
            in_steps = true;
            result.push(line.to_string());
            continue;
        }

        if in_steps {
            if !line.starts_with(' ') && !line.starts_with('\t') && !trimmed.is_empty() {
                in_steps = false;
                in_step_entry = false;
                result.push(line.to_string());
                continue;
            }

            // New step entry
            if trimmed.starts_with("- name:") && indent == 4 {
                // Flush pending for previous step
                in_step_entry = true;
                step_env_injected = false;
                pending_inject_before_script = false;
                result.push(line.to_string());
                continue;
            }

            if in_step_entry {
                if trimmed == "env:" && indent == 6 {
                    step_env_injected = true;
                    result.push(line.to_string());
                    // Inject GINGER_TOKEN as the first env entry
                    result.push(format!(
                        "        - name: GINGER_TOKEN\n          valueFrom:\n            secretKeyRef:\n              name: ginger-token-secret\n              key: token"
                    ));
                    continue;
                }
                // Inject env block before script: or command:
                if (trimmed.starts_with("script:") || trimmed.starts_with("command:"))
                    && indent == 6
                    && !step_env_injected
                {
                    result.push(format!(
                        "      env:\n        - name: GINGER_TOKEN\n          valueFrom:\n            secretKeyRef:\n              name: ginger-token-secret\n              key: token"
                    ));
                    step_env_injected = true;
                }
            }
        }

        result.push(line.to_string());
    }

    Ok(result.join("\n"))
}

// ── Utility helpers ───────────────────────────────────────────────────────────

fn leading_spaces(line: &str) -> String {
    let count = line.len() - line.trim_start().len();
    " ".repeat(count)
}

/// Collect workspace names already declared in a Task's `spec.workspaces:` block.
fn collect_declared_workspaces(yaml: &str) -> Vec<String> {
    let mut names = Vec::new();
    let mut in_workspaces = false;

    for line in yaml.lines() {
        let trimmed = line.trim();
        let indent = line.len() - line.trim_start().len();

        if trimmed == "workspaces:" && indent == 2 {
            in_workspaces = true;
            continue;
        }
        if in_workspaces {
            if !line.starts_with(' ') && !line.starts_with('\t') && !trimmed.is_empty() {
                break;
            }
            if indent <= 2 && !trimmed.is_empty() && !trimmed.starts_with('-') {
                break;
            }
            if let Some(rest) = trimmed.strip_prefix("- name:") {
                names.push(rest.trim().to_string());
            }
        }
    }
    names
}

/// Collect workspace names declared in a Pipeline's `spec.workspaces:` block
/// (indent=2 level, not inside tasks).
fn collect_declared_spec_workspaces(yaml: &str) -> Vec<String> {
    let mut names = Vec::new();
    let mut in_workspaces = false;

    for line in yaml.lines() {
        let trimmed = line.trim();
        let indent = line.len() - line.trim_start().len();

        // spec-level workspaces: at indent 2
        if trimmed == "workspaces:" && indent == 2 {
            in_workspaces = true;
            continue;
        }
        if in_workspaces {
            // Exit on next sibling key at indent 2
            if indent <= 2 && !trimmed.is_empty() && !trimmed.starts_with('-') {
                break;
            }
            if let Some(rest) = trimmed.strip_prefix("- name:") {
                names.push(rest.trim().to_string());
            }
        }
    }
    names
}

/// Inject lines into an existing `<block_key>:` section, or insert the section
/// before `<insert_before>:` if not present.
fn inject_into_spec_block(
    yaml: &str,
    block_key: &str,    // e.g. "workspaces:"
    new_lines: &str,
    insert_before: &str, // e.g. "steps:" or "tasks:"
) -> Result<String, String> {
    let lines: Vec<&str> = yaml.lines().collect();
    let mut result: Vec<String> = Vec::new();
    let mut found_block = false;
    let mut injected = false;

    let block_key_trimmed = block_key.trim_end_matches(':');
    let insert_before_trimmed = insert_before.trim_end_matches(':');

    for line in &lines {
        let trimmed = line.trim();
        let indent = line.len() - line.trim_start().len();

        if trimmed == block_key && indent == 2 {
            found_block = true;
            result.push(line.to_string());
            continue;
        }

        if found_block && !injected {
            // End of existing block — append new entries before the next sibling
            if indent <= 2 && !trimmed.is_empty() && !trimmed.starts_with('-') {
                result.push(new_lines.to_string());
                injected = true;
                found_block = false;
            }
        }

        // Insert entire block before insert_before: if block not found yet
        if !found_block && !injected && trimmed == insert_before && indent == 2 {
            result.push(format!("  {}:", block_key_trimmed));
            result.push(new_lines.to_string());
            injected = true;
        }

        result.push(line.to_string());
    }

    // Append if block was last thing in file
    if found_block && !injected {
        result.push(new_lines.to_string());
    }

    Ok(result.join("\n"))
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_inject_namespace() {
        let yaml = "apiVersion: tekton.dev/v1beta1\nkind: Task\nmetadata:\n  name: foo\nspec:\n  steps: []\n";
        let out = inject_namespace(yaml, "my-ns").unwrap();
        assert!(out.contains("namespace: my-ns"), "got: {}", out);
    }

    #[test]
    fn test_inject_namespace_replaces_existing() {
        let yaml = "apiVersion: tekton.dev/v1beta1\nkind: Task\nmetadata:\n  name: foo\n  namespace: old-ns\nspec:\n  steps: []\n";
        let out = inject_namespace(yaml, "new-ns").unwrap();
        assert!(out.contains("namespace: new-ns"));
        assert!(!out.contains("namespace: old-ns"));
    }

    #[test]
    fn test_ginger_token_injected() {
        let yaml = "apiVersion: tekton.dev/v1beta1\nkind: Task\nmetadata:\n  name: t\nspec:\n  steps:\n    - name: build\n      image: foo\n      script: |\n        echo hi\n";
        let out = inject_ginger_token_env(yaml).unwrap();
        assert!(out.contains("GINGER_TOKEN"), "got: {}", out);
    }
}