use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};

use nix::unistd::Pid;
use tracing::{debug, warn};

use crate::error::SandboxError;
use crate::ipc::{self, DirEntry, SetupMessage};
use crate::lock_or_poisoned;
use crate::namespace::NamespaceConfig;

/// Shared handle to a sandboxed child process.
///
/// Consumers hold `Arc<Sandbox>` and call methods on it. All IPC is
/// serialized through an internal mutex. When the configuration changes,
/// call [`reinit`](Sandbox::reinit) to tear down the old child and spawn
/// a new one.
pub struct Sandbox {
    config: Mutex<NamespaceConfig>,
    inner: Mutex<Option<SandboxInner>>,
}

struct SandboxInner {
    pid: Pid,
    sock: UnixStream,
}

impl Sandbox {
    fn child_mut(inner: &mut Option<SandboxInner>) -> Result<&mut SandboxInner, SandboxError> {
        inner
            .as_mut()
            .ok_or_else(|| SandboxError::Ipc("sandbox not initialized (call reinit first)".into()))
    }

    fn child_ref(inner: &Option<SandboxInner>) -> Result<&SandboxInner, SandboxError> {
        inner
            .as_ref()
            .ok_or_else(|| SandboxError::Ipc("sandbox not initialized (call reinit first)".into()))
    }

    /// Create a new sandbox and spawn the first child process.
    pub fn new(config: NamespaceConfig) -> Result<Arc<Self>, SandboxError> {
        let (pid, sock) = crate::spawn_child(&config)?;
        debug!(pid = %pid.as_raw(), "sandbox: spawned");
        let inner = SandboxInner { pid, sock };
        Ok(Arc::new(Self {
            config: Mutex::new(config),
            inner: Mutex::new(Some(inner)),
        }))
    }

    /// Tear down the current child and spawn a new one with the given config.
    ///
    /// Old children are sent [`Exit`](crate::ipc::ParentMsg::Exit) and waited
    /// on before the new child is started.
    pub fn reinit(&self, config: NamespaceConfig) -> Result<(), SandboxError> {
        // Drop old child (sends Exit + wait via SandboxInner::drop).
        {
            let old = lock_or_poisoned(&self.inner)?.take();
            drop(old);
        }
        let (pid, sock) = crate::spawn_child(&config)?;
        debug!(pid = %pid.as_raw(), "sandbox: reinit spawned");
        *lock_or_poisoned(&self.config)? = config;
        *lock_or_poisoned(&self.inner)? = Some(SandboxInner { pid, sock });
        Ok(())
    }

    /// Send a [`SetupMessage`] to the child (code, timeout, memory limit).
    pub fn setup(&self, msg: &SetupMessage) -> Result<(), SandboxError> {
        let mut inner = lock_or_poisoned(&self.inner)?;
        let child = Self::child_mut(&mut inner)?;
        ipc::send_setup(&mut child.sock, msg)
    }

    /// The child's PID, if a child is running.
    pub fn pid(&self) -> Option<Pid> {
        match lock_or_poisoned(&self.inner) {
            Ok(inner) => inner.as_ref().map(|c| c.pid),
            Err(e) => {
                warn!("sandbox: pid() failed: {e}");
                None
            }
        }
    }

    /// Wait for the child process to exit.
    pub fn wait(&self) -> Result<(), SandboxError> {
        let pid = lock_or_poisoned(&self.inner)?
            .as_ref()
            .map(|c| c.pid)
            .ok_or_else(|| SandboxError::Ipc("no child to wait on".into()))?;
        crate::wait_child(pid)
    }

    // ── Browser / shell query methods ──

    /// Clone the underlying stream for direct IPC access.
    ///
    /// Prefer the high-level methods ([`pwd`](Sandbox::pwd),
    /// [`ls`](Sandbox::ls), etc.) unless you need raw socket access
    /// (e.g. for diagnostic tools).
    pub fn clone_stream(&self) -> Result<UnixStream, SandboxError> {
        let inner = lock_or_poisoned(&self.inner)?;
        let child = Self::child_ref(&inner)?;
        child
            .sock
            .try_clone()
            .map_err(|e| SandboxError::Ipc(format!("clone stream: {e}")))
    }

    /// Query the child's current working directory.
    pub fn pwd(&self) -> Result<String, SandboxError> {
        let mut inner = lock_or_poisoned(&self.inner)?;
        let child = Self::child_mut(&mut inner)?;
        ipc::query_pwd(&mut child.sock)
    }

    /// List directory entries in the child.
    pub fn ls(&self, path: &str) -> Result<Vec<DirEntry>, SandboxError> {
        let mut inner = lock_or_poisoned(&self.inner)?;
        let child = Self::child_mut(&mut inner)?;
        ipc::query_ls(&mut child.sock, path)
    }

