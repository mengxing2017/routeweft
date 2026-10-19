use crate::error::Error;
use crate::north::message::{RouteRequest, RouteResponse};
use crate::south::port::routing::RoutingEngine;
use routeweft_domain::model::{Coordinate, TravelProfile};
use routeweft_domain::service::RoutingPolicy;
use std::sync::Arc;

/// 用例编排：转换 DTO、执行领域规则、调用出站端口；不依赖具体适配器。
pub struct RoutingAppService {
    engine: Arc<dyn RoutingEngine>,
}

impl RoutingAppService {
    pub fn new(engine: Arc<dyn RoutingEngine>) -> Self {
        Self { engine }
    }

    pub fn route(&self, request: RouteRequest) -> Result<RouteResponse, Error> {
        let profile = match request.profile.as_str() {
            "driving" => TravelProfile::Driving,
            "cycling" => TravelProfile::Cycling,
            "walking" => TravelProfile::Walking,
            value => return Err(Error::InvalidRequest(format!("未知出行方式：{value}"))),
        };
        let coordinates = request
            .coordinates
            .into_iter()
            .map(|(longitude, latitude)| Coordinate::new(longitude, latitude))
            .collect::<Result<Vec<_>, _>>()?;
        let query = RoutingPolicy::prepare(coordinates, profile)?;
        let route = self.engine.route(&query)?;
        Ok(RouteResponse {
            geometry: route
                .geometry()
                .iter()
                .map(|point| (point.longitude(), point.latitude()))
                .collect(),
        })
    }
}
