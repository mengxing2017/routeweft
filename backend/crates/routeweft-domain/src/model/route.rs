use super::Coordinate;
use crate::error::DomainError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TravelProfile {
    Driving,
    Cycling,
    Walking,
}

/// 经过校验的路径查询，调用者不能绕过途经点数量约束。
#[derive(Debug, Clone, PartialEq)]
pub struct RouteQuery {
    waypoints: Vec<Coordinate>,
    profile: TravelProfile,
}

impl RouteQuery {
    pub fn new(waypoints: Vec<Coordinate>, profile: TravelProfile) -> Result<Self, DomainError> {
        if waypoints.len() < 2 {
            return Err(DomainError::InsufficientWaypoints);
        }
        Ok(Self { waypoints, profile })
    }

    pub fn waypoints(&self) -> &[Coordinate] {
        &self.waypoints
    }
    pub fn profile(&self) -> TravelProfile {
        self.profile
    }
}

/// 路由引擎返回的路线几何；后续在领域需求确定后扩展耗时、距离与导航步骤。
#[derive(Debug, Clone, PartialEq)]
pub struct Route {
    geometry: Vec<Coordinate>,
}

impl Route {
    pub fn new(geometry: Vec<Coordinate>) -> Result<Self, DomainError> {
        if geometry.len() < 2 {
            return Err(DomainError::InsufficientWaypoints);
        }
        Ok(Self { geometry })
    }

    pub fn geometry(&self) -> &[Coordinate] {
        &self.geometry
    }
}