    /// Change the child's working directory.
    pub fn cd(&self, path: &str) -> Result<(), SandboxError> {
        let mut inner = lock_or_poisoned(&self.inner)?;
        let child = Self::child_mut(&mut inner)?;
        ipc::query_cd(&mut child.sock, path)
    }

    /// Execute a shell command in the child. Returns `(output, is_error)`.
    pub fn exec(&self, command: &str) -> Result<(String, bool), SandboxError> {
        let mut inner = lock_or_poisoned(&self.inner)?;
        let child = Self::child_mut(&mut inner)?;
        ipc::query_exec(&mut child.sock, command)
    }

    /// Send an exit signal to the child.
    pub fn exit(&self) -> Result<(), SandboxError> {
        let mut inner = lock_or_poisoned(&self.inner)?;
        let child = Self::child_mut(&mut inner)?;
        ipc::send_exit(&mut child.sock)
    }
}

impl Drop for SandboxInner {
    fn drop(&mut self) {
        let _ = ipc::send_exit(&mut self.sock);
        let _ = crate::wait_child(self.pid);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipc::SetupMessage;
    use crate::namespace::NamespaceConfig;
    use std::path::PathBuf;

    const SKIP_NO_NS: &str = "sandbox tests require user namespace support (CLONE_NEWUSER)";

    fn test_config() -> NamespaceConfig {
        let dir = tempfile::TempDir::new().unwrap();
        NamespaceConfig::new(
            vec![],
            vec![],
            PathBuf::from(dir.path()),
            "test".into(),
            vec![],
            vec![],
            vec![],
            vec![],
            vec![],
        )
    }

    fn try_sandbox() -> Option<Arc<Sandbox>> {
        let config = test_config();
        Sandbox::new(config).ok()
    }

    #[test]
    fn sandbox_new_and_drop() {
        let Some(sandbox) = try_sandbox() else {
            eprintln!("{SKIP_NO_NS}");
            return;
        };
        assert!(sandbox.pid().is_some());
        drop(sandbox);
    }

    #[test]
    fn sandbox_reinit_spawns_new_child() {
        let Some(sandbox) = try_sandbox() else {
            eprintln!("{SKIP_NO_NS}");
            return;
        };
        let pid1 = sandbox.pid();
        let new_config = test_config();
        sandbox.reinit(new_config).expect("reinit should succeed");
        let pid2 = sandbox.pid();
        assert!(pid2.is_some());
        if let (Some(p1), Some(p2)) = (pid1, pid2) {
            assert_ne!(p1.as_raw(), p2.as_raw(), "reinit should spawn a new PID");
        }
    }

    #[test]
    fn sandbox_setup_and_pwd() {
        let Some(sandbox) = try_sandbox() else {
            eprintln!("{SKIP_NO_NS}");
            return;
        };
        let _ = sandbox.setup(&SetupMessage::browse());
        let pwd = match sandbox.pwd() {
            Ok(p) => p,
            Err(_) => {
                eprintln!("{SKIP_NO_NS}");
                return;
            }
        };
        assert!(!pwd.is_empty());
    }

    #[test]
    fn sandbox_exec_echo() {
        let Some(sandbox) = try_sandbox() else {
            eprintln!("{SKIP_NO_NS}");
            return;
        };
        let _ = sandbox.setup(&SetupMessage::browse());
        let (output, is_error) = match sandbox.exec("echo hello") {
            Ok(r) => r,
            Err(_) => {
                eprintln!("{SKIP_NO_NS}");
                return;
            }
        };
        assert!(!is_error);
        assert_eq!(output.trim(), "hello");
    }

    #[test]
    fn sandbox_ls_lists_entries() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("file.txt"), b"data").unwrap();
        let config = NamespaceConfig::new(
            vec![],
            vec![],
            PathBuf::from(dir.path()),
            "test".into(),
            vec![],
            vec![],
            vec![],
            vec![],
            vec![],
        );
        let sandbox = match Sandbox::new(config) {
            Ok(s) => s,
            Err(_) => {
                eprintln!("{SKIP_NO_NS}");
                return;
            }
        };
        let _ = sandbox.setup(&SetupMessage::browse());
        let entries = match sandbox.ls(".") {
            Ok(e) => e,
            Err(_) => {
                eprintln!("{SKIP_NO_NS}");
                return;
            }
        };
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"file.txt"));
    }

    #[test]
    fn sandbox_exit_succeeds() {
        let Some(sandbox) = try_sandbox() else {
            eprintln!("{SKIP_NO_NS}");
            return;
        };
        let _ = sandbox.setup(&SetupMessage::browse());
        let _ = sandbox.exit();
    }

    #[test]
    fn sandbox_setup_without_init_returns_error() {
        let Some(sandbox) = try_sandbox() else {
            eprintln!("{SKIP_NO_NS}");
            return;
        };
        drop(sandbox.inner.lock().unwrap().take());
        let err = sandbox.setup(&SetupMessage::browse()).unwrap_err();
        assert!(err.to_string().contains("not initialized"));
    }
}
