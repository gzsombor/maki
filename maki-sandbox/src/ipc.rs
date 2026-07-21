use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use tracing::debug;

use crate::error::SandboxError;

pub const HANDSHAKE_VERSION: u32 = 1;

#[derive(Serialize, Deserialize, Debug)]
pub struct Handshake {
    pub name: String,
    pub version: u32,
}

pub fn send_handshake(sock: &mut UnixStream, name: &str) -> Result<(), SandboxError> {
    let msg = Handshake {
        name: name.to_string(),
        version: HANDSHAKE_VERSION,
    };
    let data = serde_json::to_vec(&msg)
        .map_err(|e| SandboxError::Ipc(format!("handshake serialize: {e}")))?;
    write_message(sock, &data)
}

pub fn recv_handshake(sock: &mut UnixStream) -> Result<String, SandboxError> {
    let data = read_message(sock)?;
    let hs: Handshake = serde_json::from_slice(&data)
        .map_err(|e| SandboxError::Ipc(format!("handshake deserialize: {e}")))?;
    if hs.version != HANDSHAKE_VERSION {
        return Err(SandboxError::Ipc(format!(
            "handshake version mismatch: expected {}, got {}",
            HANDSHAKE_VERSION, hs.version
        )));
    }
    Ok(hs.name)
}

pub const SYNC_READY: &[u8] = b"ready";
pub const SYNC_GO: &[u8] = b"go";

pub fn send_sync(sock: &mut UnixStream, msg: &[u8]) -> Result<(), SandboxError> {
    sock.write_all(msg)
        .map_err(|e| SandboxError::Ipc(format!("sync send: {e}")))
}

pub fn recv_sync(sock: &mut UnixStream, expected: &[u8]) -> Result<(), SandboxError> {
    let mut buf = vec![0u8; expected.len()];
    sock.read_exact(&mut buf)
        .map_err(|e| SandboxError::Ipc(format!("sync recv: {e}")))?;
    if buf[..] != *expected {
        return Err(SandboxError::Ipc(format!(
            "unexpected sync message: expected {:?}, got {:?}",
            std::str::from_utf8(expected).unwrap_or("?"),
            std::str::from_utf8(&buf).unwrap_or("?")
        )));
    }
    Ok(())
}

#[derive(Serialize, Deserialize, Debug)]
pub struct SetupMessage {
    pub code: String,
    pub timeout_secs: u64,
    pub max_memory: usize,
}

pub fn send_setup(sock: &mut UnixStream, msg: &SetupMessage) -> Result<(), SandboxError> {
    let data =
        serde_json::to_vec(msg).map_err(|e| SandboxError::Ipc(format!("setup serialize: {e}")))?;
    debug!(pid = %std::process::id(), len = data.len(), "ipc: setup send");
    write_message(sock, &data)
}

pub fn recv_setup(sock: &mut UnixStream) -> Result<SetupMessage, SandboxError> {
    let data = read_message(sock)?;
    debug!(pid = %std::process::id(), len = data.len(), "ipc: setup recv");
    serde_json::from_slice(&data).map_err(|e| SandboxError::Ipc(format!("setup deserialize: {e}")))
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "type")]
pub enum ChildMsg {
    #[serde(rename = "stdout")]
    Stdout { text: String },
    #[serde(rename = "tool_call")]
    ToolCall {
        call_id: u32,
        name: String,
        args: Vec<Value>,
        kwargs: Vec<(String, Value)>,
    },
    #[serde(rename = "done")]
    Done {
        output: Option<Value>,
        stdout: String,
        error: Option<String>,
    },
    #[serde(rename = "ls_result")]
    LsResult { entries: Vec<DirEntry> },
    #[serde(rename = "pwd_result")]
    PwdResult { path: String },
    #[serde(rename = "cd_result")]
    CdResult,
    #[serde(rename = "exec_result")]
    ExecResult {
        output: String,
        #[serde(rename = "is_error")]
        is_error: bool,
    },
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "type")]
pub enum ParentMsg {
    #[serde(rename = "tool_result")]
    ToolResult {
        call_id: u32,
        #[serde(flatten)]
        result: ToolResultPayload,
    },
    #[serde(rename = "tool_batch_result")]
    ToolBatchResult {
        results: Vec<(u32, ToolResultPayload)>,
    },
    #[serde(rename = "cancel")]
    Cancel,
    #[serde(rename = "exit")]
    Exit,
    #[serde(rename = "ls")]
    Ls { path: String },
    #[serde(rename = "pwd")]
    Pwd,
    #[serde(rename = "cd")]
    Cd { path: String },
    #[serde(rename = "exec")]
    Exec { command: String },
}

