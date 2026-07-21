# maki-sandbox

Linux namespace-based sandbox for running untrusted code with filesystem isolation.

## Overview

When `sandbox_enabled = true`, code execution runs inside a child process with user and mount namespaces. The child has a minimal, read-only root filesystem built from the host's `/usr`, `/lib`, and `/dev`, with only the workspace directory writable. Filesystem tools (read, write, edit, glob, grep) run inside the child via a minimal Lua runtime that loads the existing plugins. Only trusted tools (network, UI, agent state) are forwarded to the parent over IPC.

## Process model

```
 maki (parent)                     maki --sandbox-inner (child, post-exec)
 ┌─────────────────────┐           ┌──────────────────────────────────────┐
 │                     │           │                                      │
 │  Sandbox struct     │  socket   │  InnerChild                         │
 │  ├── setup()        │◄─────────►│  ├── recv SetupMessage               │
 │  ├── pwd/ls/cd/exec │           │  ├── run interpreter (or browse loop)│
 │  ├── run_child_io() │           │  ├── forward tool calls via IPC      │
 │  └── wait()         │           │  └── send Done                       │
 │                     │           │                                      │
 └─────────────────────┘           └──────────────────────────────────────┘
          │
          │ fork()
          ▼
 maki (outer child, short-lived)
 ┌─────────────────────────────┐
 │  SandboxChild               │
 │  ├── filter_env()           │
 │  ├── unshare(CLONE_NEWUSER) │
 │  ├── unshare(CLONE_NEWNS)   │
 │  ├── setup_mounts()         │
 │  ├── pivot_root()           │
 │  └── exec(/proc/self/exe    │
 │       --sandbox-inner)      │
 └─────────────────────────────┘
```

Three processes are involved:

1. **Parent** (`Sandbox` / `ChildIoHandler`) -- the main maki process. Holds the `Sandbox` struct, sends setup/queries over the socket, and runs `run_child_io` in a background thread to dispatch tool calls from the child back to Lua plugins.

2. **Outer child** (`SandboxChild`) -- a short-lived process forked by `spawn_child`. Sets up namespaces, builds the mount tree, does `pivot_root`, then execs `/proc/self/exe --sandbox-inner` so the inner instance starts with a clean process state inside the isolated filesystem. If exec fails, it falls back to running the inner loop in-place.

3. **Inner child** (`InnerChild`) -- the post-exec process that runs inside the isolated root. Receives the `SetupMessage`, runs the monty interpreter (or enters browse-only mode for file browsing/shell), and sends results back over the socket. If the code is empty (browse mode), it responds to `Ls`, `Pwd`, `Cd`, `Exec` queries.

## IPC protocol

Communication uses a Unix socket pair with length-prefixed JSON messages (4-byte big-endian length header + payload). Max message size is 16 MB.

### Startup sequence

```
Parent                          Child
  │                               │
  │──── Handshake {name,ver} ────►│
  │◄─── Handshake {name,ver} ─────│
  │                               │
  │         (child unshares user ns)
  │                               │
  │◄────── sync "ready" ─────────│
  │     (parent writes uid_map)   │
  │─────── sync "go" ────────────►│
  │         (child continues)     │
```

### Message types

**Parent -> Child** (`ParentMsg`):
- `ToolResult { call_id, result }` -- response to a tool call
- `ToolBatchResult { results }` -- batch response
- `Cancel` -- cancel pending tool calls
- `Exit` -- shut down the child
- `Ls { path }`, `Pwd`, `Cd { path }`, `Exec { command }` -- filesystem queries (browse mode)

**Child -> Parent** (`ChildMsg`):
- `Stdout { text }` -- streaming stdout line
- `ToolCall { call_id, name, args, kwargs }` -- request to run a tool
- `Done { output, stdout, error }` -- final result
- `LsResult { entries }`, `PwdResult { path }`, `CdResult`, `ExecResult { output, is_error }` -- query responses

## Filesystem layout

Inside the mount namespace, the child sees:

```
/                   tmpfs (staging root)
├── usr/            bind-mounted from host (read-only)
├── bin -> usr/bin  symlink
├── sbin -> usr/sbin symlink
├── lib/            bind-mounted from host (read-only, resolves ELF loader)
├── lib64/          tmpfs with real ld-linux copied in (breaks symlink chain)
├── etc/            tmpfs (empty, except /etc/ssl bind-mounted from host)
├── dev/            tmpfs with bind-mounted device nodes (null, zero, random, etc.)
├── proc/           procfs (or bind-mounted from host if procfs mount fails)
├── tmp/            tmpfs (scratch space)
└── home/maki/
    └── workspace/
        └── {name}/ bind-mounted from host (read-write, the working directory)
```

Host directories from profiles and `sandbox_allowed_paths` are bind-mounted under `/home/maki/`.

