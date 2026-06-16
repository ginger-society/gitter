/// Permission registry and gitolite.conf generator.
///
/// Storage layout (inside the gitolite-admin repo):
///
///   permissions/
///     <workspace>/
///       users     — newline-delimited usernames  (e.g. "vriksh")
///       groups    — newline-delimited group UUIDs (e.g. "435a8c5a-...")
///
///   pair-programming/
///     <workspace>/
///       <repo>/
///         <branch-prefix>  — newline-delimited usernames (no groups)
///
/// Generated gitolite.conf rules:
///
///   admin_key
///     RW+ on every repo (all branches) — backup + admin access
///
///   users  (per workspace)
///     R      on  <workspace>-*           (read all projects in workspace)
///     RW+    on  refs/heads/dev-<user>-* (personal dev branches only)
///
///   groups (per workspace)
///     RW+    on  <workspace>-*           (full access to all branches)
///
///   pair-programming slots (per workspace/repo/branch-prefix)
///     RW+    on  refs/heads/<branch-prefix>-*  =  <name_a> <name_b> …
///
///   Fixed:
///     repo gitolite-admin  RW+ = admin_key
///     repo testing         RW+ = @all

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::{Context, Result};
use tracing::{debug, info};

use crate::handler_pair_programming::load_pair_programming_slots;

// ── Types ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MemberType {
    User,
    Group,
}

impl MemberType {
    pub fn as_str(&self) -> &'static str {
        match self {
            MemberType::User  => "users",
            MemberType::Group => "groups",
        }
    }
}

/// Everything we know about a single workspace's membership.
#[derive(Debug, Default, Clone)]
pub struct WorkspaceMembers {
    pub users:  BTreeSet<String>,
    pub groups: BTreeSet<String>,
}

// ── Read / write membership files ────────────────────────────────────────────

fn workspace_dir(repo_root: &Path, workspace: &str) -> std::path::PathBuf {
    repo_root.join("permissions").join(workspace)
}

fn member_file(repo_root: &Path, workspace: &str, kind: &MemberType) -> std::path::PathBuf {
    workspace_dir(repo_root, workspace).join(kind.as_str())
}

