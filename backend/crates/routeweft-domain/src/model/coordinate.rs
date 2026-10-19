use crate::error::DomainError;

/// 经纬度值对象。构造时维护不变量；坐标顺序为经度、纬度。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Coordinate {
    longitude: f64,
    latitude: f64,
}

impl Coordinate {
    pub fn new(longitude: f64, latitude: f64) -> Result<Self, DomainError> {
        if !longitude.is_finite()
            || !latitude.is_finite()
            || !(-180.0..=180.0).contains(&longitude)
            || !(-90.0..=90.0).contains(&latitude)
        {
            return Err(DomainError::InvalidCoordinate);
        }
        Ok(Self {
            longitude,
            latitude,
        })
    }

    pub fn longitude(self) -> f64 {
        self.longitude
    }
    pub fn latitude(self) -> f64 {
        self.latitude
    }
}
