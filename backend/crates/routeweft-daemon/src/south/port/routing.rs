use routeweft_domain::model::{Route, RouteQuery};
use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RoutingError {
    NotConfigured,
    NoRoute,
    Backend(String),
}

impl fmt::Display for RoutingError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotConfigured => write!(f, "尚未配置路由引擎；当前仅提供项目骨架"),
            Self::NoRoute => write!(f, "未找到可达路线"),
            Self::Backend(message) => write!(f, "路由后端错误：{message}"),
        }
    }
}

impl std::error::Error for RoutingError {}

/// 由应用侧定义的路由能力契约，不暴露文件、网络或算法实现类型。
pub trait RoutingEngine: Send + Sync {
    fn route(&self, query: &RouteQuery) -> Result<Route, RoutingError>;
}