#[derive(Serialize, Deserialize, Debug)]
pub struct ToolResultPayload {
    pub output: Option<String>,
    pub error: Option<String>,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct DirEntry {
    pub name: String,
    pub is_dir: bool,
}

fn write_message(sock: &mut UnixStream, data: &[u8]) -> Result<(), SandboxError> {
    let len: u32 = data
        .len()
        .try_into()
        .map_err(|_| SandboxError::Ipc("message too large".into()))?;
    let header = len.to_be_bytes();
    sock.write_all(&header)
        .map_err(|e| SandboxError::Ipc(format!("write header: {e}")))?;
    sock.write_all(data)
        .map_err(|e| SandboxError::Ipc(format!("write payload: {e}")))?;
    Ok(())
}

const MAX_MSG_LEN: usize = 16 * 1024 * 1024;

fn read_message(sock: &mut UnixStream) -> Result<Vec<u8>, SandboxError> {
    let mut header = [0u8; 4];
    sock.read_exact(&mut header)
        .map_err(|e| SandboxError::Ipc(format!("read header: {e}")))?;
    let len = u32::from_be_bytes(header) as usize;
    if len > MAX_MSG_LEN {
        return Err(SandboxError::Ipc(format!(
            "message too large: {len} bytes (max {MAX_MSG_LEN})"
        )));
    }
    let mut buf = vec![0u8; len];
    sock.read_exact(&mut buf)
        .map_err(|e| SandboxError::Ipc(format!("read payload: {e}")))?;
    Ok(buf)
}

/// Send a [`ChildMsg`] to the parent process over the IPC socket.
pub fn send_child_msg(sock: &mut UnixStream, msg: &ChildMsg) -> Result<(), SandboxError> {
    let label = child_msg_label(msg);
    let data = serde_json::to_vec(msg)
        .map_err(|e| SandboxError::Ipc(format!("serialize child msg: {e}")))?;
    let r = write_message(sock, &data);
    let pid = std::process::id();
    if let ChildMsg::Done { error, .. } = msg {
        debug!(pid = %pid, msg = %label, ok = r.is_ok(), error = error.as_deref().unwrap_or(""), "ipc: child send");
    } else {
        debug!(pid = %pid, msg = %label, ok = r.is_ok(), "ipc: child send");
    }
    r
}

/// Receive a [`ChildMsg`] from the parent process over the IPC socket.
pub fn recv_child_msg(sock: &mut UnixStream) -> Result<ChildMsg, SandboxError> {
    let data = read_message(sock)?;
    let msg: ChildMsg = serde_json::from_slice(&data)
        .map_err(|e| SandboxError::Ipc(format!("deserialize child msg: {e}")))?;
    debug!(pid = %std::process::id(), msg = ?msg, "ipc: child recv");
    Ok(msg)
}

/// Send a [`ParentMsg`] to the child process over the IPC socket.
pub fn send_parent_msg(sock: &mut UnixStream, msg: &ParentMsg) -> Result<(), SandboxError> {
    let label = parent_msg_label(msg);
    let data = serde_json::to_vec(msg)
        .map_err(|e| SandboxError::Ipc(format!("serialize parent msg: {e}")))?;
    let r = write_message(sock, &data);
    debug!(pid = %std::process::id(), msg = %label, ok = r.is_ok(), "ipc: parent send");
    r
}

/// Send an exit signal to the child process.
pub fn send_exit(sock: &mut UnixStream) -> Result<(), SandboxError> {
    send_parent_msg(sock, &ParentMsg::Exit)
}

/// Receive a [`ParentMsg`] from the child process over the IPC socket.
pub fn recv_parent_msg(sock: &mut UnixStream) -> Result<ParentMsg, SandboxError> {
    let data = read_message(sock)?;
    let msg: ParentMsg = serde_json::from_slice(&data)
        .map_err(|e| SandboxError::Ipc(format!("deserialize parent msg: {e}")))?;
    debug!(pid = %std::process::id(), msg = ?msg, "ipc: parent recv");
    Ok(msg)
}

/// Send a filesystem query to the child and await its response.
/// Used in the pre-interpreter query phase.
pub fn query_ls(sock: &mut UnixStream, path: &str) -> Result<Vec<DirEntry>, SandboxError> {
    send_parent_msg(sock, &ParentMsg::Ls { path: path.into() })?;
    let msg = recv_child_msg(sock)?;
    match msg {
        ChildMsg::LsResult { entries } => Ok(entries),
        other => Err(SandboxError::Ipc(format!(
            "expected LsResult, got {:?}",
            serde_json::to_string(&other).unwrap_or_default()
        ))),
    }
}

/// Query the child's current working directory.
pub fn query_pwd(sock: &mut UnixStream) -> Result<String, SandboxError> {
    send_parent_msg(sock, &ParentMsg::Pwd)?;
    let msg = recv_child_msg(sock)?;
    match msg {
        ChildMsg::PwdResult { path } => Ok(path),
        other => Err(SandboxError::Ipc(format!(
            "expected PwdResult, got {:?}",
            serde_json::to_string(&other).unwrap_or_default()
        ))),
    }
}

/// Change the child's working directory.
pub fn query_cd(sock: &mut UnixStream, path: &str) -> Result<(), SandboxError> {
    send_parent_msg(sock, &ParentMsg::Cd { path: path.into() })?;
    let msg = recv_child_msg(sock)?;
    match msg {
        ChildMsg::CdResult => Ok(()),
        other => Err(SandboxError::Ipc(format!(
            "expected CdResult, got {:?}",
            serde_json::to_string(&other).unwrap_or_default()
        ))),
    }
}

/// Send an exec command to the child and await its response.
/// Used in the pre-interpreter query phase (browse/shell mode).
/// Execute a shell command in the child and return (output, is_error).
pub fn query_exec(sock: &mut UnixStream, command: &str) -> Result<(String, bool), SandboxError> {
    send_parent_msg(
        sock,
        &ParentMsg::Exec {
            command: command.into(),
        },
    )?;
    let msg = recv_child_msg(sock)?;
    match msg {
        ChildMsg::ExecResult { output, is_error } => Ok((output, is_error)),
        other => Err(SandboxError::Ipc(format!(
            "expected ExecResult, got {:?}",
            serde_json::to_string(&other).unwrap_or_default()
        ))),
    }
}

fn child_msg_label(msg: &ChildMsg) -> &'static str {
    match msg {
        ChildMsg::Stdout { .. } => "stdout",
        ChildMsg::ToolCall { .. } => "tool_call",
        ChildMsg::Done { .. } => "done",
        ChildMsg::LsResult { .. } => "ls_result",
        ChildMsg::PwdResult { .. } => "pwd_result",
        ChildMsg::CdResult => "cd_result",
        ChildMsg::ExecResult { .. } => "exec_result",
    }
}