Plugins are embedded in the binary at compile time via `include_dir!` — no filesystem mount is needed. The `ChildLuaRuntime` loads them from the embedded static, falling back to the filesystem if the plugin directory exists (useful for development).

## Namespace isolation

- **User namespace** (`CLONE_NEWUSER`): maps the current uid/gid to root inside the child. Required for all other namespace operations.
- **Mount namespace** (`CLONE_NEWNS`): gives the child its own mount tree. If unavailable (e.g. AppArmor restrictions), the child falls back to running without filesystem isolation.

## Environment

The child's environment is wiped (`clearenv`) and rebuilt from scratch. Only these variables pass through:

- `PATH` -- rebuilt as `/usr/bin:/usr/local/bin` plus profile PATH entries
- `HOME` -- always `/home/maki`
- `USER` -- always `maki`
- Default allowed: `LANG`, `TERM`, `TMPDIR`, `RUST_LOG`
- Any `LC_*` variables from the host
- User-specified `sandbox_allowed_env` entries

## Tool dispatch

Tools split into two categories:

- **Sandbox-local tools** (`read`, `write`, `edit`, `multiedit`, `glob`, `grep`) -- run inside the child via the `ChildLuaRuntime`. The Lua plugins are embedded in the binary at compile time (via `include_dir!`), so no host filesystem mount is needed. Filesystem operations are naturally sandboxed by the mount namespace. `bash` runs inside the child via `fork()+execve()`.
- **Trusted tools** (`webfetch`, `websearch`, `question`, `todo_write`, `task`, `memory`, `skill`, `index`) -- forwarded to the parent via IPC. These require host resources (network, UI, tree-sitter grammars) not available in the sandbox.

### Child Lua Runtime

The `ChildLuaRuntime` (`lua_runtime.rs`) provides a minimal `maki.*` API surface:
- `maki.fs.*` -- filesystem operations (sandboxed by mount namespace)
- `maki.uv.*` -- cwd, os_homedir, os_getenv
- `maki.fn.*` -- synchronous process execution
- `maki.json.*` -- encode/decode
- `maki.log.*` -- structured logging
- `maki.split` -- string splitting
- `maki.ui.*` -- stubs (no terminal in sandbox)
- `maki.api.register_tool` -- tool registration

Plugins are loaded from the embedded static (`include_dir!("$CARGO_MANIFEST_DIR/../plugins")`). If a filesystem plugin directory exists at the expected path (useful for development), it is used instead. The `require()` function resolves modules from the plugin directory's `lib/` subdirectory or from the embedded sources.

## Public API

The main entry point is `Sandbox`, an `Arc`-shared handle:

```rust
let sandbox = Sandbox::new(config)?;           // fork + namespace setup
sandbox.setup(&SetupMessage { code, .. })?;    // send code to execute
let pwd = sandbox.pwd()?;                       // query child state
let entries = sandbox.ls("/home/maki")?;        // list directory
sandbox.cd("/tmp")?;                            // change child cwd
let (out, is_err) = sandbox.exec("echo hi")?;  // run shell command
sandbox.reinit(new_config)?;                    // tear down + respawn
sandbox.exit()?;                                // send exit signal
// child is waited on when Sandbox is dropped
```

All IPC is serialized through an internal mutex. `reinit` tears down the old child (sends `Exit`, waits) before spawning a new one.

## Profiles

Profiles define collections of host directories to mount inside the sandbox. Built-in profiles:

| Name   | Mounts                                         |
|--------|-------------------------------------------------|
| `rust` | `~/.cargo` (rw), `~/.cargo/bin` (PATH), `~/.rustup` (ro) |
| `java` | `~/.m2` (rw), `~/.gradle` (rw)                |
| `node` | `~/.npm` (rw), `~/.yarn` (rw), `~/.npm/bin` (PATH) |
| `go`   | `~/go` (rw), `~/go/bin` (PATH)                |

Use `profiles::build_namespace_config()` to convert enabled profiles into a `NamespaceConfig`.

## Binaries

- `sandbox-shell` -- interactive CLI for testing the sandbox. Supports `--profile`, `--exec-only`, and `--list-profiles` flags.
- `sandbox-diag` -- diagnostic tool that probes namespace support, filesystem layout, and exec behavior to diagnose why sandbox commands may fail.

## Tests

- `src/child.rs` -- unit tests for `require_str`, `list_dir_entries`
- `src/ipc.rs` -- roundtrip tests for all IPC message types
- `src/namespace.rs` -- tests for env computation, path building, linker detection
- `src/profiles.rs` -- tests for path resolution, profile-to-config conversion
- `src/sandbox.rs` -- integration tests for `Sandbox` lifecycle (require namespace support)
- `src/lua_runtime.rs` -- unit tests for `ChildLuaRuntime` (plugin loading, tool registration, fs operations)
- `tests/browse.rs` -- file browser integration test
- `tests/exec.rs` -- shell execution integration test
