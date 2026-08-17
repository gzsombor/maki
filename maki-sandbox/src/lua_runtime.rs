use std::path::{Path, PathBuf};
use std::process::Command;

use include_dir::{Dir, include_dir};
use mlua::prelude::*;
use serde_json::{Value, json};
use tracing::{debug, info, warn};

static EMBEDDED_PLUGINS: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/../plugins");

/// Minimal Lua runtime for the sandbox child.
///
/// Provides a stripped-down `maki.*` API so the existing tool plugins
/// (read, write, edit, glob, grep) can load and execute inside the
/// mount namespace.  Filesystem ops are naturally sandboxed — they only
/// see paths mounted inside the namespace.
pub struct ChildLuaRuntime {
    lua: Lua,
}

impl ChildLuaRuntime {
    pub fn new(plugin_dir: &Path) -> Result<Self, LuaError> {
        let lua = Lua::new();
        create_maki_api(&lua)?;
        setup_require(&lua, plugin_dir.to_path_buf())?;
        load_plugins(&lua, plugin_dir)?;
        Ok(Self { lua })
    }

    /// Call a registered tool by name.
    pub fn call_tool(
        &self,
        name: &str,
        args: &[Value],
        kwargs: &[(String, Value)],
    ) -> Result<(String, bool), String> {
        let globals = self.lua.globals();
        let tools: LuaTable = globals
            .get("_registered_tools")
            .map_err(|e| format!("no registered tools: {e}"))?;
        let handler: LuaFunction = tools
            .get(name)
            .map_err(|e| format!("tool '{name}' not found: {e}"))?;

        info!(name = %name, ?args, ?kwargs, "call_tool");

        let input = build_tool_input(args, kwargs);
        let input_lua = json_to_lua(&self.lua, &input).map_err(|e| e.to_string())?;

        let values: LuaMultiValue = handler
            .call(input_lua)
            .map_err(|e| format!("{name}: {e}"))?;

        info!(?values, "call_tool_result");
        extract_tool_result(values)
    }

    /// List the names of all registered tools.
    pub fn registered_tool_names(&self) -> Result<Vec<String>, String> {
        let globals = self.lua.globals();
        let tools: LuaTable = globals
            .get("_registered_tools")
            .map_err(|e| format!("no registered tools: {e}"))?;
        let mut names = Vec::new();
        for pair in tools.pairs::<String, LuaFunction>() {
            let (name, _) = pair.map_err(|e| e.to_string())?;
            names.push(name);
        }
        Ok(names)
    }
}

// ──────────────────────────────────────────────
//  maki.* API surface
// ──────────────────────────────────────────────

