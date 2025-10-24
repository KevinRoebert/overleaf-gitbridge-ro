use crate::config::{Config, GIT_AUTHOR_EMAIL, GIT_AUTHOR_NAME};
use crate::error::BridgeError;
use chrono::Utc;
use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};
use tempfile::TempDir;
use tracing::{debug, info, warn};
use walkdir::WalkDir;

const DEFAULT_GITIGNORE: &str = r#"
output.pdf
.project-sync-state
*.synctex.gz
*.aux
*.log
*.bbl
*.blg
*.out
*.toc
*.stdout
*.stderr
*.fls
*.fdb_latexmk
"#;

/// Public async wrapper that also handles locking per project.
/// We will call this from the HTTP handler.
pub async fn ensure_repo(cfg: Config, project_id: &str) -> Result<(), BridgeError> {
    // We do heavy filesystem + git work, so run it blocking.
    let cfg_cloned = cfg.clone();
    let project_id_owned = project_id.to_string();
    tokio::task::spawn_blocking(move || ensure_repo_blocking(&cfg_cloned, &project_id_owned))
        .await
        .map_err(|e| BridgeError::Other(format!("join error: {e}")))?
}

fn ensure_repo_blocking(cfg: &Config, project_id: &str) -> Result<(), BridgeError> {
    let source_dir = cfg.project_source_dir(project_id);
    let bare_repo_dir = cfg.bare_repo_dir(project_id);

    if !source_dir.is_dir() {
        if bare_repo_dir.exists() {
            match fs::remove_dir_all(&bare_repo_dir) {
                Ok(_) => {
                    info!(%project_id, "removed stale bare repo because source project is missing")
                }
                Err(e) => warn!(%project_id, error = %e, "failed to remove stale bare repo"),
            }
        }
        return Err(BridgeError::ProjectNotFound(project_id.to_string()));
    }

    if !bare_repo_dir.is_dir() {
        info!(%project_id, "bare repo does not exist, creating initial snapshot");
        initial_create(cfg, project_id, &source_dir, &bare_repo_dir)?;
    } else {
        debug!(%project_id, "bare repo exists, syncing");
        sync_existing(cfg, project_id, &source_dir, &bare_repo_dir)?;
    }

    Ok(())
}

/// Create initial bare repo from ShareLatex snapshot
fn initial_create(
    cfg: &Config,
    project_id: &str,
    source_dir: &Path,
    bare_repo_dir: &Path,
) -> Result<(), BridgeError> {
    if let Some(parent) = bare_repo_dir.parent() {
        fs::create_dir_all(parent).map_err(BridgeError::Io)?;
    }

    let tmpdir = TempDir::new_in(&cfg.git_root).map_err(|e| {
        BridgeError::Other(format!(
            "failed to create tempdir in {}: {e}",
            cfg.git_root.display()
        ))
    })?;
    let tmp = tmpdir.path();

    copy_snapshot(source_dir, tmp)?;
    ensure_gitignore(tmp)?;

    // git init
    run_git(&["init"], tmp)?;
    // checkout branch we want
    run_git(&["checkout", "-b", &cfg.readonly_branch], tmp)?;

    // config user
    run_git(&["config", "user.name", GIT_AUTHOR_NAME], tmp)?;
    run_git(&["config", "user.email", GIT_AUTHOR_EMAIL], tmp)?;

    // add & commit
    run_git(&["add", "-A"], tmp)?;
    let msg = format!("Initial snapshot from ShareLatex project {project_id}");
    run_git(&["commit", "-m", &msg], tmp)?;

    // clone --bare into bare_repo_dir
    run_git(
        &[
            "clone",
            "--bare",
            ".",
            bare_repo_dir
                .to_str()
                .ok_or_else(|| BridgeError::Other("invalid bare path".into()))?,
        ],
        tmp,
    )?;

    // Make sure HEAD in bare repo points to our readonly branch
    run_git(
        &[
            "symbolic-ref",
            "HEAD",
            &format!("refs/heads/{}", cfg.readonly_branch),
        ],
        bare_repo_dir,
    )?;

    Ok(())
}

/// Sync changes from ShareLatex data dir into existing bare repo
fn sync_existing(
    cfg: &Config,
    project_id: &str,
    source_dir: &Path,
    bare_repo_dir: &Path,
) -> Result<(), BridgeError> {
    let tmpdir = TempDir::new_in(&cfg.git_root).map_err(|e| {
        BridgeError::Other(format!(
            "failed to create tempdir in {}: {e}",
            cfg.git_root.display()
        ))
    })?;
    let tmp = tmpdir.path();

    // git clone bare_repo_dir tmp
    run_git(
        &[
            "clone",
            bare_repo_dir
                .to_str()
                .ok_or_else(|| BridgeError::Other("invalid bare path".into()))?,
            ".",
        ],
        tmp,
    )?;

    // checkout desired branch (create if missing)
    if let Err(e) = run_git(&["checkout", &cfg.readonly_branch], tmp) {
        warn!("branch checkout failed: {e}, trying to create");
        run_git(&["checkout", "-b", &cfg.readonly_branch], tmp)?;
    }

    // mirror ShareLatex project files into tmp working tree
    sync_worktree_with_source(source_dir, tmp)?;
    ensure_gitignore(tmp)?;

    // git add -A
    run_git(&["add", "-A"], tmp)?;

    // check if staged diff exists
    let has_changes = staged_has_changes(tmp)?;

    if has_changes {
        // commit & push
        run_git(&["config", "user.name", GIT_AUTHOR_NAME], tmp)?;
        run_git(&["config", "user.email", GIT_AUTHOR_EMAIL], tmp)?;

        let ts = Utc::now().to_rfc3339();
        let msg = format!("Sync {ts} from ShareLatex project {project_id}");

        run_git(&["commit", "-m", &msg], tmp)?;
        run_git(&["push", "origin", &cfg.readonly_branch], tmp)?;
        info!(%project_id, "pushed new commit");
    } else {
        debug!(%project_id, "no changes detected, skipping commit");
    }

    Ok(())
}

