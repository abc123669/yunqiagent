//! yq-core 统一错误类型

use thiserror::Error;

/// 工具执行错误。
#[derive(Debug, Error)]
pub enum ToolError {
    /// 参数 JSON 不合法
    #[error("工具参数不合法: {0}")]
    InvalidArguments(String),

    /// 缺少必需参数
    #[error("缺少必需参数: {0}")]
    MissingArgument(&'static str),

    /// 文件系统错误
    #[error("文件操作失败: {0}")]
    Io(#[from] std::io::Error),

    /// 目标路径越界（超出允许工作目录）
    #[error("路径越界，拒绝访问: {0}")]
    PathOutOfBounds(String),

    /// 子代理执行失败
    #[error("子代理执行失败: {0}")]
    SubagentFailed(String),

    /// 其他工具错误
    #[error("工具执行错误: {0}")]
    Other(String),
}

/// yq-core 核心错误。
#[derive(Debug, Error)]
pub enum CoreError {
    /// 工具错误
    #[error(transparent)]
    Tool(#[from] ToolError),

    /// persona 配置文件读取失败
    #[error("读取 persona 配置失败: {0}")]
    PersonaIo(#[from] std::io::Error),

    /// persona 配置解析失败
    #[error("解析 persona 配置失败: {0}")]
    PersonaParse(#[from] toml::de::Error),

    /// 消息构造错误
    #[error("消息错误: {0}")]
    Message(String),

    /// 其他核心错误
    #[error("核心错误: {0}")]
    Other(String),
}