fn create_maki_api(lua: &Lua) -> Result<(), LuaError> {
    let maki = lua.create_table()?;

    // maki.fs
    let fs = lua.create_table()?;
    fs.set("read", lua.create_function(fs_read)?)?;
    fs.set("write", lua.create_function(fs_write)?)?;
    fs.set("metadata", lua.create_function(fs_metadata)?)?;
    fs.set("dirname", lua.create_function(fs_dirname)?)?;
    fs.set("basename", lua.create_function(fs_basename)?)?;
    fs.set("abspath", lua.create_function(fs_abspath)?)?;
    fs.set("dir", lua.create_function(fs_dir)?)?;
    fs.set("mkdir", lua.create_function(fs_mkdir)?)?;
    fs.set("rm", lua.create_function(fs_rm)?)?;
    fs.set("glob", lua.create_function(fs_glob)?)?;
    fs.set("grep", lua.create_function(fs_grep)?)?;
    maki.set("fs", fs)?;

    // maki.uv
    let uv = lua.create_table()?;
    uv.set("cwd", lua.create_function(uv_cwd)?)?;
    uv.set("os_homedir", lua.create_function(uv_os_homedir)?)?;
    uv.set("os_getenv", lua.create_function(uv_os_getenv)?)?;
    maki.set("uv", uv)?;

    // maki.log
    let log = lua.create_table()?;
    log.set("debug", lua.create_function(log_debug)?)?;
    log.set("info", lua.create_function(log_info)?)?;
    log.set("warn", lua.create_function(log_warn)?)?;
    log.set("error", lua.create_function(log_error)?)?;
    maki.set("log", log)?;

    // maki.ui (stubs — no terminal in sandbox)
    let ui = lua.create_table()?;
    ui.set("buf", lua.create_function(ui_buf)?)?;
    ui.set("highlight", lua.create_function(ui_highlight)?)?;
    ui.set("theme_color", lua.create_function(ui_theme_color)?)?;
    ui.set("humantime", lua.create_function(ui_humantime)?)?;
    maki.set("ui", ui)?;

    // maki.api
    let api = lua.create_table()?;
    api.set("register_tool", lua.create_function(api_register_tool)?)?;
    api.set(
        "register_options",
        lua.create_function(api_register_options)?,
    )?;
    api.set("register_prompt_hint", lua.create_function(api_noop)?)?;
    api.set("register_command", lua.create_function(api_noop)?)?;
    maki.set("api", api)?;

    // maki.fn (job management — synchronous in sandbox)
    let fn_tbl = lua.create_table()?;
    fn_tbl.set("jobstart", lua.create_function(fn_jobstart)?)?;
    fn_tbl.set("jobwait", lua.create_function(fn_jobwait)?)?;
    fn_tbl.set("jobstop", lua.create_function(fn_jobstop)?)?;
    maki.set("fn", fn_tbl)?;

    // maki.treesitter (stub)
    let ts = lua.create_table()?;
    ts.set("get_parser", lua.create_function(ts_get_parser)?)?;
    ts.set("get_node_text", lua.create_function(ts_get_node_text)?)?;
    maki.set("treesitter", ts)?;

    // maki.json
    let json_tbl = lua.create_table()?;
    json_tbl.set("encode", lua.create_function(json_encode)?)?;
    json_tbl.set("decode", lua.create_function(json_decode)?)?;
    maki.set("json", json_tbl)?;

    // maki.split
    maki.set("split", lua.create_function(maki_split)?)?;

    // maki.async (run inline — no async in child)
    let async_tbl = lua.create_table()?;
    async_tbl.set("run", lua.create_function(async_run)?)?;
    maki.set("async", async_tbl)?;

    lua.globals().set("maki", maki)?;
    lua.globals()
        .set("_registered_tools", lua.create_table()?)?;
    lua.globals().set("_loaded", lua.create_table()?)?;

    Ok(())
}

// ──────────────────────────────────────────────
//  require() resolution
// ──────────────────────────────────────────────

fn setup_require(lua: &Lua, plugin_dir: PathBuf) -> Result<(), LuaError> {
    lua.globals().set(
        "require",
        lua.create_function(move |lua, name: String| resolve_require(lua, &plugin_dir, &name))?,
    )?;
    Ok(())
}

fn read_embedded_file(rel: &str) -> Option<String> {
    EMBEDDED_PLUGINS
        .get_file(rel)
        .and_then(|f| f.contents_utf8())
        .map(String::from)
}

fn resolve_require(lua: &Lua, plugin_dir: &Path, modname: &str) -> LuaResult<LuaValue> {
    let loaded: LuaTable = lua.globals().get("_loaded")?;
    if let Ok(val) = loaded.get::<LuaValue>(modname)
        && !val.is_nil()
    {
        return Ok(val);
    }

    let rel = modname.replace('.', "/");
    let candidates = [
        format!("{rel}.lua"),
        format!("lib/{rel}.lua"),
        format!("lib/{rel}/init.lua"),
    ];

    for path in &candidates {
        // Try filesystem first, then embedded
        let src = plugin_dir
            .join(path)
            .is_file()
            .then(|| std::fs::read_to_string(plugin_dir.join(path)).ok())
            .flatten()
            .or_else(|| read_embedded_file(path));

        if let Some(src) = src {
            let display_path = if plugin_dir.join(path).is_file() {
                plugin_dir.join(path).to_string_lossy().to_string()
            } else {
                path.clone()
            };

            let env = lua.create_table()?;
            // Inherit all standard globals (string, table, math, etc.)
            for pair in lua.globals().pairs::<LuaValue, LuaValue>() {
                let (k, v) = pair?;
                env.set(k, v)?;
            }
            env.set("require", lua.globals().get::<LuaFunction>("require")?)?;
            env.set("maki", lua.globals().get::<LuaValue>("maki")?)?;
            let result: LuaValue = lua
                .load(&src)
                .set_name(&display_path)
                .set_environment(env)
                .eval()
                .map_err(|e| mlua::Error::runtime(format!("require '{modname}': {e}")))?;
            loaded.set(modname, result.clone())?;
            return Ok(result);
        }
    }

    // Return nil instead of error — some optional modules may not exist
    Ok(LuaValue::Nil)
}

