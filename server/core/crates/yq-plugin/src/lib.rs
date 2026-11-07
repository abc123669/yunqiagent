//! yq-plugin —— yunqiagent 插件宿主
//!
//! 双轨设计（SPEC 第十一节）：
//!
//! | 轨道 | 运行方式 | 安全等级 | 默认 |
//! |---|---|---|---|
//! | **WASM 轨** | wasmtime + WASI 沙箱 | 高（能力校验 + 沙箱） | 关闭（`--features wasm`） |
//! | **外部进程轨** | stdio / JSON-RPC | 低（进程级权限，文档明确警告） | **开启** |
//!
//! 模块：
//!   · `manifest`    plugin.toml 清单解析
//!   · `capability`  能力声明与越权校验
//!   · `registry`    插件注册表（加载 / 卸载 / 热插拔）
//!   · `process`     外部进程轨（stdio JSON-RPC）
//!   · `wasm`        WASM 轨（feature = "wasm"）
//!
//! **热插拔**：插件目录 `plugins/`，支持运行时加载/卸载，卸载时完整撤销副作用，
//! 插件变化时重新解析依赖，不重启 agent。

pub mod capability;
pub mod manifest;
pub mod process;
pub mod registry;

#[cfg(feature = "wasm")]
pub mod wasm;

pub use capability::{Capability, CapabilityError, CapabilitySet};
pub use manifest::{PluginManifest, ToolSpec};
pub use process::{ProcessPlugin, RpcRequest, RpcResponse};
pub use registry::{PluginRegistry, PluginState};
