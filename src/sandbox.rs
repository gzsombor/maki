use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use maki_agent::tools::interpreter_bridge::build_tool_input;
use maki_interpreter::runner::InterpreterResult;
use maki_sandbox::ToolDispatcher;
use mlua::{Function, Lua, Value as LuaValue};
use serde_json::Value as JsonValue;

/// Tools the child executes with local Rust functions, so the parent
/// dispatcher never sees them.
const CHILD_LOCAL_TOOLS: &[&str] = &["read", "write", "edit", "multiedit", "glob", "grep", "list"];

enum BridgeMsg {
    Call {
        lua: Lua,
        func: Function,
        name: String,
        arg: LuaValue,
        reply: flume::Sender<Result<String, String>>,
    },
}

async fn call_lua_tool(
    lua: Lua,
    func: Function,
    name: String,
    arg: LuaValue,
) -> Result<String, String> {
    let thread = lua
        .create_thread(func)
        .map_err(|e| format!("{name}: {e}"))?;
    let values: mlua::MultiValue =
        thread.into_async(arg).map_err(|e| format!("{name}: {e}"))?.await
            .map_err(|e| format!("{name}: {e}"))?;
    maki_lua::lua_tool_result(values).map_err(|e| format!("{name}: {e}"))
}

pub(crate) async fn run_sandbox_with(
    sandbox: &Arc<maki_sandbox::Sandbox>,
    lua: Lua,
    code: String,
    timeout: Duration,
    fns: HashMap<String, Function>,
    config_json: String,
) -> Result<Result<InterpreterResult, String>, mlua::Error> {
    let (bridge_tx, bridge_rx) = flume::unbounded::<BridgeMsg>();

    struct BridgeDispatcher {
        sandbox: Arc<maki_sandbox::Sandbox>,
        lua: Lua,
        fns: HashMap<String, Function>,
        tx: flume::Sender<BridgeMsg>,
    }

    impl ToolDispatcher for BridgeDispatcher {
        fn dispatch(
            &self,
            name: &str,
            args: Vec<JsonValue>,
            kwargs: Vec<(String, JsonValue)>,
        ) -> Result<String, String> {
            if CHILD_LOCAL_TOOLS.contains(&name) {
                return self
                    .sandbox
                    .call_tool(name, args, kwargs)
                    .map_err(|e| e.to_string())
                    .and_then(|r| {
                        r.error
                            .map(Err)
                            .unwrap_or_else(|| r.output.ok_or_else(|| "empty tool result".into()))
                    });
            }
            let Some(func) = self.fns.get(name).cloned() else {
                return Err(format!("unknown tool: {name}"));
            };
            let input = build_tool_input(&args, &kwargs).map_err(|e| e.to_string())?;
            let arg = maki_lua::json_to_lua(&self.lua, &input).map_err(|e| e.to_string())?;
            let (reply_tx, reply_rx) = flume::bounded(1);
            let _ = self.tx.send(BridgeMsg::Call {
                lua: self.lua.clone(),
                func,
                name: name.to_owned(),
                arg,
                reply: reply_tx,
            });
            reply_rx.recv().map_err(|e| e.to_string())?
        }
    }

    sandbox.set_dispatcher(Arc::new(BridgeDispatcher {
        sandbox: Arc::clone(sandbox),
        lua: lua.clone(),
        fns,
        tx: bridge_tx,
    }));

    let sandbox_arc = Arc::clone(sandbox);
    let result = smol::unblock(move || sandbox_arc.run_code(code, timeout.as_secs(), 0, config_json))
        .await
        .map_err(|e| mlua::Error::runtime(format!("sandbox run: {e}")))?;

    while let Ok(BridgeMsg::Call {
        lua,
        func,
        name,
        arg,
        reply,
    }) = bridge_rx.recv_async().await
    {
        let _ = reply.send(call_lua_tool(lua, func, name, arg).await);
    }

    if let Some(err) = result.error {
        return Ok(Err(err));
    }

    Ok(Ok(InterpreterResult {
        output: result.output,
        stdout: result.stdout,
    }))
}
