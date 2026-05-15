use std::env;
use std::path::PathBuf;
use std::process::{Command, ExitCode};

fn main() -> ExitCode {
    let args: Vec<String> = env::args().collect();

    if args.len() != 6 {
        eprintln!(
            "Usage: ginger-gitter-pipeline-hook <gl_user> <gl_repo> <refname> <old_rev> <new_rev>"
        );
        eprintln!("Got {} args: {:?}", args.len() - 1, &args[1..]);
        return ExitCode::FAILURE;
    }

    let gl_user = &args[1];
    let gl_repo = &args[2];
    let refname = &args[3];
    let old_rev = &args[4];
    let new_rev = &args[5];

    println!(
        "[pipeline-hook] push received: user={} repo={} ref={} old={} new={}",
        gl_user, gl_repo, refname, old_rev, new_rev
    );

    // Only act on branch pushes (refs/heads/*)
    if !refname.starts_with("refs/heads/") {
        println!("[pipeline-hook] Skipping non-branch ref: {}", refname);
        return ExitCode::SUCCESS;
    }

    // Skip delete pushes (new_rev is all zeros)
    if new_rev.chars().all(|c| c == '0') {
        println!("[pipeline-hook] Skipping branch deletion");
        return ExitCode::SUCCESS;
    }

    let repo_path = PathBuf::from(format!("/home/git/repositories/{}.git", gl_repo));
    if !repo_path.exists() {
        eprintln!(
            "[pipeline-hook] ERROR: repo path does not exist: {}",
            repo_path.display()
        );
        return ExitCode::FAILURE;
    }

    println!(
        "[pipeline-hook] Scanning .tekton/ in commit {} of repo {}",
        &new_rev[..8.min(new_rev.len())],
        gl_repo
    );

    match list_tekton_files(&repo_path, new_rev) {
        Ok(files) => {
            if files.is_empty() {
                println!("[pipeline-hook] No .tekton pipeline files found in this commit.");
            } else {
                println!("[pipeline-hook] Found {} pipeline file(s):", files.len());
                for f in &files {
                    println!("[pipeline-hook]   {}", f);
                }
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("[pipeline-hook] ERROR listing .tekton files: {}", e);
            ExitCode::FAILURE
        }
    }
}

/// Lists all .yaml/.yml files under .tekton/ in the given commit (new_rev),
/// using `git ls-tree` against the bare repo at repo_path.
fn list_tekton_files(repo_path: &PathBuf, new_rev: &str) -> Result<Vec<String>, String> {
    // git ls-tree -r --name-only <new_rev> -- .tekton/
    // -r: recurse into subtrees
    // --name-only: just filenames
    let output = Command::new("git")
        .args([
            "ls-tree",
            "-r",
            "--name-only",
            new_rev,
            "--",
            ".tekton/",
        ])
        .env("GIT_DIR", repo_path)
        .output()
        .map_err(|e| format!("failed to run git ls-tree: {}", e))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        // ls-tree exits non-zero if the path doesn't exist in the tree — that's fine
        if stderr.contains("Not a valid object name")
            || stderr.contains("fatal")
        {
            return Err(format!("git ls-tree failed: {}", stderr.trim()));
        }
        // Path simply doesn't exist in this commit
        return Ok(vec![]);
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let files: Vec<String> = stdout
        .lines()
        .filter(|line| {
            let lower = line.to_lowercase();
            lower.ends_with(".yaml") || lower.ends_with(".yml")
        })
        .map(|s| s.to_string())
        .collect();

    Ok(files)
}