// ──────────────────────────────────────────────
//  Plugin loading
// ──────────────────────────────────────────────

fn load_plugins(lua: &Lua, plugin_dir: &Path) -> Result<(), LuaError> {
    // Try filesystem first, fall back to embedded
    let use_embedded = !plugin_dir.is_dir();
    if use_embedded {
        debug!("lua_runtime: plugin dir not found, using embedded plugins");
    }

    let subdirs: Vec<(String, PathBuf)> = if !use_embedded {
        std::fs::read_dir(plugin_dir)
            .map(|rd| {
                rd.flatten()
                    .filter(|e| e.file_type().map(|ft| ft.is_dir()).unwrap_or(false))
                    .map(|e| {
                        let name = e.file_name().to_string_lossy().to_string();
                        (name, e.path())
                    })
                    .collect()
            })
            .unwrap_or_default()
    } else {
        EMBEDDED_PLUGINS
            .dirs()
            .map(|d| {
                let name = d
                    .path()
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default();
                (name, PathBuf::new())
            })
            .collect()
    };

    for (name, fs_path) in &subdirs {
        // Read init.lua from filesystem or embedded
        let src = if !use_embedded {
            let init_lua = fs_path.join("init.lua");
            if !init_lua.is_file() {
                continue;
            }
            match std::fs::read_to_string(&init_lua) {
                Ok(s) => s,
                Err(e) => {
                    warn!(plugin = %name, error = %e, "lua_runtime: cannot read");
                    continue;
                }
            }
        } else if let Some(dir) = EMBEDDED_PLUGINS.get_dir(name) {
            match dir.get_file("init.lua").and_then(|f| f.contents_utf8()) {
                Some(s) => s.to_string(),
                None => continue,
            }
        } else {
            continue;
        };

        let env = lua.create_table()?;
        // Inherit all standard globals (string, table, math, etc.)
        for pair in lua.globals().pairs::<LuaValue, LuaValue>() {
            let (k, v) = pair?;
            env.set(k, v)?;
        }
        env.set("require", lua.globals().get::<LuaFunction>("require")?)?;
        env.set("maki", lua.globals().get::<LuaValue>("maki")?)?;

        if let Err(e) = lua.load(&src).set_name(name).set_environment(env).exec() {
            warn!(plugin = %name, error = %e, "lua_runtime: load failed");
        } else {
            debug!(plugin = %name, "lua_runtime: loaded");
        }
    }

    let tools: LuaTable = lua.globals().get("_registered_tools")?;
    debug!(count = tools.raw_len(), "lua_runtime: plugins loaded");
    Ok(())
}

// ──────────────────────────────────────────────
//  Tool dispatch helpers
// ──────────────────────────────────────────────

fn build_tool_input(args: &[Value], kwargs: &[(String, Value)]) -> Value {
    if let Some(first) = args.first()
        && first.is_object()
    {
        return first.clone();
    }
    if !kwargs.is_empty() {
        let mut obj = serde_json::Map::new();
        for (k, v) in kwargs {
            obj.insert(k.clone(), v.clone());
        }
        return Value::Object(obj);
    }
    json!({})
}

fn json_to_lua(lua: &Lua, value: &Value) -> LuaResult<LuaValue> {
    Ok(match value {
        Value::Null => LuaValue::Nil,
        Value::Bool(b) => LuaValue::Boolean(*b),
        Value::Number(n) => match (n.as_i64(), n.as_f64()) {
            (Some(i), _) => LuaValue::Integer(i),
            (_, Some(f)) => LuaValue::Number(f),
            _ => LuaValue::Nil,
        },
        Value::String(s) => LuaValue::String(lua.create_string(s.as_bytes())?),
        Value::Array(arr) => {
            let t = lua.create_table()?;
            for (i, v) in arr.iter().enumerate() {
                t.set(i + 1, json_to_lua(lua, v)?)?;
            }
            LuaValue::Table(t)
        }
        Value::Object(map) => {
            let t = lua.create_table()?;
            for (k, v) in map {
                t.set(k.as_str(), json_to_lua(lua, v)?)?;
            }
            LuaValue::Table(t)
        }
    })
}

