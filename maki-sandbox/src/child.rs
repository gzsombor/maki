use std::collections::HashMap;
use std::ffi::CStr;
use std::ffi::CString;
use std::os::unix::io::{AsFd, FromRawFd, IntoRawFd};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use maki_interpreter::runner::{self, ToolFn};
use nix::fcntl::{FcntlArg, FdFlag, fcntl};
use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
use nix::sys::wait::{WaitStatus, waitpid};
use nix::unistd::{ForkResult, close, dup2, execve, fork, pipe, read as nix_read};
use serde_json::Value;
use tracing::{debug, error, warn};

use crate::error::SandboxError;
use crate::ipc::{self, ChildMsg, DirEntry, NO_CALL_ID, ParentMsg, ToolResultPayload};
use crate::lua_runtime::ChildLuaRuntime;
use crate::namespace::{self, NamespaceConfig};

/// Tools that must run in the parent process (network, UI, agent state).
/// All other tools are executed by local Rust functions inside the sandbox.
const TRUSTED_TOOLS: &[&str] = &[
    "webfetch",
    "websearch",
    "question",
    "todo_write",
    "task",
    "memory",
    "skill",
    "index",
];

const ENV_SANDBOX_FD: &str = "MAKI_SANDBOX_FD";

/// Filesystem tools that execute inside the child via the Lua runtime.
/// Everything else stays Rust-native (bash) or is forwarded to the parent
/// (trusted tools), since the child's `maki.*` API is stripped down.
const CHILD_LOCAL_TOOLS: &[&str] = &["read", "write", "edit", "multiedit", "glob", "grep", "list"];

/// Upper bound for closing extraneous file descriptors in the fork child.
/// Linux kernels typically limit default FDs to 1024.
const MAX_FD_CLOSE: i32 = 1024;

/// Socket poll timeout in the child's IO thread, in milliseconds.
const IO_POLL_TIMEOUT_MS: u16 = 100;

/// Error message for trusted-tool forwards aborted by a parent cancel/exit.
const CANCELED_MSG: &str = "canceled";

type PendingMap = HashMap<u32, Sender<Result<ToolResultPayload, String>>>;

struct RemoteDispatch {
    next_id: AtomicU32,
    pending: Mutex<PendingMap>,
    outgoing: Sender<IoCommand>,
}

impl RemoteDispatch {
    fn lock_pending(&self) -> Result<std::sync::MutexGuard<'_, PendingMap>, SandboxError> {
        self.pending
            .lock()
            .map_err(|e| SandboxError::Ipc(format!("mutex poisoned: {e}")))
    }
}

enum IoCommand {
    SendChild(ChildMsg),
}

/// Sets up namespaces, then execs or enters inner loop.
struct SandboxChild {
    sock: UnixStream,
    config: NamespaceConfig,
}

impl SandboxChild {
    fn new(sock: UnixStream, config: NamespaceConfig) -> Self {
        Self { sock, config }
    }

    pub fn run(mut self) -> ! {
        let result = self.setup_sandbox();
        match result {
            Ok(true) => self.exec_inner(),
            Ok(false) => {
                warn!(
                    "sandbox child: no mount ns, skipping exec — running without filesystem isolation"
                );
                self.run_inner_no_mount_ns()
            }
            Err(e) => {
                error!("sandbox child: setup failed: {e}");
                let _ = ipc::send_child_msg(
                    &mut self.sock,
                    &ChildMsg::Done {
                        call_id: NO_CALL_ID,
                        output: None,
                        stdout: String::new(),
                        error: Some(e.to_string()),
                    },
                );
                std::process::exit(1);
            }
        }
    }

    fn setup_sandbox(&mut self) -> Result<bool, SandboxError> {
        let parent_name = ipc::recv_handshake(&mut self.sock)?;
        if parent_name != "maki-server" {
            return Err(SandboxError::Ipc(format!(
                "unexpected parent handshake: got '{parent_name}', expected 'maki-server'"
            )));
        }
        ipc::send_handshake(&mut self.sock, "maki-child")?;

        self.config.filter_env()?;
        debug!("sandbox child: env filtered");

        namespace::isolate_user_ns(&mut self.sock)?;
        debug!("sandbox child: user namespace created");

        let has_mount_ns = namespace::isolate_mount_ns()?;
        debug!(has_mount_ns, "sandbox child: mount namespace");

        self.config.setup_mounts(has_mount_ns)?;
        debug!("sandbox child: mounts set up");

        Ok(has_mount_ns)
    }

