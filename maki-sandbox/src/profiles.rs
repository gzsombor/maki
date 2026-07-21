use std::path::PathBuf;

use crate::namespace::NamespaceConfig;

/// How a profile directory is mounted inside the sandbox.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MountUsage {
    /// Read-write access to the directory.
    Write,
    /// Read-only access to the directory.
    ReadOnly,
    /// Add the directory to $PATH.
    OnlyPath,
    /// Recreate the host symlink inside the sandbox (e.g. /etc/localtime).
    SymLink,
}

impl MountUsage {
    pub fn label(self) -> &'static str {
        match self {
            Self::Write => "rw",
            Self::ReadOnly => "ro",
            Self::OnlyPath => "on $PATH",
            Self::SymLink => "symlink",
        }
    }
}

/// A single directory mount within a profile.
#[derive(Debug, Clone)]
pub struct ProfileMount {
    pub path: String,
    pub usage: MountUsage,
}

impl ProfileMount {
    pub fn rw(path: &str) -> Self {
        Self {
            path: path.into(),
            usage: MountUsage::Write,
        }
    }
    pub fn only_path(path: &str) -> Self {
        Self {
            path: path.into(),
            usage: MountUsage::OnlyPath,
        }
    }
    pub fn read_only(path: &str) -> Self {
        Self {
            path: path.into(),
            usage: MountUsage::ReadOnly,
        }
    }

    /// Resolve tilde path to an absolute host path.
    pub fn resolved_host_path(&self) -> PathBuf {
        if let Some(rest) = self.path.strip_prefix("~/")
            && let Ok(home) = std::env::var("HOME")
        {
            return PathBuf::from(home).join(rest);
        }
        PathBuf::from(&self.path)
    }

    /// Map tilde path to sandbox-internal path (`~/.cargo` → `/home/maki/.cargo`).
    pub fn sandbox_internal_path(&self) -> String {
        if let Some(rest) = self.path.strip_prefix("~/") {
            return format!("/home/maki/{rest}");
        }
        self.path.clone()
    }

    /// Derive directory name by stripping `~/` prefix (`~/.cargo` → `.cargo`).
    pub fn dir_name(&self) -> String {
        if let Some(rest) = self.path.strip_prefix("~/") {
            return rest.to_string();
        }
        self.path.clone()
    }
}

/// A named collection of directory mounts that can be toggled on/off.
#[derive(Debug, Clone)]
pub struct SandboxProfile {
    pub name: String,
    pub mounts: Vec<ProfileMount>,
}

/// Returns the built-in profiles.
pub fn builtin_profiles() -> Vec<SandboxProfile> {
    vec![
        SandboxProfile {
            name: "rust".into(),
            mounts: vec![
                ProfileMount::rw("~/.cargo"),
                ProfileMount::only_path("~/.cargo/bin"),
                ProfileMount::read_only("~/.rustup"),
            ],
        },
        SandboxProfile {
            name: "java".into(),
            mounts: vec![ProfileMount::rw("~/.m2"), ProfileMount::rw("~/.gradle")],
        },
        SandboxProfile {
            name: "node".into(),
            mounts: vec![
                ProfileMount::rw("~/.npm"),
                ProfileMount::rw("~/.yarn"),
                ProfileMount::only_path("~/.npm/bin"),
            ],
        },
        SandboxProfile {
            name: "go".into(),
            mounts: vec![
                ProfileMount::rw("~/go"),
                ProfileMount::only_path("~/go/bin"),
            ],
        },
    ]
}