/// Extract `(output, is_error)` from Lua handler return values.
///
/// Convention: `handler(input)` returns:
/// - `string` → success
/// - `nil, string` → error
/// - `table { llm_output, is_error? }` → structured result
fn extract_tool_result(values: LuaMultiValue) -> Result<(String, bool), String> {
    let mut iter = values.into_iter();
    match iter.next() {
        Some(LuaValue::String(s)) => {
            let text = s.to_str().map_err(|e| e.to_string())?.to_string();
            Ok((text, false))
        }
        Some(LuaValue::Table(t)) => {
            let output: String = t.get("llm_output").unwrap_or_default();
            let is_error: bool = t.get("is_error").unwrap_or(false);
            Ok((output, is_error))
        }
        Some(LuaValue::Nil) => match iter.next() {
            Some(LuaValue::String(err)) => {
                let msg = err.to_str().map_err(|e| e.to_string())?.to_string();
                Ok((msg, true))
            }
            _ => Ok(("tool returned nil".into(), true)),
        },
        _ => Ok(("tool returned unexpected type".into(), true)),
    }
}

// ──────────────────────────────────────────────
//  maki.fs.*  (returns (value, error))
// ──────────────────────────────────────────────

fn fs_read(lua: &Lua, path: String) -> LuaResult<LuaMultiValue> {
    match std::fs::read_to_string(&path) {
        Ok(content) => Ok(LuaMultiValue::from_vec(vec![LuaValue::String(
            lua.create_string(&content)?,
        )])),
        Err(e) => Ok(LuaMultiValue::from_vec(vec![
            LuaValue::Nil,
            LuaValue::String(lua.create_string(format!("read error: {e}").as_bytes())?),
        ])),
    }
}

fn fs_write(lua: &Lua, (path, content): (String, String)) -> LuaResult<LuaMultiValue> {
    if let Some(parent) = Path::new(&path).parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    match std::fs::write(&path, &content) {
        Ok(()) => Ok(LuaMultiValue::from_vec(vec![
            LuaValue::Boolean(true),
            LuaValue::Nil,
        ])),
        Err(e) => Ok(LuaMultiValue::from_vec(vec![
            LuaValue::Nil,
            LuaValue::String(lua.create_string(format!("write error: {e}").as_bytes())?),
        ])),
    }
}

fn fs_metadata(lua: &Lua, path: String) -> LuaResult<LuaValue> {
    match std::fs::metadata(&path) {
        Ok(meta) => {
            let t = lua.create_table()?;
            t.set("is_dir", meta.is_dir())?;
            t.set("is_file", meta.is_file())?;
            t.set("size", meta.len())?;
            Ok(LuaValue::Table(t))
        }
        Err(_) => Ok(LuaValue::Nil),
    }
}

fn fs_dirname(lua: &Lua, path: String) -> LuaResult<LuaValue> {
    match Path::new(&path).parent() {
        Some(p) => Ok(LuaValue::String(
            lua.create_string(p.to_string_lossy().as_bytes())?,
        )),
        None => Ok(LuaValue::Nil),
    }
}

fn fs_basename(lua: &Lua, path: String) -> LuaResult<LuaValue> {
    match Path::new(&path).file_name() {
        Some(name) => Ok(LuaValue::String(
            lua.create_string(name.to_string_lossy().as_bytes())?,
        )),
        None => Ok(LuaValue::Nil),
    }
}

fn fs_abspath(lua: &Lua, path: String) -> LuaResult<LuaValue> {
    let p = Path::new(&path);
    let abs = if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(p))
            .unwrap_or_else(|_| p.to_path_buf())
    };
    Ok(LuaValue::String(
        lua.create_string(abs.to_string_lossy().as_bytes())?,
    ))
}

fn fs_dir(lua: &Lua, path: String) -> LuaResult<LuaMultiValue> {
    let entries = match std::fs::read_dir(&path) {
        Ok(rd) => rd,
        Err(e) => {
            return Ok(LuaMultiValue::from_vec(vec![
                LuaValue::Nil,
                LuaValue::String(lua.create_string(format!("dir error: {e}").as_bytes())?),
            ]));
        }
    };

    let table = lua.create_table()?;
    for (idx, entry) in entries.flatten().enumerate() {
        let name = entry.file_name().to_string_lossy().to_string();
        let is_dir = entry.file_type().map(|ft| ft.is_dir()).unwrap_or(false);
        let kind = if is_dir { "directory" } else { "file" };
        let inner = lua.create_table()?;
        inner.set(1, name)?;
        inner.set(2, kind)?;
        table.set(idx + 1, inner)?;
    }
    Ok(LuaMultiValue::from_vec(vec![
        LuaValue::Table(table),
        LuaValue::Nil,
    ]))
}

