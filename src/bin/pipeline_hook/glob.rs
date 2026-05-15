/// Simple glob matcher supporting `*` (any segment) and `**` (any path).
pub fn glob_matches(pattern: &str, value: &str) -> bool {
    if pattern == "*" || pattern == "**" {
        return true;
    }
    if let Some(prefix) = pattern.strip_suffix("/**") {
        return value.starts_with(prefix);
    }
    if let Some(prefix) = pattern.strip_suffix("/*") {
        return value.starts_with(prefix);
    }
    if let Some(suffix) = pattern.strip_prefix("**/") {
        return value.ends_with(suffix) || value.contains(&format!("/{}", suffix));
    }
    if pattern.contains('*') {
        let parts: Vec<&str> = pattern.splitn(2, '*').collect();
        return value.starts_with(parts[0]) && value.ends_with(parts[1]);
    }
    pattern == value
}