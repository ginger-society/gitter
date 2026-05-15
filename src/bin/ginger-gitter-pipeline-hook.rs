mod pipeline_hook;

use pipeline_hook::pipeline;
use std::env;
use std::process::ExitCode;

const ADMIN_GIT_DIR: &str = "/home/git/repositories/gitolite-admin.git";
const REPOS_DIR: &str = "/home/git/repositories";
const SIDECAR_URL: &str = "http://ginger-gitter-sidecar:8080";
const CLUSTER_TTL_SECONDS: u32 = 5 * 24 * 60 * 60;

fn main() -> ExitCode {
    let args: Vec<String> = env::args().collect();

    if args.len() != 6 {
        eprintln!(
            "Usage: ginger-gitter-pipeline-hook <gl_user> <gl_repo> <refname> <old_rev> <new_rev>"
        );
        eprintln!("Got {} args: {:?}", args.len() - 1, &args[1..]);
        return ExitCode::FAILURE;
    }

    match pipeline::run(
        &args[1], &args[2], &args[3], &args[4], &args[5],
        ADMIN_GIT_DIR, REPOS_DIR, SIDECAR_URL, CLUSTER_TTL_SECONDS,
    ) {
        Ok(_) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("[ginger-gitter] ERROR: {}", e);
            ExitCode::FAILURE
        }
    }
}