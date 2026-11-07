# YunqiAgent

YunqiAgent is an open-source, self-hosted coding agent with a Rust execution core and a customizable Windows desktop client. It connects to an agent host over SSH, streams model output, runs tools, and keeps task-relevant memory between turns.

云栖 Agent 是一个可自托管的开源编程 Agent，包含 Rust 执行核心与可定制的 Windows 桌面客户端。客户端通过 SSH 连接 Agent 主机，支持流式对话、工具执行和任务相关记忆。

## Features

- Rust agent loop with read, write, edit, shell, memory, and task tools.
- SSH and NDJSON desktop bridge; output streams into the client while tools run.
- Memory search with freshness and importance ranking, plus bounded context injection.
- Declarative JSON UI plugins. Plugins can add, move, update, hide, or remove supported interface components without running arbitrary JavaScript.
- Windows desktop security using Electron sandboxing, context isolation, host-key checks, and Windows DPAPI-backed credential storage.
- OpenAI-compatible model gateways. Configure the model endpoint on the agent host; the client can select a model per connection.

## Requirements

- Rust 1.85 or newer for the agent core.
- Node.js 22.12 or newer and Windows x64 for the desktop client.
- PostgreSQL with pgvector for persistent memory and conversation storage.
- An SSH account on the agent host and a model endpoint supported by the gateway.

## Build the agent

```sh
cd server/core
cargo test --workspace
cargo build --release -p yq-cli --bin yunqiagent
```

The desktop bridge command is `yunqiagent desktop-stdio`. Configure the database and model endpoint in the agent host's `~/.yunqiagent/config.toml` or through the documented environment variables. Keep credentials and host-specific configuration outside this repository.

## Build the Windows client

```powershell
npm ci
npm run build
npm test
npm run dist
```

Launch the generated portable executable. In **Settings**, enter the SSH host, port, username, agent binary path, and the host's SHA256 fingerprint. Leave the model field empty to use the agent host's default model. The first connection requires the SSH password; Windows stores it through Electron `safeStorage`.

For development, use `npm start`; `npm run dev` starts a browser preview that does not have desktop IPC or SSH access. `npm run test:desktop` runs the desktop UI checks. Live model tests are opt-in and require `YUNQI_TEST_PASSWORD` in the environment.

## UI plugins

Plugins are JSON manifests. The examples in `examples/` show how to add shortcuts and change supported components. The editor validates operations, rolls back a failed plugin, and keeps a recovery action available if a layout becomes unusable. Plugins cannot inject scripts or arbitrary HTML.

## Security

The desktop client verifies the SSH host key before connecting. Credentials are stored using Electron `safeStorage`, backed by Windows DPAPI. The renderer has no Node.js access. Configure only trusted SSH hosts and model gateways, and do not commit API keys, passwords, private keys, local workspace data, or server configuration.

## License

Licensed under either the Apache License, Version 2.0 or the MIT license, at your option. See [LICENSE-APACHE](LICENSE-APACHE) and [LICENSE-MIT](LICENSE-MIT).