fn fs_mkdir(lua: &Lua, (path, opts): (String, Option<LuaTable>)) -> LuaResult<LuaMultiValue> {
    let parents = opts
        .and_then(|o| o.get::<bool>("parents").ok())
        .unwrap_or(false);
    let result = if parents {
        std::fs::create_dir_all(&path)
    } else {
        std::fs::create_dir(&path)
    };
    match result {
        Ok(()) => Ok(LuaMultiValue::from_vec(vec![
            LuaValue::Boolean(true),
            LuaValue::Nil,
        ])),
        Err(e) => Ok(LuaMultiValue::from_vec(vec![
            LuaValue::Nil,
            LuaValue::String(lua.create_string(format!("mkdir error: {e}").as_bytes())?),
        ])),
    }
}

fn fs_rm(lua: &Lua, path: String) -> LuaResult<LuaMultiValue> {
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(LuaMultiValue::from_vec(vec![LuaValue::Boolean(true)])),
        Err(e) => Ok(LuaMultiValue::from_vec(vec![
            LuaValue::Nil,
            LuaValue::String(lua.create_string(format!("rm error: {e}").as_bytes())?),
        ])),
    }
}

fn fs_glob(lua: &Lua, (pattern, opts): (String, Option<LuaTable>)) -> LuaResult<LuaMultiValue> {
    let search_path = opts
        .as_ref()
        .and_then(|o| o.get::<String>("path").ok())
        .unwrap_or_else(|| {
            std::env::current_dir()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default()
        });
    let limit = opts
        .as_ref()
        .and_then(|o| o.get::<usize>("limit").ok())
        .unwrap_or(100);

    let root = Path::new(&search_path);
    let mut paths: Vec<String> = Vec::new();
    collect_glob_matches(root, &pattern, &mut paths, limit);

    let table = lua.create_table()?;
    for (i, p) in paths.iter().enumerate() {
        table.set(i + 1, p.as_str())?;
    }
    Ok(LuaMultiValue::from_vec(vec![
        LuaValue::Table(table),
        LuaValue::Nil,
    ]))
}

/// Simple recursive glob: split pattern on `**/`, match the last segment
/// as a suffix against files found by walking the directory tree.
fn collect_glob_matches(dir: &Path, pattern: &str, out: &mut Vec<String>, limit: usize) {
    if out.len() >= limit {
        return;
    }
    let (prefix, suffix) = match pattern.split_once("**/") {
        Some((p, s)) => (Some(p), s),
        None => (None, pattern),
    };
    let walk_root = prefix.map_or_else(|| dir.to_path_buf(), |p| dir.join(p));

    let mut stack = vec![walk_root];
    while let Some(current) = stack.pop() {
        if out.len() >= limit {
            break;
        }
        let rd = match std::fs::read_dir(&current) {
            Ok(rd) => rd,
            Err(_) => continue,
        };
        for entry in rd.flatten() {
            if out.len() >= limit {
                break;
            }
            let path = entry.path();
            if path.is_dir() {
                let name = path.file_name().unwrap_or_default();
                if name != ".git" && name != "node_modules" {
                    stack.push(path);
                }
            } else if let Some(name) = path.file_name().and_then(|n| n.to_str())
                && glob_match(suffix, name)
            {
                out.push(path.to_string_lossy().to_string());
            }
        }
    }
}

/// Simple glob pattern match: `*` matches any chars except `/`, `?` matches one char.
fn glob_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    glob_match_inner(&p, &t)
}

