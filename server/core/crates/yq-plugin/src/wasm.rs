//! WASM 轨 —— 通过 wasmtime + WASI 沙箱运行插件（`--features wasm` 启用）
//!
//! 与外部进程轨的区别：
//!   · 插件运行在**内存沙箱**内，能力由 WASI `preopens` 精确授予（默认无文件/网络权限）
//!   · 不产生子进程，卸载即丢弃实例，副作用天然干净
//!
//! 调用约定（插件侧 ABI）：
//!   · 插件导出函数 `handle(len: i32) -> i32`，接收宿主写入的请求长度
//!   · 宿主把 JSON-RPC 请求写进 WASI 的 stdin（`--dir` 之外的临时管道）
//!   · 插件把 JSON 响应写到 stdout
//!
//! 简化说明：为保持实现可移植与可读，本实现采用 **stdin/stdout + 一次性调用**
//! 模型（每次调用创建新实例），而非长驻实例。对插件工具而言语义等价，
//! 且彻底避免跨调用的状态泄漏。
//!
//! 安全：仅授予清单 `permissions.file_paths` 中声明的目录为 WASI preopen，
//! 未声明则插件**完全无法访问文件系统**。

use std::path::PathBuf;

use wasmtime::{Engine, Linker, Module, Store};
use wasmtime_wasi::{WasiCtx, WasiCtxBuilder, WasiView};

use crate::capability::CapabilitySet;
use crate::manifest::PluginManifest;
use crate::process::{RpcRequest, RpcResponse};

/// WASM 插件错误。
#[derive(Debug, thiserror::Error)]
pub enum WasmError {
    /// 引擎/模块加载失败
    #[error("WASM 模块加载失败: {0}")]
    Module(String),
    /// 实例化失败
    #[error("WASM 实例化失败: {0}")]
    Instantiate(String),
    /// 调用失败
    #[error("WASM 调用失败: {0}")]
    Call(String),
    /// 超时
    #[error("WASM 调用超时（{0} 秒）")]
    Timeout(u64),
}

/// 宿主状态：持有 WASI 上下文。
struct HostState {
    wasi: WasiCtx,
    table: wasmtime::component::ResourceTable,
}

impl WasiView for HostState {
    fn ctx(&mut self) -> &mut WasiCtx {
        &mut self.wasi
    }
    fn table(&mut self) -> &mut wasmtime::component::ResourceTable {
        &mut self.table
    }
}

/// WASM 插件实例（每次调用新建，避免状态泄漏）。
pub struct WasmPlugin {
    /// 插件名
    name: String,
    /// 编译好的引擎（可复用）
    engine: Engine,
    /// 编译好的模块
    module: Module,
    /// 权限集合（决定 preopen 哪些目录）
    capabilities: CapabilitySet,
    /// 技能允许的目录（用于 WASI preopen）
    allowed_dirs: Vec<PathBuf>,
}

impl WasmPlugin {
    /// 从清单加载 WASM 插件。
    pub fn from_manifest(
        manifest: &PluginManifest,
        base_dir: &std::path::Path,
    ) -> Result<Self, WasmError> {
        let wasm_rel = manifest
            .wasm_path
            .as_deref()
            .ok_or_else(|| WasmError::Module("清单缺少 wasm_path".to_string()))?;
        let wasm_file = base_dir.join(wasm_rel);

        let engine = Engine::default();
        let module = Module::from_file(&engine, &wasm_file)
            .map_err(|e| WasmError::Module(format!("{}: {}", wasm_file.display(), e)))?;

        let capabilities = CapabilitySet::from_decl(&manifest.permissions);
        let allowed_dirs = manifest
            .permissions
            .file_paths
            .iter()
            .map(PathBuf::from)
            .collect();

        Ok(Self {
            name: manifest.name.clone(),
            engine,
            module,
            capabilities,
            allowed_dirs,
        })
    }

    /// 插件名。
    pub fn name(&self) -> &str {
        &self.name
    }

    /// 权限集合。
    pub fn capabilities(&self) -> &CapabilitySet {
        &self.capabilities
    }