    fn exec_inner(self) -> ! {
        let fd = self.sock.into_raw_fd();
        if let Err(e) = fcntl(fd, FcntlArg::F_SETFD(FdFlag::empty())) {
            warn!(error = %e, "sandbox child: failed to clear FD_CLOEXEC");
        }
        unsafe {
            std::env::set_var(ENV_SANDBOX_FD, fd.to_string());
        }
        let child = std::process::Command::new("/proc/self/exe")
            .arg("--sandbox-inner")
            .spawn();
        match child {
            Ok(mut child) => {
                let status = child.wait();
                match status {
                    Ok(s) if s.success() => {
                        std::process::exit(0);
                    }
                    _ => {
                        warn!("sandbox child: inner exec failed, continuing in-place");
                        Self::run_inner_static()
                    }
                }
            }
            Err(e) => {
                warn!(
                    "sandbox child: spawn failed ({e}), continuing in-place inside isolated root"
                );
                Self::run_inner_static()
            }
        }
    }

    fn run_inner_no_mount_ns(self) -> ! {
        InnerChild::new(self.sock).run()
    }

    fn run_inner_static() -> ! {
        let fd: i32 = match std::env::var(ENV_SANDBOX_FD) {
            Ok(val) => match val.parse() {
                Ok(fd) => fd,
                Err(_) => {
                    eprintln!("MAKI_SANDBOX_FD must be a valid fd number, got: {val}");
                    std::process::exit(1);
                }
            },
            Err(_) => {
                eprintln!("MAKI_SANDBOX_FD must be set for sandbox inner instance");
                std::process::exit(1);
            }
        };
        unsafe {
            std::env::remove_var(ENV_SANDBOX_FD);
        }
        let sock = unsafe { UnixStream::from_raw_fd(fd) };
        InnerChild::new(sock).run()
    }
}

/// Entry point for the sandbox child's first invocation (fork child).
///
/// Sets up namespaces and mounts. When mount namespace is available, it
/// pivot_roots into the new root and execs `/proc/self/exe --sandbox-inner`
/// so the inner instance starts with a clean process state inside the
/// isolated filesystem. When mount namespace is unavailable, it calls the
/// inner loop directly (no isolation, no exec).
pub fn child_main(sock: UnixStream, ns_config: &NamespaceConfig) -> ! {
    SandboxChild::new(sock, ns_config.clone()).run()
}

/// Second invocation (post-exec) entry point.
pub fn child_inner_main() -> ! {
    SandboxChild::run_inner_static()
}

/// Work items executed by the child's worker (main) thread.
enum Work {
    Run(RunSpec),
    ToolCall {
        call_id: u32,
        name: String,
        args: Vec<Value>,
        kwargs: Vec<(String, Value)>,
    },
    Exit,
}

struct RunSpec {
    call_id: u32,
    code: String,
    timeout_secs: u64,
    max_memory: usize,
    config: String,
}

/// Runs inside the isolated filesystem after setup.
///
/// One IO thread owns the socket (all reads and all writes), while the main
/// thread executes blocking work (code runs, tool calls) so slow tools never
/// stall IPC. The tool map and Lua runtime live only on the worker thread.
struct InnerChild {
    sock: UnixStream,
}

impl InnerChild {
    fn new(sock: UnixStream) -> Self {
        Self { sock }
    }