fn glob_match_inner(pattern: &[char], text: &[char]) -> bool {
    let mut pi = 0;
    let mut ti = 0;
    let mut star_pi = usize::MAX;
    let mut star_ti = 0;

    while ti < text.len() {
        if pi < pattern.len() && (pattern[pi] == '?' || pattern[pi] == text[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < pattern.len() && pattern[pi] == '*' {
            star_pi = pi;
            star_ti = ti;
            pi += 1;
        } else if star_pi != usize::MAX {
            pi = star_pi + 1;
            star_ti += 1;
            ti = star_ti;
        } else {
            return false;
        }
    }

    while pi < pattern.len() && pattern[pi] == '*' {
        pi += 1;
    }
    pi == pattern.len()
}

fn fs_grep(lua: &Lua, (pattern, opts): (String, Option<LuaTable>)) -> LuaResult<LuaMultiValue> {
    let _ = (pattern, opts);
    // Grep is complex — return empty for now, parent handles it as trusted tool
    Ok(LuaMultiValue::from_vec(vec![
        LuaValue::Nil,
        LuaValue::String(lua.create_string(b"grep not available in sandbox child")?),
    ]))
}

// ──────────────────────────────────────────────
//  maki.uv.*
// ──────────────────────────────────────────────

fn uv_cwd(lua: &Lua, _: ()) -> LuaResult<LuaValue> {
    match std::env::current_dir() {
        Ok(p) => Ok(LuaValue::String(
            lua.create_string(p.to_string_lossy().as_bytes())?,
        )),
        Err(_) => Ok(LuaValue::Nil),
    }
}

fn uv_os_homedir(lua: &Lua, _: ()) -> LuaResult<LuaValue> {
    match std::env::var("HOME") {
        Ok(h) => Ok(LuaValue::String(lua.create_string(h.as_bytes())?)),
        Err(_) => Ok(LuaValue::Nil),
    }
}

fn uv_os_getenv(lua: &Lua, key: String) -> LuaResult<LuaValue> {
    match std::env::var(&key) {
        Ok(val) => Ok(LuaValue::String(lua.create_string(val.as_bytes())?)),
        Err(_) => Ok(LuaValue::Nil),
    }
}

// ──────────────────────────────────────────────
//  maki.log.*
// ──────────────────────────────────────────────

fn log_debug(_: &Lua, msg: String) -> LuaResult<()> {
    tracing::debug!(msg, "lua plugin");
    Ok(())
}
fn log_info(_: &Lua, msg: String) -> LuaResult<()> {
    tracing::info!(msg, "lua plugin");
    Ok(())
}
fn log_warn(_: &Lua, msg: String) -> LuaResult<()> {
    tracing::warn!(msg, "lua plugin");
    Ok(())
}
fn log_error(_: &Lua, msg: String) -> LuaResult<()> {
    tracing::error!(msg, "lua plugin");
    Ok(())
}

// ──────────────────────────────────────────────
//  maki.ui.*  (stubs)
// ──────────────────────────────────────────────

fn ui_buf(lua: &Lua, _: ()) -> LuaResult<LuaValue> {
    let buf = lua.create_table()?;
    buf.set(
        "line",
        lua.create_function(|_, _: LuaValue| Ok(LuaValue::Nil))?,
    )?;
    buf.set(
        "on",
        lua.create_function(|_, _: (LuaValue, LuaValue)| Ok(LuaValue::Nil))?,
    )?;
    Ok(LuaValue::Table(buf))
}

fn ui_highlight(
    _: &Lua,
    (_source, _ext, _opts): (String, String, Option<LuaTable>),
) -> LuaResult<LuaValue> {
    Ok(LuaValue::Nil)
}

fn ui_theme_color(_: &Lua, _: String) -> LuaResult<LuaValue> {
    Ok(LuaValue::Nil)
}

fn ui_humantime(_: &Lua, secs: u64) -> LuaResult<String> {
    Ok(format!("{secs}s"))
}

// ──────────────────────────────────────────────
//  maki.api.*
// ──────────────────────────────────────────────

fn api_register_tool(lua: &Lua, spec: LuaTable) -> LuaResult<()> {
    let name: String = spec
        .get("name")
        .map_err(|_| mlua::Error::runtime("register_tool: missing 'name'"))?;
    let handler: LuaFunction = spec
        .get("handler")
        .map_err(|_| mlua::Error::runtime("register_tool: missing 'handler'"))?;

    let tools: LuaTable = lua.globals().get("_registered_tools")?;
    tools.set(name.as_str(), handler)?;
    Ok(())
}

fn api_register_options(_: &Lua, spec: LuaTable) -> LuaResult<LuaValue> {
    // Build a table of default values from the spec
    let result = LuaValue::Table(spec);
    Ok(result)
}

fn api_noop(_: &Lua, _: LuaValue) -> LuaResult<()> {
    Ok(())
}

// ──────────────────────────────────────────────
//  maki.fn.*  (synchronous process execution)
// ──────────────────────────────────────────────

fn fn_jobstart(lua: &Lua, (command, opts): (String, Option<LuaTable>)) -> LuaResult<LuaValue> {
    let workdir = opts.as_ref().and_then(|o| o.get::<String>("cwd").ok());

    let mut cmd = Command::new("sh");
    cmd.arg("-c").arg(&command);
    if let Some(dir) = &workdir {
        cmd.current_dir(dir);
    }
    cmd.stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    let output = cmd
        .output()
        .map_err(|e| mlua::Error::runtime(format!("jobstart: {e}")))?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let exit_code = output.status.code().unwrap_or(-1);

    let result = lua.create_table()?;
    result.set("stdout", stdout.as_ref())?;
    result.set("stderr", stderr.as_ref())?;
    result.set("exit_code", exit_code)?;
    Ok(LuaValue::Table(result))
}

fn fn_jobwait(_: &Lua, (id, _timeout): (LuaValue, Option<u64>)) -> LuaResult<LuaValue> {
    // In synchronous mode, jobstart already returned the result.
    // If id is a table (the result from jobstart), return it directly.
    Ok(id)
}

fn fn_jobstop(_: &Lua, _: LuaValue) -> LuaResult<()> {
    Ok(())
}

// ──────────────────────────────────────────────
//  maki.treesitter.*  (stubs)
// ──────────────────────────────────────────────

fn ts_get_parser(_: &Lua, (_source, _lang): (String, String)) -> LuaResult<LuaValue> {
    Ok(LuaValue::Nil)
}

fn ts_get_node_text(lua: &Lua, (_node, _src): (LuaValue, String)) -> LuaResult<LuaValue> {
    Ok(LuaValue::String(lua.create_string(b"")?))
}

// ──────────────────────────────────────────────
//  maki.json.*
// ──────────────────────────────────────────────

fn json_encode(_: &Lua, value: LuaValue) -> LuaResult<String> {
    let json_val = lua_to_json_inner(value)?;
    serde_json::to_string(&json_val).map_err(|e| mlua::Error::runtime(e.to_string()))
}

fn json_decode(lua: &Lua, text: String) -> LuaResult<LuaValue> {
    let json_val: Value = serde_json::from_str(&text)
        .map_err(|e| mlua::Error::runtime(format!("json decode: {e}")))?;
    json_to_lua(lua, &json_val)
}

fn lua_to_json_inner(value: LuaValue) -> LuaResult<Value> {
    Ok(match value {
        LuaValue::Nil => Value::Null,
        LuaValue::Boolean(b) => Value::Bool(b),
        LuaValue::Integer(i) => json!(i),
        LuaValue::Number(f) => json!(f),
        LuaValue::String(s) => Value::String(s.to_str()?.to_string()),
        LuaValue::Table(t) => {
            // Try array first, then object
            let len = t.raw_len();
            if len > 0 {
                let mut arr = Vec::with_capacity(len);
                for i in 1..=len {
                    let v: LuaValue = t.get(i)?;
                    arr.push(lua_to_json_inner(v)?);
                }
                Value::Array(arr)
            } else {
                let mut map = serde_json::Map::new();
                for pair in t.pairs::<LuaValue, LuaValue>() {
                    let (k, v) = pair?;
                    if let LuaValue::String(key) = k {
                        map.insert(key.to_str()?.to_string(), lua_to_json_inner(v)?);
                    }
                }
                Value::Object(map)
            }
        }
        _ => Value::Null,
    })
}

// ──────────────────────────────────────────────
//  maki.split
// ──────────────────────────────────────────────

fn maki_split(lua: &Lua, (text, sep): (String, String)) -> LuaResult<LuaValue> {
    let table = lua.create_table()?;
    for (i, part) in text.split(&sep).enumerate() {
        table.set(i + 1, part)?;
    }
    Ok(LuaValue::Table(table))
}

// ──────────────────────────────────────────────
//  maki.async.run  (run inline)
// ──────────────────────────────────────────────

fn async_run(_lua: &Lua, f: LuaFunction) -> LuaResult<LuaValue> {
    // Just call the function inline — no async in sandbox child
    let _: LuaValue = f.call(())?;
    Ok(LuaValue::Nil)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_plugin_dir() -> tempfile::TempDir {
        tempfile::TempDir::new().unwrap()
    }

    #[test]
    fn empty_plugin_dir_loads_cleanly() {
        let dir = tmp_plugin_dir();
        let rt = ChildLuaRuntime::new(dir.path()).unwrap();
        // No tools registered
        let err = rt.call_tool("read", &[], &[]).unwrap_err();
        assert!(err.contains("not found"));
    }

    #[test]
    fn register_tool_and_call() {
        let dir = tmp_plugin_dir();
        std::fs::create_dir(dir.path().join("echo")).unwrap();
        std::fs::write(
            dir.path().join("echo/init.lua"),
            r#"
            maki.api.register_tool({
                name = "echo",
                description = "echo tool",
                schema = { type = "object", properties = {} },
                handler = function(input)
                    return input.text or "nothing"
                end,
            })
            "#,
        )
        .unwrap();

        let rt = ChildLuaRuntime::new(dir.path()).unwrap();
        let (output, is_error) = rt
            .call_tool("echo", &[], &[("text".into(), json!("hello"))])
            .unwrap();
        assert_eq!(output, "hello");
        assert!(!is_error);
    }

    #[test]
    fn handler_returning_table() {
        let dir = tmp_plugin_dir();
        std::fs::create_dir(dir.path().join("t")).unwrap();
        std::fs::write(
            dir.path().join("t/init.lua"),
            r#"
            maki.api.register_tool({
                name = "t",
                description = "test",
                schema = { type = "object", properties = {} },
                handler = function(input)
                    return { llm_output = "ok", is_error = false }
                end,
            })
            "#,
        )
        .unwrap();

        let rt = ChildLuaRuntime::new(dir.path()).unwrap();
        let (output, is_error) = rt.call_tool("t", &[], &[]).unwrap();
        assert_eq!(output, "ok");
        assert!(!is_error);
    }

    #[test]
    fn handler_returning_error() {
        let dir = tmp_plugin_dir();
        std::fs::create_dir(dir.path().join("e")).unwrap();
        std::fs::write(
            dir.path().join("e/init.lua"),
            r#"
            maki.api.register_tool({
                name = "e",
                description = "error tool",
                schema = { type = "object", properties = {} },
                handler = function(input)
                    return nil, "something went wrong"
                end,
            })
            "#,
        )
        .unwrap();

        let rt = ChildLuaRuntime::new(dir.path()).unwrap();
        let (output, is_error) = rt.call_tool("e", &[], &[]).unwrap();
        assert_eq!(output, "something went wrong");
        assert!(is_error);
    }

    #[test]
    fn fs_read_works_in_sandbox() {
        let dir = tmp_plugin_dir();
        let file = dir.path().join("test.txt");
        std::fs::write(&file, "hello sandbox").unwrap();

        std::fs::create_dir(dir.path().join("reader")).unwrap();
        std::fs::write(
            dir.path().join("reader/init.lua"),
            r#"
            maki.api.register_tool({
                name = "reader",
                description = "read test",
                schema = { type = "object", properties = {} },
                handler = function(input)
                    local content, err = maki.fs.read(input.path)
                    if not content then
                        return { llm_output = err, is_error = true }
                    end
                    return content
                end,
            })
            "#,
        )
        .unwrap();

        let rt = ChildLuaRuntime::new(dir.path()).unwrap();
        let (output, _) = rt
            .call_tool(
                "reader",
                &[],
                &[("path".into(), json!(file.to_str().unwrap()))],
            )
            .unwrap();
        assert_eq!(output, "hello sandbox");
    }

    #[test]
    fn maki_split_works() {
        let dir = tmp_plugin_dir();
        std::fs::create_dir(dir.path().join("spliter")).unwrap();
        std::fs::write(
            dir.path().join("spliter/init.lua"),
            r#"
            maki.api.register_tool({
                name = "spliter",
                description = "split test",
                schema = { type = "object", properties = {} },
                handler = function(input)
                    local parts = maki.split("a,b,c", ",")
                    return table.concat(parts, "|")
                end,
            })
            "#,
        )
        .unwrap();

        let rt = ChildLuaRuntime::new(dir.path()).unwrap();
        let (output, _) = rt.call_tool("spliter", &[], &[]).unwrap();
        assert_eq!(output, "a|b|c");
    }
}