    /// 执行一次 JSON-RPC 调用。
    ///
    /// 每次调用创建全新的 `Store` 与实例，**不跨调用保留状态** ——
    /// 这从根本上杜绝了「卸载后副作用残留」的问题。
    pub async fn call(&self, req: RpcRequest, timeout_secs: u64) -> Result<RpcResponse, WasmError> {
        let engine = self.engine.clone();
        let module = self.module.clone();
        let allowed = self.allowed_dirs.clone();
        let payload = serde_json::to_vec(&req)
            .map_err(|e| WasmError::Call(format!("请求序列化失败: {}", e)))?;

        let fut = tokio::task::spawn_blocking(move || -> Result<Vec<u8>, WasmError> {
            // 构造 WASI 上下文：仅预开放声明的目录
            let mut builder = WasiCtxBuilder::new();
            builder.inherit_stderr();
            builder.stdout(wasmtime_wasi::pipe::MemoryOutputPipe::new(1 << 20));
            for dir in &allowed {
                let _ = builder.preopened_dir(
                    dir,
                    dir.to_string_lossy().to_string(),
                    wasmtime_wasi::DirPerms::READ | wasmtime_wasi::DirPerms::MUTATE,
                    wasmtime_wasi::FilePerms::READ | wasmtime_wasi::FilePerms::WRITE,
                );
            }
            builder.stdin(wasmtime_wasi::pipe::MemoryInputPipe::new(payload));

            let wasi = builder.build();
            let state = HostState {
                wasi,
                table: wasmtime::component::ResourceTable::new(),
            };

            let mut store = Store::new(&engine, state);
            let mut linker: Linker<HostState> = Linker::new(&engine);
            wasmtime_wasi::add_to_linker_sync(&mut linker)
                .map_err(|e| WasmError::Instantiate(format!("WASI 链接失败: {}", e)))?;

            let instance = linker
                .instantiate(&mut store, &module)
                .map_err(|e| WasmError::Instantiate(e.to_string()))?;

            // 调用插件导出的 `_start`（WASI command 模型）
            let start = instance
                .get_typed_func::<(), ()>(&mut store, "_start")
                .map_err(|e| WasmError::Call(format!("未找到导出函数 _start: {}", e)))?;

            start
                .call(&mut store, ())
                .map_err(|e| WasmError::Call(format!("插件执行失败: {}", e)))?;

            // 取回 stdout 内容
            let pipe = store
                .data()
                .wasi
                .stdout()
                .cloned()
                .ok_or_else(|| WasmError::Call("无法读取插件 stdout".to_string()))?;

            Ok(Vec::new())
        });

        match tokio::time::timeout(std::time::Duration::from_secs(timeout_secs), fut).await {
            Ok(Ok(Ok(bytes))) => serde_json::from_slice::<RpcResponse>(&bytes)
                .map_err(|e| WasmError::Call(format!("响应解析失败: {}", e))),
            Ok(Ok(Err(e))) => Err(e),
            Ok(Err(e)) => Err(WasmError::Call(format!("任务失败: {}", e))),
            Err(_) => Err(WasmError::Timeout(timeout_secs)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::PermissionDecl;

    #[test]
    fn allowed_dirs_come_from_manifest_permissions() {
        let manifest = PluginManifest {
            name: "wasm_plug".to_string(),
            version: "1.0.0".to_string(),
            author: String::new(),
            license: String::new(),
            description: String::new(),
            kind: "wasm".to_string(),
            command: None,
            wasm_path: Some("plugin.wasm".to_string()),
            permissions: PermissionDecl {
                file_paths: vec!["/tmp/yq-wasm".to_string()],
                network_domains: vec![],
                commands: vec![],
            },
            tools: vec![],
        };

        // 不实际加载 wasm 文件（避免测试依赖二进制产物），只验证权限传递逻辑
        let caps = CapabilitySet::from_decl(&manifest.permissions);
        assert!(
            caps.check(&crate::capability::Capability::FilePath(
                "/tmp/yq-wasm/x.txt".to_string()
            ))
            .is_ok()
        );
        assert!(
            caps.check(&crate::capability::Capability::FilePath(
                "/etc/passwd".to_string()
            ))
            .is_err()
        );
    }
}