    fn run(mut self) -> ! {
        let io_sock = match self.sock.try_clone() {
            Ok(sock) => sock,
            Err(e) => {
                error!("sandbox child: socket clone failed: {e}");
                std::process::exit(1);
            }
        };

        let (outgoing_tx, outgoing_rx) = mpsc::channel::<IoCommand>();
        let dispatch = Arc::new(RemoteDispatch {
            next_id: AtomicU32::new(1),
            pending: Mutex::new(HashMap::new()),
            outgoing: outgoing_tx.clone(),
        });
        let (work_tx, work_rx) = mpsc::channel::<Work>();

        // Load Lua plugins inside the sandbox for filesystem tools
        let plugin_dir = Path::new("/home/maki/.maki/plugins");
        let lua_runtime = match ChildLuaRuntime::new(plugin_dir, None) {
            Ok(rt) => {
                debug!("sandbox child: lua runtime initialized");
                Some(Arc::new(rt))
            }
            Err(e) => {
                warn!(error = %e, "sandbox child: lua runtime init failed, filesystem tools unavailable");
                None
            }
        };

        let mut tools = build_bash_tool();
        let setup = (|| -> Result<(), SandboxError> {
            if let Some(ref rt) = lua_runtime {
                tools.extend(build_lua_tools(Arc::clone(rt))?);
            }
            tools.extend(build_trusted_tools(Arc::clone(&dispatch))?);
            Ok(())
        })();
        if let Err(e) = setup {
            error!("sandbox child: tool setup failed: {e}");
            let _ = ipc::send_child_msg(
                &mut self.sock,
                &ChildMsg::Done {
                    call_id: NO_CALL_ID,
                    output: None,
                    stdout: String::new(),
                    error: Some(e.to_string()),
                },
            );
            std::process::exit(1);
        }

        std::thread::Builder::new()
            .name("sandbox-io".into())
            .spawn(move || IoHandler::run(io_sock, outgoing_rx, dispatch, work_tx))
            .expect("spawn sandbox-io thread");

        Self::worker_loop(work_rx, outgoing_tx, tools, lua_runtime);
        std::process::exit(0);
    }

    fn worker_loop(
        work_rx: Receiver<Work>,
        outgoing_tx: Sender<IoCommand>,
        tools: HashMap<String, ToolFn>,
        lua_runtime: Option<Arc<ChildLuaRuntime>>,
    ) {
        while let Ok(work) = work_rx.recv() {
            match work {
                Work::Run(spec) => {
                    if let Some(ref rt) = lua_runtime
                        && let Err(e) = rt.set_config(&spec.config)
                    {
                        warn!(error = %e, "sandbox child: failed to apply run config");
                    }
                    let limits =
                        runner::limits(Duration::from_secs(spec.timeout_secs), spec.max_memory);
                    debug!(
                        call_id = spec.call_id,
                        code_len = spec.code.len(),
                        "sandbox child: running code"
                    );
                    let outgoing = outgoing_tx.clone();
                    let call_id = spec.call_id;
                    let result =
                        runner::run_streaming(&spec.code, &tools, None, limits, &mut |line| {
                            let _ = outgoing.send(IoCommand::SendChild(ChildMsg::Stdout {
                                call_id,
                                text: line.to_string(),
                            }));
                        });
                    match result {
                        Ok(interp) => {
                            debug!(call_id = spec.call_id, "sandbox child: run finished");
                            let _ = outgoing_tx.send(IoCommand::SendChild(ChildMsg::Done {
                                call_id: spec.call_id,
                                output: interp.output,
                                stdout: interp.stdout,
                                error: None,
                            }));
                        }
                        Err(e) => {
                            warn!(
                                call_id = spec.call_id,
                                error = %e,
                                "sandbox child: interpreter error"
                            );
                            let _ = outgoing_tx.send(IoCommand::SendChild(ChildMsg::Done {
                                call_id: spec.call_id,
                                output: None,
                                stdout: String::new(),
                                error: Some(format!("interpreter: {e}")),
                            }));
                        }
                    }
                }
                Work::ToolCall {
                    call_id,
                    name,
                    args,
                    kwargs,
                } => {
                    let result = match tools.get(&name) {
                        Some(tool) => tool(&name, args, kwargs),
                        None => Err(format!("unknown tool: {name}")),
                    };
                    let payload = match result {
                        Ok(output) => ToolResultPayload {
                            output: Some(output.to_string()),
                            error: None,
                        },
                        Err(error) => ToolResultPayload {
                            output: None,
                            error: Some(error),
                        },
                    };
                    let _ = outgoing_tx.send(IoCommand::SendChild(ChildMsg::ToolResult {
                        call_id,
                        result: payload,
                    }));
                }
                Work::Exit => {
                    debug!(pid = %std::process::id(), "sandbox child: worker exiting");
                    break;
                }
            }
        }
    }
}