/// Returns true if there are staged changes
fn staged_has_changes(repo: &Path) -> Result<bool, BridgeError> {
    let status = Command::new("git")
        .arg("diff")
        .arg("--staged")
        .arg("--quiet")
        .current_dir(repo)
        .status()
        .map_err(BridgeError::Io)?;

    match status.code() {
        Some(0) => Ok(false), // no diff
        Some(1) => Ok(true),  // there is a diff
        other => Err(BridgeError::Other(format!(
            "git diff --staged --quiet unexpected exit code {other:?}"
        ))),
    }
}

/// Copy entire snapshot from source -> dest (no delete here)
fn copy_snapshot(src: &Path, dst: &Path) -> Result<(), BridgeError> {
    copy_recursive(src, dst)
}

/// Sync snapshot (copy + delete missing in dst) into already-cloned worktree
fn sync_worktree_with_source(src: &Path, dst: &Path) -> Result<(), BridgeError> {
    copy_recursive(src, dst)?;
    delete_removed(src, dst)?;
    Ok(())
}

/// Copy files recursively from `src` to `dst`
/// Skips `.git` dirs in `src` just in case.
fn copy_recursive(src: &Path, dst: &Path) -> Result<(), BridgeError> {
    for entry in WalkDir::new(src).into_iter().filter_map(|e| e.ok()) {
        let path = entry.path();
        let rel = match path.strip_prefix(src) {
            Ok(r) => r,
            Err(_) => continue,
        };
        if rel.as_os_str().is_empty() {
            continue;
        }
        if rel.components().any(|c| c.as_os_str() == ".git") {
            // skip any embedded .git
            continue;
        }
        let target_path = dst.join(rel);
        if entry.file_type().is_dir() {
            fs::create_dir_all(&target_path).map_err(BridgeError::Io)?;
        } else if entry.file_type().is_file() {
            if let Some(parent) = target_path.parent() {
                fs::create_dir_all(parent).map_err(BridgeError::Io)?;
            }
            fs::copy(path, &target_path).map_err(BridgeError::Io)?;
        }
    }
    Ok(())
}

/// Delete files/dirs in `dst` which no longer exist in `src`
/// Never touch `dst/.git` directory.
fn delete_removed(src: &Path, dst: &Path) -> Result<(), BridgeError> {
    for entry in WalkDir::new(dst)
        .into_iter()
        .filter_map(|e| e.ok())
        .collect::<Vec<_>>()
    // collect first because we'll mutate
    {
        let path = entry.path();
        let rel = match path.strip_prefix(dst) {
            Ok(r) => r,
            Err(_) => continue,
        };
        if rel.as_os_str().is_empty() {
            continue;
        }

        // don't delete .git or anything underneath it
        if rel.components().next().map(|c| c.as_os_str()) == Some(".git".as_ref()) {
            continue;
        }

        let corresponding_src = src.join(rel);
        if !corresponding_src.exists() {
            if entry.file_type().is_dir() {
                fs::remove_dir_all(path).map_err(BridgeError::Io)?;
            } else {
                fs::remove_file(path).map_err(BridgeError::Io)?;
            }
        }
    }
    Ok(())
}

/// Ensure a default .gitignore exists in dst root
fn ensure_gitignore(dst: &Path) -> Result<(), BridgeError> {
    let gi_path = dst.join(".gitignore");
    if !gi_path.exists() {
        fs::write(&gi_path, DEFAULT_GITIGNORE).map_err(BridgeError::Io)?;
    } else {
        // keep existing .gitignore, do not overwrite
    }
    Ok(())
}

/// Run a git command and ensure success
fn run_git(args: &[&str], cwd: &Path) -> Result<(), BridgeError> {
    let mut cmd = std::process::Command::new("git");
    cmd.args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let out = cmd.output().map_err(|e| {
        BridgeError::Other(format!(
            "failed to run git {:?} in {}: {e}",
            args,
            cwd.display()
        ))
    })?;
    if !out.status.success() {
        return Err(BridgeError::GitFailed(
            format!("git {:?}", args),
            String::from_utf8_lossy(&out.stderr).to_string(),
        ));
    }
    Ok(())
}
