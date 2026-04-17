//! Backend registry: discover → first-boot copy → layered merge.
//!
//! # Precedence
//!
//! On each boot, the registry is resolved via:
//!
//! 1. If `$PEER_BACKENDS_TOML` points to a file, load **only** that file.
//!    Used by tests and CI; bypasses the rest.
//! 2. Otherwise:
//!    a. Ensure `~/.nott/peer.toml` exists — if not, copy the shipped
//!    `peer-defaults.toml` to it and log the creation.
//!    b. Load `~/.nott/peer.toml` (user global).
//!    c. If `./.nott/peer.toml` exists in cwd, merge it over the user
//!    global (project override wins on `name` collision).
//!
//! The shipped defaults file is only consulted during the first-boot copy.
//! After that it is never read again. Reset by `rm ~/.nott/peer.toml`.
//!
//! # Zero hardcode
//!
//! No backend spec is hardcoded in Rust. [`BackendSpec`] is a pure schema —
//! every entry comes from TOML. Add a backend by appending to the user toml;
//! no rebuild required.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::env;

use serde::{Deserialize, Serialize};

use crate::error::PeerError;

/// Canonical spec for one backend. Pure schema — never hardcoded.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BackendSpec {
    /// Lookup key used by callers (e.g. `codex`, `gemini`).
    pub name: String,
    /// Human-readable description surfaced via `list_backends`.
    #[serde(default)]
    pub description: String,
    /// Executable to invoke (must resolve via `$PATH` unless absolute).
    pub command: String,
    /// Base arguments. Supports `{prompt}`, `{env:VAR:default}`, `{extra}`.
    #[serde(default)]
    pub args: Vec<String>,
    /// When true, the prompt is piped via stdin (and `{prompt}` is NOT
    /// substituted in args). When false, `{prompt}` must appear in args.
    #[serde(default)]
    pub stdin: bool,
    /// Environment variables layered onto the inherited env.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Default timeout. Callers may override per-call within the clamp range.
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms_default: u64,
    /// Optional auth hint surfaced when the subprocess reports an auth error.
    #[serde(default)]
    pub auth_hint: Option<String>,
}

fn default_timeout_ms() -> u64 {
    180_000
}

/// On-disk TOML schema — a flat list of `[[backend]]` tables.
#[derive(Debug, Deserialize, Serialize, Default)]
struct RegistryFile {
    #[serde(default, rename = "backend")]
    backends: Vec<BackendSpec>,
}

/// Fully resolved registry after precedence merge.
#[derive(Debug, Clone)]
pub struct Registry {
    /// Keyed by backend name.
    backends: BTreeMap<String, BackendSpec>,
    /// Absolute path to the primary registry file that was loaded.
    /// Reported via `list_backends` for debug transparency.
    registry_path: PathBuf,
    /// True when a project-local override was merged on top.
    project_overrides_loaded: bool,
    /// True when the `$PEER_BACKENDS_TOML` escape hatch was used.
    env_override: bool,
    /// True when `~/.nott/peer.toml` was created during this boot
    /// (first-boot copy from shipped defaults).
    created_user_toml: bool,
}

impl Registry {
    /// Resolve the registry using the full precedence chain.
    pub fn load() -> Result<Self, PeerError> {
        Self::load_from_env(&RealEnv)
    }

    /// Testable variant — all environment lookups go through [`EnvProvider`].
    pub fn load_from_env<E: EnvProvider>(envp: &E) -> Result<Self, PeerError> {
        if let Some(path) = envp.env_override_path() {
            let file = read_registry_file(&path)?;
            let backends = file
                .backends
                .into_iter()
                .map(|b| (b.name.clone(), b))
                .collect();
            return Ok(Self {
                backends,
                registry_path: path,
                project_overrides_loaded: false,
                env_override: true,
                created_user_toml: false,
            });
        }

        let user_path = envp.user_config_path();
        let defaults_path = envp.shipped_defaults_path()?;
        let created = ensure_user_config(&user_path, &defaults_path)?;

        let user_file = read_registry_file(&user_path)?;
        let mut backends: BTreeMap<String, BackendSpec> = user_file
            .backends
            .into_iter()
            .map(|b| (b.name.clone(), b))
            .collect();

        let mut project_overrides_loaded = false;
        let project_path = envp.project_config_path();
        if project_path.is_file() {
            let project_file = read_registry_file(&project_path)?;
            for spec in project_file.backends {
                backends.insert(spec.name.clone(), spec);
            }
            project_overrides_loaded = true;
        }

        Ok(Self {
            backends,
            registry_path: user_path,
            project_overrides_loaded,
            env_override: false,
            created_user_toml: created,
        })
    }

    /// Look up a backend spec by name.
    pub fn get(&self, name: &str) -> Option<&BackendSpec> {
        self.backends.get(name)
    }

    /// Snapshot of all loaded backends, sorted by name.
    pub fn list(&self) -> Vec<&BackendSpec> {
        self.backends.values().collect()
    }

