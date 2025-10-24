use std::{env, path::PathBuf};
use tracing::info;

pub const GIT_AUTHOR_NAME: &str = "Overleaf Sync";
pub const GIT_AUTHOR_EMAIL: &str = "sync@example.invalid";

#[derive(Clone, Debug)]
pub struct Config {
    pub port: u16,
    pub overleaf_data_path: PathBuf,
    pub projects_dir: PathBuf,
    pub git_root: PathBuf,
    pub readonly_branch: String,
    pub admin_password: Option<String>,
    pub admin_cookie_secure: bool,
    pub admin_session_ttl_seconds: u64,
}

impl Config {
    pub fn from_env() -> Self {
        let port = env::var("PORT")
            .ok()
            .and_then(|v| v.parse::<u16>().ok())
            .unwrap_or(8022);

        let overleaf_data_path = resolve_path(
            env::var("OVERLEAF_DATA_PATH")
                .map(PathBuf::from)
                .unwrap_or_else(|_| PathBuf::from("/overleaf-data")),
        );

        let projects_dir = env::var("PROJECTS_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("data/projects"));

        let git_root = resolve_path(
            env::var("GIT_ROOT")
                .map(PathBuf::from)
                .unwrap_or_else(|_| PathBuf::from("/data/git-bridge")),
        );

        let readonly_branch =
            env::var("READONLY_BRANCH").unwrap_or_else(|_| "master".to_string());

        let admin_password = env::var("ADMIN_PASSWORD").ok();

        let admin_cookie_secure = env::var("ADMIN_COOKIE_SECURE")
            .ok()
            .map(|v| v.trim().to_ascii_lowercase())
            .map(|v| matches!(v.as_str(), "1" | "true" | "yes" | "on"))
            .unwrap_or(false);

        let admin_session_ttl_seconds = env::var("ADMIN_SESSION_TTL_SECONDS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .filter(|&ttl| ttl > 0)
            .unwrap_or(3600);

        Self {
            port,
            overleaf_data_path,
            projects_dir,
            git_root,
            readonly_branch,
            admin_password,
            admin_cookie_secure,
            admin_session_ttl_seconds,
        }
    }

    pub fn project_source_dir(&self, project_id: &str) -> PathBuf {
        self.overleaf_data_path
            .join(&self.projects_dir)
            .join(project_id)
    }

    pub fn bare_repo_dir(&self, project_id: &str) -> PathBuf {
        self.git_root.join(format!("{project_id}.git"))
    }

    pub fn tokens_file(&self) -> PathBuf {
        self.git_root.join("tokens.json")
    }
}

fn resolve_path(p: PathBuf) -> PathBuf {
    if p.is_absolute() {
        p
    } else {
        env::current_dir().map(|base| base.join(&p)).unwrap_or(p)
    }
}

impl Config {
    pub fn log_summary(&self) {
        info!("config initialized");
        info!("  port          : {}", self.port);
        info!("  git_root      : {}", self.git_root.display());
        info!(
            "  overleaf_root : {}",
            self.overleaf_data_path.display()
        );
        info!("  projects_dir  : {}", self.projects_dir.display());
        info!("  tokens_file   : {}", self.tokens_file().display());
        info!("  readonly_branch: {}", self.readonly_branch);
        if self.admin_password.is_some() {
            info!("  admin_ui      : enabled");
            info!(
                "  cookie secure : {}",
                if self.admin_cookie_secure { "on" } else { "off" }
            );
            info!(
                "  session ttl   : {} seconds",
                self.admin_session_ttl_seconds
            );
        } else {
            info!("  admin_ui      : disabled (no ADMIN_PASSWORD)");
        }
    }
}
