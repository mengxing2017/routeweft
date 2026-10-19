use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DomainError {
    InvalidCoordinate,
    InsufficientWaypoints,
}

impl fmt::Display for DomainError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidCoordinate => {
                write!(f, "坐标必须有限，经度范围 [-180, 180]，纬度范围 [-90, 90]")
            }
            Self::InsufficientWaypoints => write!(f, "路径规划至少需要两个途经点"),
        }
    }
}

impl std::error::Error for DomainError {}