/// IO thread handler for the sandbox child.
///
/// Owns the IPC socket: every read and every write on the child side
/// happens here, so inline query results and queued tool traffic share a
/// single writer.
struct IoHandler;

impl IoHandler {
    fn run(
        mut sock: UnixStream,
        outgoing_rx: Receiver<IoCommand>,
        dispatch: Arc<RemoteDispatch>,
        work_tx: Sender<Work>,
    ) {
        loop {
            if !Self::drain_outgoing(&outgoing_rx, &mut sock) {
                return;
            }

            let ready = {
                let mut pollfds = [PollFd::new(sock.as_fd(), PollFlags::POLLIN)];
                match poll(&mut pollfds, PollTimeout::from(IO_POLL_TIMEOUT_MS)) {
                    Ok(0) => PollFlags::empty(),
                    Ok(_) => pollfds[0].revents().unwrap_or(PollFlags::empty()),
                    Err(e) => {
                        error!("sandbox-io: poll error: {e}");
                        return;
                    }
                }
            };
            if !ready.contains(PollFlags::POLLIN) {
                continue;
            }

            let msg = match ipc::recv_parent_msg(&mut sock) {
                Ok(msg) => msg,
                Err(e) => {
                    error!("sandbox-io: recv error: {e}");
                    return;
                }
            };
            if !Self::handle_parent_msg(&mut sock, msg, &dispatch, &work_tx) {
                return;
            }
        }
    }

    fn drain_outgoing(outgoing_rx: &Receiver<IoCommand>, sock: &mut UnixStream) -> bool {
        loop {
            match outgoing_rx.try_recv() {
                Ok(IoCommand::SendChild(msg)) => {
                    if let Err(e) = ipc::send_child_msg(sock, &msg) {
                        error!("sandbox-io: send error: {e}");
                        return false;
                    }
                }
                Err(mpsc::TryRecvError::Empty) => return true,
                Err(mpsc::TryRecvError::Disconnected) => return false,
            }
        }
    }

    fn handle_parent_msg(
        sock: &mut UnixStream,
        msg: ParentMsg,
        dispatch: &RemoteDispatch,
        work_tx: &Sender<Work>,
    ) -> bool {
        match msg {
            ParentMsg::Run {
                call_id,
                code,
                timeout_secs,
                max_memory,
                config,
            } => work_tx
                .send(Work::Run(RunSpec {
                    call_id,
                    code,
                    timeout_secs,
                    max_memory,
                    config,
                }))
                .is_ok(),
            ParentMsg::ToolCall {
                call_id,
                name,
                args,
                kwargs,
            } => work_tx
                .send(Work::ToolCall {
                    call_id,
                    name,
                    args,
                    kwargs,
                })
                .is_ok(),
            ParentMsg::ToolResult { call_id, result } => {
                let _ = dispatch.lock_pending().map(|mut pending| {
                    pending.remove(&call_id).map(|tx| {
                        let _ = tx.send(Ok(result));
                    })
                });
                true
            }
            ParentMsg::Cancel => {
                Self::cancel_pending(dispatch);
                true
            }
            ParentMsg::Exec { call_id, command } => match sandbox_exec(&command, None) {
                Ok((output, is_error)) => ipc::send_child_msg(
                    sock,
                    &ChildMsg::ExecResult {
                        call_id,
                        output,
                        is_error,
                    },
                )
                .is_ok(),
                Err(e) => ipc::send_child_msg(
                    sock,
                    &ChildMsg::ExecResult {
                        call_id,
                        output: e.to_string(),
                        is_error: true,
                    },
                )
                .is_ok(),
            },
            ParentMsg::Ls { call_id, path } => ipc::send_child_msg(
                sock,
                &ChildMsg::LsResult {
                    call_id,
                    entries: list_dir_entries(&path),
                },
            )
            .is_ok(),
            ParentMsg::Pwd { call_id } => {
                let path = std::env::current_dir()
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or_default();
                ipc::send_child_msg(sock, &ChildMsg::PwdResult { call_id, path }).is_ok()
            }
            ParentMsg::Cd { call_id, path } => match std::env::set_current_dir(&path) {
                Ok(()) => ipc::send_child_msg(sock, &ChildMsg::CdResult { call_id }).is_ok(),
                Err(e) => {
                    warn!(path = %path, error = %e, "sandbox child: cd failed");
                    ipc::send_child_msg(
                        sock,
                        &ChildMsg::ExecResult {
                            call_id,
                            output: format!("cd failed: {e}"),
                            is_error: true,
                        },
                    )
                    .is_ok()
                }
            },
            ParentMsg::Exit => {
                Self::cancel_pending(dispatch);
                let _ = work_tx.send(Work::Exit);
                false
            }
        }
    }