/// Build a [`NamespaceConfig`] from profiles.
///
/// Each profile contributes mounts and PATH entries. `extra_home_mounts`
/// are additional host paths to bind-mount (e.g. from the UI info struct).
pub fn build_namespace_config(
    profiles: &[SandboxProfile],
    workspace_dir: PathBuf,
    workspace_name: String,
    extra_home_mounts: Vec<(PathBuf, String)>,
    extra_workspace_dirs: Vec<(PathBuf, String)>,
) -> NamespaceConfig {
    let mut home_mounts = extra_home_mounts;
    let mut readonly_mounts: Vec<(PathBuf, String)> = Vec::new();
    let mut path_dirs: Vec<String> = Vec::new();
    let mut symlinks: Vec<(PathBuf, String)> = Vec::new();

    for profile in profiles {
        for mount in &profile.mounts {
            let path = mount.resolved_host_path();
            let name = mount.dir_name();
            match mount.usage {
                MountUsage::Write => home_mounts.push((path, name)),
                MountUsage::ReadOnly => readonly_mounts.push((path, name)),
                MountUsage::OnlyPath => path_dirs.push(mount.sandbox_internal_path()),
                MountUsage::SymLink => symlinks.push((path, name)),
            }
        }
    }

    NamespaceConfig::new(
        vec![],
        vec![],
        workspace_dir,
        workspace_name,
        home_mounts,
        readonly_mounts,
        path_dirs,
        extra_workspace_dirs,
        symlinks,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    const SANDBOX_HOME: &str = "/home/maki";

    #[test_case("~/.cargo", ".cargo" ; "tilde cargo")]
    #[test_case("~/go", "go" ; "tilde go")]
    #[test_case("~/.npm/bin", ".npm/bin" ; "tilde npm bin")]
    fn dir_name_strips_tilde_prefix(input: &str, expected: &str) {
        let mount = ProfileMount::rw(input);
        assert_eq!(mount.dir_name(), expected);
    }

    #[test_case("/usr/local" ; "absolute path")]
    #[test_case("relative/path" ; "relative path")]
    fn dir_name_passthrough_without_tilde(input: &str) {
        let mount = ProfileMount::rw(input);
        assert_eq!(mount.dir_name(), input);
    }

    #[test_case("~/.cargo", "/home/maki/.cargo" ; "tilde cargo")]
    #[test_case("~/go/bin", "/home/maki/go/bin" ; "tilde go bin")]
    #[test_case("~/.npm/bin", "/home/maki/.npm/bin" ; "tilde npm bin")]
    fn sandbox_internal_path_maps_tilde(input: &str, expected: &str) {
        let mount = ProfileMount::rw(input);
        assert_eq!(mount.sandbox_internal_path(), expected);
    }

    #[test_case("/usr/local" ; "absolute")]
    #[test_case("relative" ; "relative")]
    fn sandbox_internal_path_passthrough_without_tilde(input: &str) {
        let mount = ProfileMount::rw(input);
        assert_eq!(mount.sandbox_internal_path(), input);
    }

    #[test]
    fn resolved_host_path_expands_to_home() {
        let home = std::env::var("HOME").unwrap();
        let home = PathBuf::from(home);
        assert_eq!(
            ProfileMount::rw("~/.cargo").resolved_host_path(),
            home.join(".cargo")
        );
        assert_eq!(
            ProfileMount::rw("~/go/bin").resolved_host_path(),
            home.join("go/bin")
        );
    }

    #[test]
    fn resolved_host_path_passthrough_without_tilde() {
        assert_eq!(
            ProfileMount::rw("/usr/local").resolved_host_path(),
            PathBuf::from("/usr/local")
        );
        assert_eq!(
            ProfileMount::rw("relative").resolved_host_path(),
            PathBuf::from("relative")
        );
    }

    #[test]
    fn builtin_profiles_have_expected_names() {
        let profiles = builtin_profiles();
        let names: Vec<&str> = profiles.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["rust", "java", "node", "go"]);
    }

    #[test]
    fn builtin_profiles_rust_has_cargo_mounts() {
        let profiles = builtin_profiles();
        let rust = profiles.into_iter().find(|p| p.name == "rust").unwrap();
        assert_eq!(rust.mounts.len(), 3);
        assert_eq!(rust.mounts[0].path, "~/.cargo");
        assert_eq!(rust.mounts[0].usage, MountUsage::Write);
        assert_eq!(rust.mounts[1].path, "~/.cargo/bin");
        assert_eq!(rust.mounts[1].usage, MountUsage::OnlyPath);
        assert_eq!(rust.mounts[2].path, "~/.rustup");
        assert_eq!(rust.mounts[2].usage, MountUsage::ReadOnly);
    }

    #[test]
    fn mount_usage_label() {
        assert_eq!(MountUsage::Write.label(), "rw");
        assert_eq!(MountUsage::ReadOnly.label(), "ro");
        assert_eq!(MountUsage::OnlyPath.label(), "on $PATH");
    }

    #[test]
    fn build_namespace_config_enabled_profiles() {
        let profiles = builtin_profiles();
        let rust: Vec<SandboxProfile> = profiles.into_iter().filter(|p| p.name == "rust").collect();
        let config =
            build_namespace_config(&rust, "/workspace".into(), "test".into(), vec![], vec![]);
        assert_eq!(config.home_mounts.len(), 1);
        assert_eq!(config.home_mounts[0].1, ".cargo");
        assert_eq!(config.readonly_mounts.len(), 1);
        assert_eq!(config.readonly_mounts[0].1, ".rustup");
        assert_eq!(config.path_dirs.len(), 1);
        assert_eq!(config.path_dirs[0], format!("{SANDBOX_HOME}/.cargo/bin"));
        assert_eq!(config.workspace_dir, PathBuf::from("/workspace"));
    }

    #[test]
    fn build_namespace_config_multiple_profiles() {
        let profiles = builtin_profiles();
        let selected: Vec<SandboxProfile> = profiles
            .into_iter()
            .filter(|p| p.name == "rust" || p.name == "node")
            .collect();
        let config = build_namespace_config(&selected, "/ws".into(), "ws".into(), vec![], vec![]);
        // Rust: ~/.cargo (Write), ~/.cargo/bin (OnlyPath), ~/.rustup (ReadOnly)
        // Node: ~/.npm (Write), ~/.yarn (Write), ~/.npm/bin (OnlyPath)
        assert_eq!(config.home_mounts.len(), 3); // .cargo, .npm, .yarn
        assert_eq!(config.readonly_mounts.len(), 1); // .rustup
        assert_eq!(config.path_dirs.len(), 2); // .cargo/bin, .npm/bin
    }

    #[test]
    fn build_namespace_config_with_extra_mounts() {
        let config = build_namespace_config(
            &[],
            "/ws".into(),
            "ws".into(),
            vec![(PathBuf::from("/host/dir"), "dir".into())],
            vec![],
        );
        assert_eq!(config.home_mounts.len(), 1);
        assert_eq!(config.home_mounts[0].0, PathBuf::from("/host/dir"));
        assert_eq!(config.home_mounts[0].1, "dir");
    }
}
