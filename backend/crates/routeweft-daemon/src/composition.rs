//! 唯一组合根：选择具体适配器并注入应用服务。
use crate::north::local::app_service::RoutingAppService;
use crate::south::adapter::routing::UnconfiguredRoutingEngine;
use std::sync::Arc;

pub fn build_routing_service() -> RoutingAppService {
    RoutingAppService::new(Arc::new(UnconfiguredRoutingEngine))
}