    fn cancel_pending(dispatch: &RemoteDispatch) {
        if let Ok(mut pending) = dispatch.lock_pending() {
            for tx in pending.drain().map(|(_, tx)| tx) {
                let _ = tx.send(Err(CANCELED_MSG.into()));
            }
        }
    }
}

fn build_bash_tool() -> HashMap<String, ToolFn> {
    let mut tools: HashMap<String, ToolFn> = HashMap::new();
    tools.insert(
        "bash".into(),
        Box::new(|_: &str, args: Vec<Value>, kwargs: Vec<(String, Value)>| {
            let command = require_str(&args, &kwargs, "command")?;
            let workdir = kwargs
                .iter()
                .find(|(k, _)| k == "workdir")
                .and_then(|(_, v)| v.as_str())
                .or_else(|| {
                    args.first()
                        .and_then(|a| a.get("workdir"))
                        .and_then(|v| v.as_str())
                });
            match sandbox_exec(&command, workdir) {
                Ok((output, _is_error)) => Ok(Value::String(output)),
                Err(e) => Err(format!("bash failed: {e}")),
            }
        }),
    );
    tools
}

/// Build tool functions from the child-side Lua runtime.
///
/// Each registered Lua plugin becomes a `ToolFn` that calls into the
/// `ChildLuaRuntime`. The Lua plugins run inside the mount namespace,
/// so filesystem operations are naturally sandboxed.
fn build_lua_tools(runtime: Arc<ChildLuaRuntime>) -> Result<HashMap<String, ToolFn>, SandboxError> {
    let mut tools = HashMap::new();

    // Get the list of registered tool names from the Lua runtime
    let names = runtime
        .registered_tool_names()
        .map_err(|e| SandboxError::Ipc(format!("lua_runtime: cannot list tools: {e}")))?;

    for name in names {
        if !CHILD_LOCAL_TOOLS.contains(&name.as_str()) {
            debug!(tool = %name, "build_lua_tools: not child-local, skipping");
            continue;
        }
        let rt = Arc::clone(&runtime);
        let tool_name = name.clone();
        tools.insert(
            name,
            Box::new(
                move |_fn_name: &str, args: Vec<Value>, kwargs: Vec<(String, Value)>| match rt
                    .call_tool(&tool_name, &args, &kwargs)
                {
                    Ok((output, is_error)) => {
                        if is_error {
                            Err(output)
                        } else {
                            Ok(Value::String(output))
                        }
                    }
                    Err(e) => Err(e),
                },
            ) as ToolFn,
        );
    }

    Ok(tools)
}

fn build_trusted_tools(
    dispatch: Arc<RemoteDispatch>,
) -> Result<HashMap<String, ToolFn>, SandboxError> {
    let mut tools: HashMap<String, ToolFn> = HashMap::new();
    for name in TRUSTED_TOOLS {
        let d = Arc::clone(&dispatch);
        tools.insert(
            name.to_string(),
            Box::new(move |fn_name: &str, args, kwargs| {
                let call_id = d.next_id.fetch_add(1, Ordering::SeqCst);
                let (tx, rx) = mpsc::channel();

                // Single lock scope: insert, send, then handle response.
                {
                    let mut pending = d.lock_pending().map_err(|e| e.to_string())?;
                    pending.insert(call_id, tx);

                    if d.outgoing
                        .send(IoCommand::SendChild(ChildMsg::ToolCall {
                            call_id,
                            name: fn_name.to_string(),
                            args,
                            kwargs,
                        }))
                        .is_err()
                    {
                        pending.remove(&call_id);
                        return Err("io thread disconnected".into());
                    }
                }

                match rx.recv() {
                    Ok(Ok(payload)) => {
                        d.lock_pending()
                            .map_err(|e| e.to_string())?
                            .remove(&call_id);
                        if let Some(err) = payload.error {
                            Err(err)
                        } else {
                            Ok(Value::String(payload.output.unwrap_or_default()))
                        }
                    }
                    Ok(Err(err)) => {
                        d.lock_pending()
                            .map_err(|e| e.to_string())?
                            .remove(&call_id);
                        Err(err)
                    }
                    Err(_) => {
                        d.lock_pending()
                            .map_err(|e| e.to_string())?
                            .remove(&call_id);
                        Err("io thread disconnected".into())
                    }
                }
            }),
        );
    }
    Ok(tools)
}

