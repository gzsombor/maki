#![cfg(all(feature = "sandbox", target_os = "linux"))]

pub mod child;
pub mod error;
pub mod ipc;
pub mod lua_runtime;
pub mod namespace;
pub mod profiles;
pub mod sandbox;

use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};

use nix::sys::wait::{WaitStatus, waitpid};
use nix::unistd::{ForkResult, Pid, getgid, getuid};
use serde_json::Value;
use tracing::{debug, warn};

use crate::error::SandboxError;
use crate::ipc::{ChildMsg, ParentMsg, SYNC_GO, SYNC_READY, ToolResultPayload};
use crate::namespace::NamespaceConfig;

/// Acquire a mutex lock, converting poison to [`SandboxError`].
pub fn lock_or_poisoned<T>(mutex: &Mutex<T>) -> Result<std::sync::MutexGuard<'_, T>, SandboxError> {
    mutex
        .lock()
        .map_err(|e| SandboxError::MutexPoisoned(e.to_string()))
}

pub use sandbox::Sandbox;

/// Spawn a sandboxed child process.
///
/// Returns the child PID and the parent end of the IPC socket.
/// The child has already:
/// - Filtered its environment
/// - Created a user namespace (uid/gid mapped)
/// - Created a mount namespace
/// - Set up bind mounts
///
/// After this returns, the caller should:
/// 1. Send a [`SetupMessage`](crate::ipc::SetupMessage) via [`ipc::send_setup`]
/// 2. Enter the IPC loop (handle tool calls, stream stdout)
/// 3. Read the final [`ChildMsg::Done`](crate::ipc::ChildMsg::Done) via the IPC socket
pub fn spawn_child(config: &NamespaceConfig) -> Result<(Pid, UnixStream), SandboxError> {
    let (mut parent_sock, child_sock) =
        UnixStream::pair().map_err(|e| SandboxError::Ipc(format!("socketpair: {e}")))?;

    match unsafe { nix::unistd::fork() }.map_err(|e| SandboxError::Fork(e.to_string()))? {
        ForkResult::Child => {
            drop(parent_sock);
            child::child_main(child_sock, config);
        }
        ForkResult::Parent { child } => {
            drop(child_sock);

            let child_pid = child;

            crate::ipc::send_handshake(&mut parent_sock, "maki-server")?;
            let child_name = crate::ipc::recv_handshake(&mut parent_sock)?;
            if child_name != "maki-child" {
                return Err(SandboxError::Ipc(format!(
                    "unexpected child handshake: got '{child_name}', expected 'maki-child'"
                )));
            }

            crate::ipc::recv_sync(&mut parent_sock, SYNC_READY)?;

            crate::namespace::write_uid_map(child_pid, getuid().as_raw(), getgid().as_raw())?;

            crate::ipc::send_sync(&mut parent_sock, SYNC_GO)?;

            debug!(child_pid = %child_pid.as_raw(), "sandbox: child spawned");
            Ok((child_pid, parent_sock))
        }
    }
}

/// Wait for the child process to exit and collect its status.
pub fn wait_child(pid: Pid) -> Result<(), SandboxError> {
    match waitpid(pid, None) {
        Ok(WaitStatus::Exited(_, 0)) => Ok(()),
        Ok(WaitStatus::Exited(_, code)) => {
            Err(SandboxError::Ipc(format!("child exited with code {code}")))
        }
        Ok(WaitStatus::Signaled(_, sig, _)) => {
            Err(SandboxError::Ipc(format!("child killed by signal {sig}")))
        }
        Ok(status) => Err(SandboxError::Ipc(format!(
            "unexpected wait status: {status:?}"
        ))),
        Err(e) => Err(SandboxError::Ipc(format!("waitpid: {e}"))),
    }
}

/// Trait for dispatching tool calls from the sandbox child.
///
/// Implementations decide how each tool call is handled: directly
/// (e.g. calling a Lua function) or forwarded (e.g. over IPC).
pub trait ToolDispatcher: Send + Sync {
    fn dispatch(
        &self,
        name: &str,
        args: Vec<Value>,
        kwargs: Vec<(String, Value)>,
    ) -> Result<String, String>;
}

/// Callback type for streaming stdout lines from the sandbox child.
pub type StdoutCallback = dyn Fn(&str) + Send + Sync;

/// Result of running the parent-side IO loop.
pub struct ChildIoResult {
    pub output: Option<Value>,
    pub stdout: String,
    pub error: Option<String>,
}

/// Parent-side IO handler for the sandbox child process.
///
/// Runs in a dedicated thread, reading [`ChildMsg`] from the IPC socket and
/// dispatching tool calls via the provided callback. Tool results are sent
/// back as [`ParentMsg::ToolResult`].
struct ChildIoHandler;

impl ChildIoHandler {
    fn run(
        mut sock: UnixStream,
        dispatch: Arc<dyn ToolDispatcher>,
        stdout_cb: Option<Arc<StdoutCallback>>,
        result: Arc<Mutex<Option<ChildIoResult>>>,
    ) {
        loop {
            let msg = match ipc::recv_child_msg(&mut sock) {
                Ok(m) => m,
                Err(e) => {
                    warn!("sandbox parent: recv error: {e}");
                    return;
                }
            };

            match msg {
                ChildMsg::ToolCall {
                    call_id,
                    name,
                    args,
                    kwargs,
                } => {
                    let (output, error) = match dispatch.dispatch(&name, args, kwargs) {
                        Ok(o) => (Some(o), None),
                        Err(e) => (None, Some(e)),
                    };
                    if let Err(e) = ipc::send_parent_msg(
                        &mut sock,
                        &ParentMsg::ToolResult {
                            call_id,
                            result: ToolResultPayload { output, error },
                        },
                    ) {
                        warn!("sandbox parent: send tool result failed (call_id={call_id}): {e}");
                    }
                }
                ChildMsg::Stdout { text } => {
                    if let Some(cb) = &stdout_cb {
                        cb(&text);
                    }
                }
                ChildMsg::Done {
                    output,
                    stdout,
                    error,
                } => {
                    if let Ok(mut guard) = lock_or_poisoned(&result) {
                        *guard = Some(ChildIoResult {
                            output,
                            stdout,
                            error,
                        });
                    }
                    return;
                }
                _ => {}
            }
        }
    }
}

/// Result of [`run_child_io`]: a join handle and a shared result arc.
pub type ChildIoHandle = (
    std::thread::JoinHandle<()>,
    Arc<Mutex<Option<ChildIoResult>>>,
);

/// Spawn a parent-side IO thread that handles tool calls from the sandbox child.
///
/// Spawns a background thread that:
/// - Reads [`ChildMsg::ToolCall`] from the IPC socket
/// - Dispatches each call via `dispatch`
/// - Sends [`ParentMsg::ToolResult`] back to the child
/// - Streams stdout via `stdout_cb`
/// - Captures the final [`ChildMsg::Done`] result
///
/// Returns a join handle and a shared result arc.
pub fn run_child_io(
    sock: UnixStream,
    dispatch: Arc<dyn ToolDispatcher>,
    stdout_cb: Option<Arc<StdoutCallback>>,
) -> Result<ChildIoHandle, SandboxError> {
    let result = Arc::new(Mutex::new(None));
    let result_clone = Arc::clone(&result);

    let handle = std::thread::Builder::new()
        .name("sandbox-parent-io".into())
        .spawn(move || {
            ChildIoHandler::run(sock, dispatch, stdout_cb, result_clone);
        })
        .map_err(|e| SandboxError::Ipc(format!("spawn sandbox-parent-io thread: {e}")))?;

    Ok((handle, result))
}
