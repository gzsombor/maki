use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use futures::future::join_all;
use maki_agent::tools::interpreter_bridge::build_tool_input;
use maki_interpreter::PendingCall;
use maki_interpreter::runner::InterpreterResult;
use maki_lua::{json_to_lua, lua_tool_result};
use maki_sandbox::ToolDispatcher;
use maki_sandbox::ipc::SetupMessage;
use maki_sandbox::run_child_io;
use mlua::{Function, Lua};
use serde_json::Value as JsonValue;

type CallResults = Vec<(u32, Result<JsonValue, String>)>;

enum BridgeMsg {
    Calls(Vec<PendingCall>, flume::Sender<CallResults>),
}

struct BridgeDispatcher {
    tx: flume::Sender<BridgeMsg>,
}

impl ToolDispatcher for BridgeDispatcher {
    fn dispatch(
        &self,
        name: &str,
        args: Vec<JsonValue>,
        kwargs: Vec<(String, JsonValue)>,
    ) -> Result<String, String> {
        let call = PendingCall {
            call_id: 0,
            name: name.to_owned(),
            args,
            kwargs,
        };
        let (reply_tx, reply_rx) = flume::bounded(1);
        let _ = self.tx.send(BridgeMsg::Calls(vec![call], reply_tx));
        let result: Option<JsonValue> = reply_rx
            .recv()
            .map_err(|e| e.to_string())?
            .into_iter()
            .next()
            .and_then(|(_, r)| r.ok());
        result
            .map(|v| v.to_string())
            .ok_or_else(|| "no result from tool dispatch".into())
    }
}

async fn call_lua_tool(
    lua: Lua,
    f: Option<Function>,
    pc: &PendingCall,
) -> Result<JsonValue, String> {
    let Some(f) = f else {
        return Err(format!("unknown tool: {}", pc.name));
    };
    let input = build_tool_input(&pc.args, &pc.kwargs)?;
    let arg = json_to_lua(&lua, &input).map_err(|e| e.to_string())?;
    let values = f
        .call_async::<mlua::MultiValue>(arg)
        .await
        .map_err(|e| e.to_string())?;
    lua_tool_result(values)
        .map(JsonValue::String)
        .map_err(|e| format!("{}: {e}", pc.name))
}

pub(crate) async fn run_sandbox_with(
    sandbox: &Arc<maki_sandbox::Sandbox>,
    lua: Lua,
    code: String,
    timeout: Duration,
    fns: HashMap<String, Function>,
) -> Result<Result<InterpreterResult, String>, mlua::Error> {
    sandbox
        .setup(&SetupMessage {
            code,
            timeout_secs: timeout.as_secs(),
            max_memory: 0,
        })
        .map_err(|e| mlua::Error::runtime(format!("sandbox setup: {e}")))?;

    let io_sock = sandbox
        .clone_stream()
        .map_err(|e| mlua::Error::runtime(format!("sandbox clone: {e}")))?;

    let (bridge_tx, bridge_rx) = flume::unbounded::<BridgeMsg>();

    let dispatch: Arc<dyn ToolDispatcher> = Arc::new(BridgeDispatcher { tx: bridge_tx });

    let (io_handle, result_arc) = run_child_io(io_sock, dispatch, None)
        .map_err(|e| mlua::Error::runtime(format!("sandbox io thread: {e}")))?;

    let recv_loop = async {
        while let Ok(BridgeMsg::Calls(batch, reply)) = bridge_rx.recv_async().await {
            let futs = batch.into_iter().map(|pc| {
                let f = fns.get(&pc.name).cloned();
                let lua = lua.clone();
                async move { (pc.call_id, call_lua_tool(lua, f, &pc).await) }
            });
            let _ = reply.send(join_all(futs).await);
        }
    };

    let sandbox_fut = {
        let sandbox = Arc::clone(sandbox);
        smol::unblock(move || {
            let status = sandbox.wait();
            (status, result_arc)
        })
    };

    let (status, result_arc) = sandbox_fut.await;
    recv_loop.await;

    let _ = io_handle.join();

    status.map_err(|e| mlua::Error::runtime(format!("sandbox wait: {e}")))?;

    let result = result_arc
        .lock()
        .map_err(|e| mlua::Error::runtime(format!("result lock: {e}")))?
        .take()
        .ok_or_else(|| mlua::Error::runtime("sandbox did not return result"))?;

    if let Some(err) = result.error {
        return Ok(Err(err));
    }

    Ok(Ok(InterpreterResult {
        output: result.output,
        stdout: result.stdout,
    }))
}