    /// Path to the primary registry file actually consulted.
    pub fn registry_path(&self) -> &Path {
        &self.registry_path
    }

    pub fn project_overrides_loaded(&self) -> bool {
        self.project_overrides_loaded
    }

    pub fn env_override(&self) -> bool {
        self.env_override
    }

    pub fn created_user_toml(&self) -> bool {
        self.created_user_toml
    }
}

fn read_registry_file(path: &Path) -> Result<RegistryFile, PeerError> {
    let bytes = fs::read_to_string(path).map_err(|e| {
        PeerError::RegistryLoad(format!("read {}: {e}", path.display()))
    })?;
    toml::from_str::<RegistryFile>(&bytes)
        .map_err(|e| PeerError::RegistryLoad(format!("parse {}: {e}", path.display())))
}

/// Ensure the user config file exists, copying from shipped defaults if
/// necessary. Returns `true` if a copy happened (first-boot).
fn ensure_user_config(user: &Path, defaults: &Path) -> Result<bool, PeerError> {
    if user.is_file() {
        return Ok(false);
    }
    if let Some(parent) = user.parent() {
        fs::create_dir_all(parent).map_err(|e| {
            PeerError::RegistryLoad(format!("create {}: {e}", parent.display()))
        })?;
    }
    if !defaults.is_file() {
        return Err(PeerError::RegistryLoad(format!(
            "shipped defaults not found at {}; set $PEER_DEFAULTS_TOML or reinstall",
            defaults.display()
        )));
    }
    fs::copy(defaults, user).map_err(|e| {
        PeerError::RegistryLoad(format!(
            "copy {} -> {}: {e}",
            defaults.display(),
            user.display()
        ))
    })?;
    tracing::info!(
        target = "peer::registry",
        "Created {} from shipped defaults — edit to customize",
        user.display()
    );
    Ok(true)
}

// -------------------------------------------------------------------------
// Environment provider (mockable)
// -------------------------------------------------------------------------

/// Abstracts over environment-variable and filesystem-path lookups so tests
/// can feed a synthetic layout without monkey-patching `$HOME`.
pub trait EnvProvider {
    /// `$PEER_BACKENDS_TOML` → absolute path, if set AND file-like.
    fn env_override_path(&self) -> Option<PathBuf>;
    /// Absolute path to `~/.nott/peer.toml`.
    fn user_config_path(&self) -> PathBuf;
    /// Absolute path to `./.nott/peer.toml` in cwd.
    fn project_config_path(&self) -> PathBuf;
    /// Absolute path to the shipped `peer-defaults.toml`.
    fn shipped_defaults_path(&self) -> Result<PathBuf, PeerError>;
}

/// Production-mode provider.
pub struct RealEnv;

impl EnvProvider for RealEnv {
    fn env_override_path(&self) -> Option<PathBuf> {
        env::var_os("PEER_BACKENDS_TOML").map(PathBuf::from)
    }

    fn user_config_path(&self) -> PathBuf {
        let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
        home.join(".nott").join("peer.toml")
    }

    fn project_config_path(&self) -> PathBuf {
        env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(".nott")
            .join("peer.toml")
    }

    fn shipped_defaults_path(&self) -> Result<PathBuf, PeerError> {
        if let Some(p) = env::var_os("PEER_DEFAULTS_TOML") {
            return Ok(PathBuf::from(p));
        }
        let candidates: Vec<PathBuf> = {
            let mut v = Vec::new();
            if let Ok(exe) = env::current_exe() {
                if let Some(parent) = exe.parent() {
                    v.push(parent.join("peer-defaults.toml"));
                    v.push(parent.join("share/peer-mcp/peer-defaults.toml"));
                    if let Some(grand) = parent.parent() {
                        v.push(grand.join("share/peer-mcp/peer-defaults.toml"));
                    }
                }
            }
            // Cargo layout: <manifest>/peer-defaults.toml ← dev mode.
            v.push(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("peer-defaults.toml"));
            v
        };
        candidates
            .into_iter()
            .find(|p| p.is_file())
            .ok_or_else(|| {
                PeerError::RegistryLoad(
                    "could not locate peer-defaults.toml (set $PEER_DEFAULTS_TOML)".to_string(),
                )
            })
    }
}

