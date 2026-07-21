# Secure Tool Dispatch Architecture

## Overview

This document describes the security architecture of the sandbox. For the full operational reference (process model, IPC protocol, filesystem layout), see [README.md](./README.md).

## Problem

LLM-generated Python code runs tools with full host filesystem access. A `read("/etc/passwd")` or `write("/home/user/.ssh/authorized_keys", ...)` succeeds — the sandbox only isolates the interpreter's direct filesystem access, not the tools it invokes.

## Solution

Run the Lua plugin runtime **inside** the sandbox child, so filesystem tools (`read`, `write`, `edit`, `glob`, `grep`) execute within the child's mount namespace where only the workspace is visible. Only a small set of **trusted tools** that require host resources (network, UI, agent state) are forwarded to the parent over IPC.

## Architecture

```
Parent process (maki)
┌──────────────────────────────────────────────────────────────────┐
│  Sandbox struct                                                  │
│  ├── ChildIoHandler thread                                       │
│  │     receives ToolCall for TRUSTED tools only                  │
│  │     dispatches via ToolDispatcher trait to parent Lua plugins  │
│  │     sends ToolResult back                                     │
│  │                                                               │
│  ├── ToolDispatcher trait (maki-sandbox)                          │
│  │     abstracts how tool calls are handled                      │
│  │     implementations: BridgeDispatcher                         │
│  └── IPC socket ──────────────────────────────────────────────── │
└──────────────────────────────────────────────────────────────────┘
         ▲  IPC (trusted tools only)
         │
         ▼
Child process (inside user + mount namespaces)
┌──────────────────────────────────────────────────────────────────┐
│  monty Python interpreter (runs LLM-generated code)              │
│    │                                                             │
│    ▼                                                             │
│  Tool dispatch layer                                             │
│    │                                                             │
│    ├── if tool in TRUSTED_TOOLS → send ToolCall over IPC to parent│
│    │                              receive ToolResult              │
│    │                                                             │
│    └── else → call local Lua plugin via ChildLuaRuntime          │
│               ├── read, write, edit, multiedit                   │
│               ├── glob, grep                                     │
│               ├── bash (fork+execve inside namespace)            │
│               └── any future filesystem tool                     │
│                                                                  │
│  ChildLuaRuntime (maki-sandbox/src/lua_runtime.rs)               │
│    ├── mlua + monty Lua bindings                                 │
│    ├── Tool plugins embedded in binary via include_dir!          │
│    └── Sandboxed API surface (no network, no agent state)        │
│                                                                  │
│  Filesystem view (mount namespace)                               │
│    ├── /home/maki/workspace/{name}/  (rw — the workspace)        │
│    ├── /usr, /lib, /bin  (ro — system binaries)                 │
│    ├── /tmp  (tmpfs — scratch)                                   │
│    └── nothing else visible                                      │
└──────────────────────────────────────────────────────────────────┘
```

## Security properties

1. **Filesystem tools execute inside the mount namespace.** `read`, `write`, `edit`, `glob`, `grep` run via Lua plugins in the child. The mount namespace restricts them to the workspace and read-only system dirs.

2. **Parent only handles trusted tools.** The IPC channel carries only `webfetch`, `websearch`, `question`, `todo_write`, `task`, `memory`, `skill`, `index`.

3. **`bash` is already local.** Shell commands execute inside the namespace via `fork()+execve()`.

4. **Plugin extensibility.** Adding a new filesystem tool is just adding a Lua plugin to `./plugins/`. No Rust changes, no IPC changes. Plugins are embedded in the binary at compile time.

5. **Defense in depth.** Even if a Lua plugin has a bug, the mount namespace provides a second layer — host paths are simply not visible.

6. **Self-contained binary.** The sandbox binary embeds all plugin Lua sources at compile time via `include_dir!`. No host filesystem bind mount is needed for plugins, making the binary fully self-contained for distribution.

## Risks and considerations

- **Lua runtime in child adds complexity.** The child now runs both monty (Python) and mlua (Lua). Memory and startup cost increase. Mitigation: the child Lua runtime is minimal — no full plugin host, no agent, no UI.

- **API surface divergence.** The child's `maki.*` API is a subset of the parent's. Plugins that call `maki.uv.http` or `maki.agent.*` will fail inside the sandbox. This is intentional.

- **Plugin compatibility.** The child Lua plugins must work with the restricted API. Existing plugins like `read`, `write`, `edit` use `maki.fs` which is provided.