/// Execute a shell command via fork+execve, capturing combined stdout+stderr.
///
/// Uses raw fork/execve instead of `std::process::Command` because the latter
/// uses `posix_spawnp` which fails with ENOENT inside user+mount namespaces.
fn sandbox_exec(command: &str, workdir: Option<&str>) -> Result<(String, bool), SandboxError> {
    let (pipe_r, pipe_w) = pipe().map_err(|e| SandboxError::Exec(format!("pipe failed: {e}")))?;
    let pipe_r = pipe_r.into_raw_fd();
    let pipe_w = pipe_w.into_raw_fd();

    let child = match unsafe { fork() } {
        Ok(ForkResult::Child) => {
            // ── Child: redirect output and exec ──
            let _ = close(pipe_r);
            // Close all extraneous fds
            for fd in 3..MAX_FD_CLOSE {
                if fd != pipe_w {
                    let _ = close(fd);
                }
            }
            let _ = dup2(pipe_w, 1); // stdout -> pipe
            let _ = dup2(pipe_w, 2); // stderr -> pipe
            if let Ok(devnull) = std::fs::File::open("/dev/null") {
                let fd = devnull.into_raw_fd();
                let _ = dup2(fd, 0); // stdin -> /dev/null
                let _ = close(fd);
            }
            let _ = close(pipe_w);

            if let Some(dir) = workdir
                && let Err(e) = std::env::set_current_dir(dir)
            {
                eprintln!("sandbox exec: chdir to {dir} failed: {e}");
                std::process::exit(126);
            }

            let a2 = CString::new(command).unwrap_or_else(|_| std::process::exit(127));

            let argv = [c"sh", c"-c", a2.as_c_str()];
            let mut env_vars: Vec<CString> = Vec::new();
            for (k, v) in std::env::vars() {
                match CString::new(format!("{k}={v}")) {
                    Ok(cs) => env_vars.push(cs),
                    Err(_) => std::process::exit(127),
                }
            }
            let envp: Vec<&CStr> = env_vars.iter().map(|e| e.as_c_str()).collect();
            let _ = execve(c"/usr/bin/sh", &argv[..], &envp[..]);
            std::process::exit(127);
        }
        Ok(ForkResult::Parent { child }) => child,
        Err(e) => {
            let _ = close(pipe_r);
            let _ = close(pipe_w);
            return Err(SandboxError::Exec(format!("fork failed: {e}")));
        }
    };

    let _ = close(pipe_w);

    let mut output = String::new();
    let mut buf = [0u8; 8192];
    loop {
        match nix_read(pipe_r, &mut buf) {
            Ok(0) => break,
            Ok(n) => output.push_str(&String::from_utf8_lossy(&buf[..n])),
            Err(nix::errno::Errno::EINTR) => continue,
            Err(_) => break,
        }
    }
    let _ = close(pipe_r);

    let is_error = match waitpid(child, None) {
        Ok(WaitStatus::Exited(_, code)) => code != 0,
        Ok(_) => true,
        Err(e) => return Err(SandboxError::Exec(format!("waitpid: {e}"))),
    };
    Ok((output, is_error))
}

fn list_dir_entries(path: &str) -> Vec<DirEntry> {
    let mut entries = Vec::new();
    if let Ok(rd) = std::fs::read_dir(path) {
        for entry in rd.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            let is_dir = entry.file_type().map(|ft| ft.is_dir()).unwrap_or(false);
            entries.push(DirEntry { name, is_dir });
        }
    }
    entries.sort_by(|a, b| match (a.is_dir, b.is_dir) {
        (true, false) => std::cmp::Ordering::Less,
        (false, true) => std::cmp::Ordering::Greater,
        _ => a.name.cmp(&b.name),
    });
    entries
}

