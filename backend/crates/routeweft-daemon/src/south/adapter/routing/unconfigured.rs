use crate::south::port::routing::{RoutingEngine, RoutingError};
use routeweft_domain::model::{Route, RouteQuery};

/// 启动占位适配器：明确返回未配置，不伪造道路路线。
#[derive(Debug, Default)]
pub struct UnconfiguredRoutingEngine;

impl RoutingEngine for UnconfiguredRoutingEngine {
    fn route(&self, _query: &RouteQuery) -> Result<Route, RoutingError> {
        Err(RoutingError::NotConfigured)
    }
}