async fn read_member_file(path: &Path) -> Result<BTreeSet<String>> {
    match tokio::fs::read_to_string(path).await {
        Ok(content) => Ok(content
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(String::from)
            .collect()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(BTreeSet::new()),
        Err(e) => Err(e).context(format!("read {}", path.display())),
    }
}

async fn write_member_file(path: &Path, members: &BTreeSet<String>) -> Result<()> {
    tokio::fs::create_dir_all(path.parent().unwrap()).await?;
    let content = members
        .iter()
        .map(|s| s.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    // Always end with a trailing newline
    let content = if content.is_empty() {
        String::new()
    } else {
        format!("{content}\n")
    };
    tokio::fs::write(path, content)
        .await
        .context(format!("write {}", path.display()))
}

// ── Public API ────────────────────────────────────────────────────────────────

/// Load the current members for a workspace from disk.
pub async fn load_workspace(repo_root: &Path, workspace: &str) -> Result<WorkspaceMembers> {
    let users  = read_member_file(&member_file(repo_root, workspace, &MemberType::User)).await?;
    let groups = read_member_file(&member_file(repo_root, workspace, &MemberType::Group)).await?;
    debug!(
        "[permissions] loaded workspace '{}': {} users, {} groups",
        workspace,
        users.len(),
        groups.len()
    );
    Ok(WorkspaceMembers { users, groups })
}

/// Add a member to a workspace and persist to disk.
/// Returns true if the member was newly added (false = already present).
pub async fn add_member(
    repo_root: &Path,
    workspace: &str,
    kind: &MemberType,
    name: &str,
) -> Result<bool> {
    let path = member_file(repo_root, workspace, kind);
    let mut members = read_member_file(&path).await?;
    let added = members.insert(name.to_string());
    if added {
        write_member_file(&path, &members).await?;
        info!(
            "[permissions] added {} '{}' to workspace '{}'",
            kind.as_str(), name, workspace
        );
    } else {
        info!(
            "[permissions] {} '{}' already in workspace '{}' — no change",
            kind.as_str(), name, workspace
        );
    }
    Ok(added)
}

/// Remove a member from a workspace and persist to disk.
/// Returns true if the member was actually present and removed.
pub async fn remove_member(
    repo_root: &Path,
    workspace: &str,
    kind: &MemberType,
    name: &str,
) -> Result<bool> {
    let path = member_file(repo_root, workspace, kind);
    let mut members = read_member_file(&path).await?;
    let removed = members.remove(name);
    if removed {
        write_member_file(&path, &members).await?;
        info!(
            "[permissions] removed {} '{}' from workspace '{}'",
            kind.as_str(), name, workspace
        );
    } else {
        info!(
            "[permissions] {} '{}' not found in workspace '{}' — no change",
            kind.as_str(), name, workspace
        );
    }
    Ok(removed)
}

// ── gitolite.conf generation ─────────────────────────────────────────────────

/// Read every workspace under `permissions/` and every pair-programming slot,
/// then generate the full gitolite.conf and write it to `conf/gitolite.conf`.
pub async fn regenerate_conf(repo_root: &Path) -> Result<()> {
    info!("[permissions] regenerating gitolite.conf …");

    // ── Discover all workspaces ───────────────────────────────────────────────
    let perms_dir = repo_root.join("permissions");
    tokio::fs::create_dir_all(&perms_dir).await?;

    let mut workspaces: BTreeMap<String, WorkspaceMembers> = BTreeMap::new();
    let mut rd = tokio::fs::read_dir(&perms_dir).await?;
    while let Some(entry) = rd.next_entry().await? {
        if entry.file_type().await?.is_dir() {
            let ws = entry.file_name().to_string_lossy().into_owned();
            let members = load_workspace(repo_root, &ws).await?;
            workspaces.insert(ws, members);
        }
    }

    // ── Collect pair-programming slots for every workspace ────────────────────
    // Shape: workspace → repo → branch_prefix → Vec<username>
    let mut pair_slots: BTreeMap<
        String,
        BTreeMap<String, BTreeMap<String, Vec<String>>>,
    > = BTreeMap::new();

    for workspace in workspaces.keys() {
        let slots = load_pair_programming_slots(repo_root, workspace).await?;
        if !slots.is_empty() {
            pair_slots.insert(workspace.clone(), slots);
        }
    }

    let conf = build_conf(&workspaces, &pair_slots);

    let conf_path = repo_root.join("conf/gitolite.conf");
    tokio::fs::create_dir_all(conf_path.parent().unwrap()).await?;
    tokio::fs::write(&conf_path, &conf).await?;

    info!(
        "[permissions] wrote conf/gitolite.conf ({} bytes, {} workspaces)",
        conf.len(),
        workspaces.len()
    );
    debug!("[permissions] conf content:\n{conf}");
    Ok(())
}

/// Pure function — builds the gitolite.conf string from the workspace map and
/// the pair-programming slot map.  Kept separate so it is trivially unit-testable.
///
/// Wildcard repo creation
/// ──────────────────────
/// gitolite supports on-the-fly repo creation when:
///   1. The repo block uses a wildcard pattern (contains `*`, `?`, `[`, or `CREATOR`)
///   2. A `C` (create) permission is granted to the principal who will push first
///   3. `gitolite-options.wildrepos = 1` is set in ~/.gitolite.rc on the server
///
/// We grant `C` to groups (API servers that provision projects) and to
/// individual users so they can create their own project repos under the
/// workspace namespace.  admin_key always has C as well so the backup
/// agent can seed repos if needed.
fn build_conf(
    workspaces: &BTreeMap<String, WorkspaceMembers>,
    pair_slots: &BTreeMap<String, BTreeMap<String, BTreeMap<String, Vec<String>>>>,
) -> String {
    let mut out = String::new();

    // ── Fixed header ──────────────────────────────────────────────────────────
    out.push_str("# Auto-generated by gitolite-sidecar. DO NOT EDIT BY HAND.\n");
    out.push_str("# Changes made directly will be overwritten on the next push.\n\n");

    // ── gitolite-admin: admin_key only ────────────────────────────────────────
    out.push_str("repo gitolite-admin\n");
    out.push_str("    RW+     =   admin_key\n\n");

    // ── testing: open to everyone ─────────────────────────────────────────────
    out.push_str("repo testing\n");
    out.push_str("    RW+     =   @all\n\n");

    // ── Per-workspace wildcard rules ──────────────────────────────────────────
    for (workspace, members) in workspaces {
        // Wildcard pattern: matches (and allows creation of) any repo whose
        // name starts with "<workspace>-".  The `.*` is what tells gitolite
        // this is a wildcard block — without it, gitolite treats the name
        // as a literal and will NOT auto-create the repo on first push.
        let repo_pattern = format!("{workspace}-.*");

        out.push_str(&format!("# ── workspace: {workspace} ──\n"));
        out.push_str(&format!("repo {repo_pattern}\n"));

        // ── Create permission ─────────────────────────────────────────────────
        out.push_str("    C                               =   admin_key\n");
        for group_id in &members.groups {
            out.push_str(&format!(
                "    C                               =   {group_id}\n"
            ));
        }

        // ── Read/write permissions ────────────────────────────────────────────

        // admin_key: full access to all branches of all repos in workspace
        out.push_str("    RW+                             =   admin_key\n");

        // groups: full RW+ on all branches (API servers / CI agents)
        for group_id in &members.groups {
            out.push_str(&format!(
                "    RW+                             =   {group_id}\n"
            ));
        }

        // users: write only to their personal dev-<username>-* branches,
        //        read everything else in the workspace
        for username in &members.users {
            out.push_str(&format!(
                "    RW+ refs/heads/dev-{username}-*  =   {username}\n"
            ));
            out.push_str(&format!(
                "    R                               =   {username}\n"
            ));
        }

        out.push('\n');

        // ── Pair-programming slots for this workspace ─────────────────────────
        // Each slot is a separate `repo` block for the specific repo (not a
        // wildcard) so gitolite can match it exactly and the branch-level rule
        // applies only to the named prefix pattern.
        //
        // Example output:
        //
        //   # pair-programming: acme/api-service
        //   repo acme-api-service
        //       RW+ refs/heads/TICKET-42-*  =   alice bob
        //
        if let Some(repo_map) = pair_slots.get(workspace) {
            for (repo_name, prefix_map) in repo_map {
                let full_repo = format!("{workspace}-{repo_name}");
                out.push_str(&format!("# pair-programming: {workspace}/{repo_name}\n"));
                out.push_str(&format!("repo {full_repo}\n"));

                for (branch_prefix, names) in prefix_map {
                    if names.is_empty() {
                        continue;
                    }
                    let names_str = names.join(" ");
                    out.push_str(&format!(
                        "    RW+ refs/heads/{branch_prefix}-*  =   {names_str}\n"
                    ));
                }

                out.push('\n');
            }
        }
    }

    out
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_members(users: &[&str], groups: &[&str]) -> WorkspaceMembers {
        WorkspaceMembers {
            users:  users.iter().map(|s| s.to_string()).collect(),
            groups: groups.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn conf_contains_fixed_repos() {
        let conf = build_conf(&BTreeMap::new(), &BTreeMap::new());
        assert!(conf.contains("repo gitolite-admin"));
        assert!(conf.contains("RW+     =   admin_key"));
        assert!(conf.contains("repo testing"));
        assert!(conf.contains("RW+     =   @all"));
    }

    #[test]
    fn repo_pattern_is_wildcard() {
        let mut ws = BTreeMap::new();
        ws.insert("wname".into(), make_members(&[], &[]));
        let conf = build_conf(&ws, &BTreeMap::new());
        assert!(conf.contains("repo wname-.*"));
        assert!(!conf.contains("repo wname-["));
    }

    #[test]
    fn admin_key_has_create_and_rw_in_workspace() {
        let mut ws = BTreeMap::new();
        ws.insert("wname".into(), make_members(&["vriksh"], &[]));
        let conf = build_conf(&ws, &BTreeMap::new());
        let block_start = conf.find("# ── workspace: wname").unwrap();
        let block = &conf[block_start..];
        assert!(block.contains("C                               =   admin_key"));
        assert!(block.contains("RW+                             =   admin_key"));
    }

    #[test]
    fn user_gets_create_dev_branch_write_and_read() {
        let mut ws = BTreeMap::new();
        ws.insert("wname".into(), make_members(&["vriksh"], &[]));
        let conf = build_conf(&ws, &BTreeMap::new());
        let block_start = conf.find("# ── workspace: wname").unwrap();
        let block = &conf[block_start..];
        assert!(block.contains("C                               =   vriksh"));
        assert!(block.contains("RW+ refs/heads/dev-vriksh-*  =   vriksh"));
        assert!(block.contains("R                               =   vriksh"));
    }

    #[test]
    fn group_gets_create_and_full_rw() {
        let uuid = "435a8c5a-da91-4b95-8364-40ca23cb1109";
        let mut ws = BTreeMap::new();
        ws.insert("wname".into(), make_members(&[], &[uuid]));
        let conf = build_conf(&ws, &BTreeMap::new());
        let block_start = conf.find("# ── workspace: wname").unwrap();
        let block = &conf[block_start..];
        assert!(block.contains(&format!("C                               =   {uuid}")));
        assert!(block.contains(&format!("RW+                             =   {uuid}")));
    }

    #[test]
    fn multiple_users_and_groups() {
        let mut ws = BTreeMap::new();
        ws.insert(
            "wname".into(),
            make_members(
                &["alice", "bob"],
                &["uuid-aaa", "uuid-bbb"],
            ),
        );
        let conf = build_conf(&ws, &BTreeMap::new());
        for user in &["alice", "bob"] {
            assert!(conf.contains(&format!("dev-{user}-*  =   {user}")));
        }
        for group in &["uuid-aaa", "uuid-bbb"] {
            assert!(conf.contains(&format!("RW+                             =   {group}")));
        }
    }

    #[test]
    fn pair_programming_slot_emits_branch_rule() {
        let mut ws = BTreeMap::new();
        ws.insert("acme".into(), make_members(&[], &[]));

        // pair_slots: acme → api-service → TICKET-42 → [alice, bob]
        let mut prefix_map = BTreeMap::new();
        prefix_map.insert(
            "TICKET-42".to_string(),
            vec!["alice".to_string(), "bob".to_string()],
        );
        let mut repo_map = BTreeMap::new();
        repo_map.insert("api-service".to_string(), prefix_map);
        let mut pair_slots = BTreeMap::new();
        pair_slots.insert("acme".to_string(), repo_map);

        let conf = build_conf(&ws, &pair_slots);

        assert!(conf.contains("# pair-programming: acme/api-service"));
        assert!(conf.contains("repo acme-api-service"));
        assert!(conf.contains("RW+ refs/heads/TICKET-42-*  =   alice bob"));
    }

    #[test]
    fn pair_programming_multiple_prefixes() {
        let mut ws = BTreeMap::new();
        ws.insert("acme".into(), make_members(&[], &[]));

        let mut prefix_map = BTreeMap::new();
        prefix_map.insert("TICKET-1".to_string(), vec!["carol".to_string()]);
        prefix_map.insert("TICKET-2".to_string(), vec!["dave".to_string(), "eve".to_string()]);
        let mut repo_map = BTreeMap::new();
        repo_map.insert("backend".to_string(), prefix_map);
        let mut pair_slots = BTreeMap::new();
        pair_slots.insert("acme".to_string(), repo_map);

        let conf = build_conf(&ws, &pair_slots);

        assert!(conf.contains("RW+ refs/heads/TICKET-1-*  =   carol"));
        assert!(conf.contains("RW+ refs/heads/TICKET-2-*  =   dave eve"));
    }

    #[test]
    fn pair_programming_empty_names_skipped() {
        let mut ws = BTreeMap::new();
        ws.insert("acme".into(), make_members(&[], &[]));

        let mut prefix_map = BTreeMap::new();
        prefix_map.insert("TICKET-99".to_string(), vec![]); // empty — should be skipped
        let mut repo_map = BTreeMap::new();
        repo_map.insert("svc".to_string(), prefix_map);
        let mut pair_slots = BTreeMap::new();
        pair_slots.insert("acme".to_string(), repo_map);

        let conf = build_conf(&ws, &pair_slots);

        // The repo block header is still emitted but no RW+ line for TICKET-99
        assert!(!conf.contains("TICKET-99"));
    }

    #[test]
    fn pair_programming_no_slots_no_extra_blocks() {
        let mut ws = BTreeMap::new();
        ws.insert("acme".into(), make_members(&["vriksh"], &[]));
        let conf = build_conf(&ws, &BTreeMap::new());
        assert!(!conf.contains("pair-programming"));
    }
}