fn parent_msg_label(msg: &ParentMsg) -> &'static str {
    match msg {
        ParentMsg::ToolResult { .. } => "tool_result",
        ParentMsg::ToolBatchResult { .. } => "tool_batch_result",
        ParentMsg::Cancel => "cancel",
        ParentMsg::Exit => "exit",
        ParentMsg::Ls { .. } => "ls",
        ParentMsg::Pwd => "pwd",
        ParentMsg::Cd { .. } => "cd",
        ParentMsg::Exec { .. } => "exec",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixStream;

    fn pair() -> (UnixStream, UnixStream) {
        UnixStream::pair().unwrap()
    }

    #[test]
    fn write_read_message_roundtrip() {
        let (mut tx, mut rx) = pair();
        let data = b"hello world";
        write_message(&mut tx, data).unwrap();
        let got = read_message(&mut rx).unwrap();
        assert_eq!(got, data);
    }

    #[test]
    fn write_read_empty_message() {
        let (mut tx, mut rx) = pair();
        write_message(&mut tx, b"").unwrap();
        let got = read_message(&mut rx).unwrap();
        assert!(got.is_empty());
    }

    #[test]
    fn read_message_rejects_oversized() {
        let (mut tx, mut rx) = pair();
        let len = (MAX_MSG_LEN as u32 + 1).to_be_bytes();
        tx.write_all(&len).unwrap();
        let err = read_message(&mut rx).unwrap_err();
        assert!(err.to_string().contains("message too large"));
    }

    #[test]
    fn handshake_roundtrip() {
        let (mut tx, mut rx) = pair();
        send_handshake(&mut tx, "maki-server").unwrap();
        let name = recv_handshake(&mut rx).unwrap();
        assert_eq!(name, "maki-server");
    }

    #[test]
    fn handshake_version_mismatch() {
        let (mut tx, mut rx) = pair();
        let hs = Handshake {
            name: "bad".into(),
            version: 999,
        };
        let data = serde_json::to_vec(&hs).unwrap();
        write_message(&mut tx, &data).unwrap();
        let err = recv_handshake(&mut rx).unwrap_err();
        assert!(err.to_string().contains("version mismatch"));
    }

    #[test]
    fn sync_roundtrip() {
        let (mut tx, mut rx) = pair();
        send_sync(&mut tx, SYNC_READY).unwrap();
        recv_sync(&mut rx, SYNC_READY).unwrap();
    }

    #[test]
    fn sync_wrong_message() {
        let (mut tx, mut rx) = pair();
        tx.write_all(SYNC_READY).unwrap();
        let err = recv_sync(&mut rx, SYNC_GO).unwrap_err();
        assert!(err.to_string().contains("unexpected sync"));
    }

    #[test]
    fn setup_roundtrip() {
        let (mut tx, mut rx) = pair();
        let msg = SetupMessage {
            code: "print('hello')".into(),
            timeout_secs: 30,
            max_memory: 1024,
        };
        send_setup(&mut tx, &msg).unwrap();
        let got = recv_setup(&mut rx).unwrap();
        assert_eq!(got.code, "print('hello')");
        assert_eq!(got.timeout_secs, 30);
        assert_eq!(got.max_memory, 1024);
    }

    #[test]
    fn child_msg_stdout_roundtrip() {
        let (mut tx, mut rx) = pair();
        let msg = ChildMsg::Stdout {
            text: "line1\n".into(),
        };
        send_child_msg(&mut tx, &msg).unwrap();
        let got = recv_child_msg(&mut rx).unwrap();
        match got {
            ChildMsg::Stdout { text } => assert_eq!(text, "line1\n"),
            other => panic!("expected Stdout, got {other:?}"),
        }
    }

    #[test]
    fn child_msg_tool_call_roundtrip() {
        let (mut tx, mut rx) = pair();
        let msg = ChildMsg::ToolCall {
            call_id: 42,
            name: "read".into(),
            args: vec![Value::String("/foo".into())],
            kwargs: vec![],
        };
        send_child_msg(&mut tx, &msg).unwrap();
        let got = recv_child_msg(&mut rx).unwrap();
        match got {
            ChildMsg::ToolCall { call_id, name, .. } => {
                assert_eq!(call_id, 42);
                assert_eq!(name, "read");
            }
            other => panic!("expected ToolCall, got {other:?}"),
        }
    }

    #[test]
    fn child_msg_done_roundtrip() {
        let (mut tx, mut rx) = pair();
        let msg = ChildMsg::Done {
            output: Some(Value::Bool(true)),
            stdout: "out".into(),
            error: Some("err".into()),
        };
        send_child_msg(&mut tx, &msg).unwrap();
        let got = recv_child_msg(&mut rx).unwrap();
        match got {
            ChildMsg::Done {
                output,
                stdout,
                error,
            } => {
                assert_eq!(output, Some(Value::Bool(true)));
                assert_eq!(stdout, "out");
                assert_eq!(error, Some("err".into()));
            }
            other => panic!("expected Done, got {other:?}"),
        }
    }

    #[test]
    fn child_msg_cd_result_roundtrip() {
        let (mut tx, mut rx) = pair();
        send_child_msg(&mut tx, &ChildMsg::CdResult).unwrap();
        let got = recv_child_msg(&mut rx).unwrap();
        assert!(matches!(got, ChildMsg::CdResult));
    }

    #[test]
    fn child_msg_exec_result_roundtrip() {
        let (mut tx, mut rx) = pair();
        let msg = ChildMsg::ExecResult {
            output: "result".into(),
            is_error: false,
        };
        send_child_msg(&mut tx, &msg).unwrap();
        let got = recv_child_msg(&mut rx).unwrap();
        match got {
            ChildMsg::ExecResult { output, is_error } => {
                assert_eq!(output, "result");
                assert!(!is_error);
            }
            other => panic!("expected ExecResult, got {other:?}"),
        }
    }

    #[test]
    fn parent_msg_tool_result_roundtrip() {
        let (mut tx, mut rx) = pair();
        let msg = ParentMsg::ToolResult {
            call_id: 7,
            result: ToolResultPayload {
                output: Some("ok".into()),
                error: None,
            },
        };
        send_parent_msg(&mut tx, &msg).unwrap();
        let got = recv_parent_msg(&mut rx).unwrap();
        match got {
            ParentMsg::ToolResult { call_id, result } => {
                assert_eq!(call_id, 7);
                assert_eq!(result.output, Some("ok".into()));
            }
            other => panic!("expected ToolResult, got {other:?}"),
        }
    }

    #[test]
    fn parent_msg_cancel_roundtrip() {
        let (mut tx, mut rx) = pair();
        send_parent_msg(&mut tx, &ParentMsg::Cancel).unwrap();
        let got = recv_parent_msg(&mut rx).unwrap();
        assert!(matches!(got, ParentMsg::Cancel));
    }

    #[test]
    fn parent_msg_exit_roundtrip() {
        let (mut tx, mut rx) = pair();
        send_parent_msg(&mut tx, &ParentMsg::Exit).unwrap();
        let got = recv_parent_msg(&mut rx).unwrap();
        assert!(matches!(got, ParentMsg::Exit));
    }

    #[test]
    fn parent_msg_ls_roundtrip() {
        let (mut tx, mut rx) = pair();
        let msg = ParentMsg::Ls {
            path: "/tmp".into(),
        };
        send_parent_msg(&mut tx, &msg).unwrap();
        let got = recv_parent_msg(&mut rx).unwrap();
        match got {
            ParentMsg::Ls { path } => assert_eq!(path, "/tmp"),
            other => panic!("expected Ls, got {other:?}"),
        }
    }

    #[test]
    fn parent_msg_cd_roundtrip() {
        let (mut tx, mut rx) = pair();
        let msg = ParentMsg::Cd {
            path: "/home".into(),
        };
        send_parent_msg(&mut tx, &msg).unwrap();
        let got = recv_parent_msg(&mut rx).unwrap();
        match got {
            ParentMsg::Cd { path } => assert_eq!(path, "/home"),
            other => panic!("expected Cd, got {other:?}"),
        }
    }

    #[test]
    fn parent_msg_exec_roundtrip() {
        let (mut tx, mut rx) = pair();
        let msg = ParentMsg::Exec {
            command: "ls -la".into(),
        };
        send_parent_msg(&mut tx, &msg).unwrap();
        let got = recv_parent_msg(&mut rx).unwrap();
        match got {
            ParentMsg::Exec { command } => assert_eq!(command, "ls -la"),
            other => panic!("expected Exec, got {other:?}"),
        }
    }

    #[test]
    fn child_msg_label_all_variants() {
        assert_eq!(
            child_msg_label(&ChildMsg::Stdout { text: "".into() }),
            "stdout"
        );
        assert_eq!(
            child_msg_label(&ChildMsg::ToolCall {
                call_id: 0,
                name: "".into(),
                args: vec![],
                kwargs: vec![]
            }),
            "tool_call"
        );
        assert_eq!(
            child_msg_label(&ChildMsg::Done {
                output: None,
                stdout: "".into(),
                error: None
            }),
            "done"
        );
        assert_eq!(
            child_msg_label(&ChildMsg::LsResult { entries: vec![] }),
            "ls_result"
        );
        assert_eq!(
            child_msg_label(&ChildMsg::PwdResult { path: "".into() }),
            "pwd_result"
        );
        assert_eq!(child_msg_label(&ChildMsg::CdResult), "cd_result");
        assert_eq!(
            child_msg_label(&ChildMsg::ExecResult {
                output: "".into(),
                is_error: false
            }),
            "exec_result"
        );
    }

    #[test]
    fn parent_msg_label_all_variants() {
        assert_eq!(
            parent_msg_label(&ParentMsg::ToolResult {
                call_id: 0,
                result: ToolResultPayload {
                    output: None,
                    error: None
                }
            }),
            "tool_result"
        );
        assert_eq!(
            parent_msg_label(&ParentMsg::ToolBatchResult { results: vec![] }),
            "tool_batch_result"
        );
        assert_eq!(parent_msg_label(&ParentMsg::Cancel), "cancel");
        assert_eq!(parent_msg_label(&ParentMsg::Exit), "exit");
        assert_eq!(parent_msg_label(&ParentMsg::Ls { path: "".into() }), "ls");
        assert_eq!(parent_msg_label(&ParentMsg::Pwd), "pwd");
        assert_eq!(parent_msg_label(&ParentMsg::Cd { path: "".into() }), "cd");
        assert_eq!(
            parent_msg_label(&ParentMsg::Exec { command: "".into() }),
            "exec"
        );
    }
}