fn require_str(args: &[Value], kwargs: &[(String, Value)], name: &str) -> Result<String, String> {
    if let Some(val) = kwargs.iter().find(|(k, _)| k == name).map(|(_, v)| v) {
        return val
            .as_str()
            .map(String::from)
            .ok_or_else(|| format!("{name} must be a string"));
    }
    if let Some(first) = args.first() {
        if let Some(s) = first.as_str() {
            return Ok(s.to_string());
        }
        // LLM sends the whole input object as args[0]; unwrap it.
        if let Some(val) = first.get(name) {
            return val
                .as_str()
                .map(String::from)
                .ok_or_else(|| format!("{name} must be a string"));
        }
        return Err("first arg must be a string".to_string());
    }
    Err(format!("missing required argument: {name}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::TempDir;

    const MISSING_ARG: &str = "missing required argument";

    #[test]
    fn require_str_from_kwargs() {
        let args: Vec<Value> = vec![];
        let kwargs = vec![("path".into(), json!("/foo/bar"))];
        let result = require_str(&args, &kwargs, "path");
        assert_eq!(result.unwrap(), "/foo/bar");
    }

    #[test]
    fn require_str_from_positional() {
        let args = vec![json!("/positional")];
        let kwargs: Vec<(String, Value)> = vec![];
        let result = require_str(&args, &kwargs, "path");
        assert_eq!(result.unwrap(), "/positional");
    }

    #[test]
    fn require_str_missing_returns_error() {
        let args: Vec<Value> = vec![];
        let kwargs: Vec<(String, Value)> = vec![];
        let err = require_str(&args, &kwargs, "path").unwrap_err();
        assert!(
            err.contains(MISSING_ARG),
            "expected missing arg error, got: {err}"
        );
    }

    #[test]
    fn require_str_non_string_returns_error() {
        let args: Vec<Value> = vec![];
        let kwargs = vec![("path".into(), json!(42))];
        let err = require_str(&args, &kwargs, "path").unwrap_err();
        assert!(
            err.contains("must be a string"),
            "expected type error, got: {err}"
        );
    }

    #[test]
    fn require_str_from_object_in_args() {
        let args = vec![json!({"command": "ls -la", "workdir": "/tmp"})];
        let kwargs: Vec<(String, Value)> = vec![];
        assert_eq!(require_str(&args, &kwargs, "command").unwrap(), "ls -la");
    }

    #[test]
    fn require_str_object_missing_key_errors() {
        let args = vec![json!({"other": "x"})];
        let kwargs: Vec<(String, Value)> = vec![];
        assert!(require_str(&args, &kwargs, "command").is_err());
    }

    #[test]
    fn require_str_kwargs_takes_priority() {
        let args = vec![json!("/positional")];
        let kwargs = vec![("path".into(), json!("/from_kwargs"))];
        let result = require_str(&args, &kwargs, "path");
        assert_eq!(result.unwrap(), "/from_kwargs");
    }

    #[test]
    fn list_dir_entries_dirs_first_then_alpha() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        std::fs::create_dir(root.join("z_dir")).unwrap();
        std::fs::create_dir(root.join("a_dir")).unwrap();
        std::fs::write(root.join("m_file.txt"), "x").unwrap();
        std::fs::write(root.join("a_file.txt"), "y").unwrap();

        let entries = list_dir_entries(&root.to_string_lossy());
        assert_eq!(entries.len(), 4);
        assert!(entries[0].is_dir);
        assert_eq!(entries[0].name, "a_dir");
        assert!(entries[1].is_dir);
        assert_eq!(entries[1].name, "z_dir");
        assert!(!entries[2].is_dir);
        assert_eq!(entries[2].name, "a_file.txt");
        assert!(!entries[3].is_dir);
        assert_eq!(entries[3].name, "m_file.txt");
    }

    #[test]
    fn list_dir_entries_nonexistent_returns_empty() {
        let entries = list_dir_entries("/nonexistent/path/that/does/not/exist");
        assert!(entries.is_empty());
    }
}