// -------------------------------------------------------------------------
// Tests
// -------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeEnv {
        env_override: Option<PathBuf>,
        user_cfg: PathBuf,
        project_cfg: PathBuf,
        defaults: PathBuf,
    }

    impl EnvProvider for FakeEnv {
        fn env_override_path(&self) -> Option<PathBuf> {
            self.env_override.clone()
        }
        fn user_config_path(&self) -> PathBuf {
            self.user_cfg.clone()
        }
        fn project_config_path(&self) -> PathBuf {
            self.project_cfg.clone()
        }
        fn shipped_defaults_path(&self) -> Result<PathBuf, PeerError> {
            Ok(self.defaults.clone())
        }
    }

    fn write_defaults(dir: &Path) -> PathBuf {
        let p = dir.join("peer-defaults.toml");
        fs::write(
            &p,
            r#"
[[backend]]
name = "codex"
command = "codex"
args = ["exec"]
stdin = true
timeout_ms_default = 480000

[[backend]]
name = "gemini"
command = "gemini"
args = ["-p", "{prompt}"]
"#,
        )
        .unwrap();
        p
    }

    #[test]
    fn first_boot_copies_defaults() {
        let tmp = tempfile::tempdir().unwrap();
        let defaults = write_defaults(tmp.path());
        let user_cfg = tmp.path().join("home/.nott/peer.toml");
        let project_cfg = tmp.path().join("cwd/.nott/peer.toml");
        let envp = FakeEnv {
            env_override: None,
            user_cfg: user_cfg.clone(),
            project_cfg,
            defaults,
        };

        let reg = Registry::load_from_env(&envp).expect("load");
        assert!(reg.created_user_toml());
        assert!(user_cfg.is_file());
        assert_eq!(reg.list().len(), 2);
        assert!(reg.get("codex").is_some());
    }

    #[test]
    fn second_boot_does_not_overwrite() {
        let tmp = tempfile::tempdir().unwrap();
        let defaults = write_defaults(tmp.path());
        let user_cfg = tmp.path().join("home/.nott/peer.toml");
        fs::create_dir_all(user_cfg.parent().unwrap()).unwrap();
        fs::write(
            &user_cfg,
            r#"
[[backend]]
name = "codex"
command = "codex"
args = ["exec"]
timeout_ms_default = 999999
"#,
        )
        .unwrap();
        let envp = FakeEnv {
            env_override: None,
            user_cfg: user_cfg.clone(),
            project_cfg: tmp.path().join("cwd/.nott/peer.toml"),
            defaults,
        };
        let reg = Registry::load_from_env(&envp).expect("load");
        assert!(!reg.created_user_toml());
        assert_eq!(reg.get("codex").unwrap().timeout_ms_default, 999_999);
    }

    #[test]
    fn project_override_wins_on_name_collision() {
        let tmp = tempfile::tempdir().unwrap();
        let defaults = write_defaults(tmp.path());
        let user_cfg = tmp.path().join("home/.nott/peer.toml");
        fs::create_dir_all(user_cfg.parent().unwrap()).unwrap();
        fs::copy(&defaults, &user_cfg).unwrap();

        let project_cfg = tmp.path().join("cwd/.nott/peer.toml");
        fs::create_dir_all(project_cfg.parent().unwrap()).unwrap();
        fs::write(
            &project_cfg,
            r#"
[[backend]]
name = "gemini"
command = "gemini"
args = ["-p", "{prompt}"]
timeout_ms_default = 42
"#,
        )
        .unwrap();

        let envp = FakeEnv {
            env_override: None,
            user_cfg,
            project_cfg,
            defaults,
        };
        let reg = Registry::load_from_env(&envp).expect("load");
        assert!(reg.project_overrides_loaded());
        assert_eq!(reg.get("gemini").unwrap().timeout_ms_default, 42);
        assert_eq!(reg.get("codex").unwrap().timeout_ms_default, 480_000);
    }

    #[test]
    fn env_override_bypasses_user_and_project() {
        let tmp = tempfile::tempdir().unwrap();
        let defaults = write_defaults(tmp.path());
        let override_path = tmp.path().join("override.toml");
        fs::write(
            &override_path,
            r#"
[[backend]]
name = "zai"
command = "zai"
args = ["{prompt}"]
"#,
        )
        .unwrap();

        let user_cfg = tmp.path().join("home/.nott/peer.toml");
        let project_cfg = tmp.path().join("cwd/.nott/peer.toml");
        // Place decoy files — should be ignored when override is set.
        fs::create_dir_all(user_cfg.parent().unwrap()).unwrap();
        fs::write(&user_cfg, "# decoy\n").unwrap();

        let envp = FakeEnv {
            env_override: Some(override_path.clone()),
            user_cfg,
            project_cfg,
            defaults,
        };
        let reg = Registry::load_from_env(&envp).expect("load");
        assert!(reg.env_override());
        assert_eq!(reg.list().len(), 1);
        assert!(reg.get("zai").is_some());
        assert_eq!(reg.registry_path(), override_path);
    }

    #[test]
    fn malformed_toml_reports_registry_load_error() {
        let tmp = tempfile::tempdir().unwrap();
        let bad = tmp.path().join("bad.toml");
        fs::write(&bad, "this is not = \"valid toml\" [[[").unwrap();
        let envp = FakeEnv {
            env_override: Some(bad),
            user_cfg: tmp.path().join("u.toml"),
            project_cfg: tmp.path().join("p.toml"),
            defaults: tmp.path().join("d.toml"),
        };
        let err = Registry::load_from_env(&envp).unwrap_err();
        assert_eq!(err.exit_code(), 6);
    }
}
