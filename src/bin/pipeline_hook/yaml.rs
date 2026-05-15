use std::collections::HashMap;

use crate::pipeline_hook::glob::glob_matches;
use crate::pipeline_hook::types::PipelineDefinition;

/// Extract all annotation key-value pairs from a YAML string.
pub fn extract_annotations(yaml: &str) -> HashMap<String, String> {
    let mut annotations = HashMap::new();
    let mut in_annotations = false;

    for line in yaml.lines() {
        if line.trim() == "annotations:" {
            in_annotations = true;
            continue;
        }
        if in_annotations {
            if !line.starts_with("    ") && !line.starts_with('\t') {
                in_annotations = false;
                continue;
            }
            let trimmed = line.trim();
            if let Some((key, value)) = trimmed.split_once(':') {
                let key = key.trim().to_string();
                let value = value.trim().trim_matches('\'').trim_matches('"').to_string();
                if !key.is_empty() {
                    annotations.insert(key, value);
                }
            }
        }
    }

    annotations
}

/// Extract a simple scalar field (name, namespace) from YAML by key.
pub fn extract_yaml_field(yaml: &str, field: &str) -> Option<String> {
    for line in yaml.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix(&format!("{}:", field)) {
            let value = rest.trim().trim_matches('"').trim_matches('\'').to_string();
            if !value.is_empty() {
                return Some(value);
            }
        }
    }
    None
}

/// Parse a JSON array of strings into a Vec<String>.
pub fn parse_json_string_array(input: &str) -> Vec<String> {
    let trimmed = input.trim().trim_start_matches('[').trim_end_matches(']');
    if trimmed.is_empty() {
        return vec![];
    }
    trimmed
        .split(',')
        .map(|s| s.trim().trim_matches('"').trim_matches('\'').to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Parse a pipeline YAML string into a PipelineDefinition.
pub fn parse_pipeline_yaml(yaml: &str, source_file: &str) -> Result<PipelineDefinition, String> {
    let annotations = extract_annotations(yaml);

    let enabled = annotations
        .get("x-gitter-enabled")
        .map(|v| v == "true")
        .unwrap_or(false);

    let trigger_branches = parse_json_string_array(
        annotations.get("x-gitter-trigger-branch").map(|s| s.as_str()).unwrap_or("[]"),
    );
    let path_filter = parse_json_string_array(
        annotations.get("x-gitter-path-filter").map(|s| s.as_str()).unwrap_or("[]"),
    );
    let ignore_paths = parse_json_string_array(
        annotations.get("x-gitter-ignore-paths").map(|s| s.as_str()).unwrap_or("[]"),
    );
    let concurrency = annotations
        .get("x-gitter-concurrency")
        .cloned()
        .unwrap_or_else(|| "replace".to_string());

    let name = extract_yaml_field(yaml, "name")
        .ok_or_else(|| format!("missing metadata.name in {}", source_file))?;
    let namespace = extract_yaml_field(yaml, "namespace").unwrap_or_default();
    let pipeline_name = annotations
        .get("x-gitter-pipeline-name")
        .cloned()
        .unwrap_or_else(|| name.clone());

    Ok(PipelineDefinition {
        source_file: source_file.to_string(),
        name,
        namespace,
        pipeline_name,
        enabled,
        trigger_branches,
        path_filter,
        ignore_paths,
        concurrency,
    })
}

/// Decide whether a pipeline should trigger for this push.
pub fn should_trigger(
    pipeline: &PipelineDefinition,
    refname: &str,
    changed_files: &[String],
) -> bool {
    if !pipeline.enabled {
        println!("[ginger-gitter]   {} — skipped (x-gitter-enabled is false)", pipeline.name);
        return false;
    }
    if !pipeline.trigger_branches.is_empty() {
        let branch_matched = pipeline.trigger_branches.iter().any(|pat| glob_matches(pat, refname));
        if !branch_matched {
            println!(
                "[ginger-gitter]   {} — skipped (branch '{}' not in trigger list)",
                pipeline.name, refname
            );
            return false;
        }
    }
    if !pipeline.ignore_paths.is_empty() {
        let all_ignored = changed_files
            .iter()
            .all(|f| pipeline.ignore_paths.iter().any(|pat| glob_matches(pat, f)));
        if all_ignored && !changed_files.is_empty() {
            println!(
                "[ginger-gitter]   {} — skipped (all changed files matched ignore-paths)",
                pipeline.name
            );
            return false;
        }
    }
    if !pipeline.path_filter.is_empty() {
        let any_matched = changed_files
            .iter()
            .any(|f| pipeline.path_filter.iter().any(|pat| glob_matches(pat, f)));
        if !any_matched {
            println!(
                "[ginger-gitter]   {} — skipped (no changed files matched path-filter)",
                pipeline.name
            );
            return false;
        }
    }
    true
}