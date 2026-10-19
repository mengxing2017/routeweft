use crate::error::DomainError;
use crate::model::{Coordinate, RouteQuery, TravelProfile};

/// 纯业务规则。算法执行与外部资源访问由应用层通过端口协调。
#[derive(Debug, Default)]
pub struct RoutingPolicy;

impl RoutingPolicy {
    pub fn prepare(
        waypoints: Vec<Coordinate>,
        profile: TravelProfile,
    ) -> Result<RouteQuery, DomainError> {
        RouteQuery::new(waypoints, profile)
    }
}
