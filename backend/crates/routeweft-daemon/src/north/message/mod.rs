//! 传输无关的应用 DTO，不将协议字段引入领域层。
#[derive(Debug, Clone)]
pub struct RouteRequest {
    /// 每项均为 (经度, 纬度)。
    pub coordinates: Vec<(f64, f64)>,
    pub profile: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RouteResponse {
    pub geometry: Vec<(f64, f64)>,
}
