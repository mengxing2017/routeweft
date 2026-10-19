use routeweft_daemon::{
    composition,
    error::Error,
    north::{local::app_service::RoutingAppService, message::RouteRequest},
    south::port::routing::{RoutingEngine, RoutingError},
};
use routeweft_domain::{
    error::DomainError,
    model::{Coordinate, Route, RouteQuery},
};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

struct RecordingEngine(Arc<AtomicUsize>);
impl RoutingEngine for RecordingEngine {
    fn route(&self, query: &RouteQuery) -> Result<Route, RoutingError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        // 仅测试替身回显输入，生产适配器不会将直线伪装成道路路线。
        Ok(Route::new(query.waypoints().to_vec()).unwrap())
    }
}

fn request(coordinates: Vec<(f64, f64)>) -> RouteRequest {
    RouteRequest {
        coordinates,
        profile: "driving".into(),
    }
}

#[test]
fn valid_request_calls_injected_engine_and_maps_response() {
    let calls = Arc::new(AtomicUsize::new(0));
    let service = RoutingAppService::new(Arc::new(RecordingEngine(calls.clone())));
    let coordinates = vec![(13.388860, 52.517037), (13.385983, 52.496891)];
    let response = service.route(request(coordinates.clone())).unwrap();
    assert_eq!(response.geometry, coordinates);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[test]
fn invalid_requests_never_reach_engine() {
    let calls = Arc::new(AtomicUsize::new(0));
    let service = RoutingAppService::new(Arc::new(RecordingEngine(calls.clone())));
    for coordinates in [vec![], vec![(0.0, 0.0)], vec![(181.0, 0.0), (0.0, 0.0)]] {
        assert!(matches!(
            service.route(request(coordinates)),
            Err(Error::Domain(_))
        ));
    }
    let mut unsupported = request(vec![(0.0, 0.0), (1.0, 1.0)]);
    unsupported.profile = "unknown".into();
    assert!(matches!(
        service.route(unsupported),
        Err(Error::InvalidRequest(_))
    ));
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[test]
fn coordinate_rejects_non_finite_values_and_accepts_boundaries() {
    for (longitude, latitude) in [
        (f64::NAN, 0.0),
        (0.0, f64::INFINITY),
        (-181.0, 0.0),
        (0.0, 91.0),
    ] {
        assert_eq!(
            Coordinate::new(longitude, latitude),
            Err(DomainError::InvalidCoordinate)
        );
    }
    assert!(Coordinate::new(-180.0, -90.0).is_ok());
    assert!(Coordinate::new(180.0, 90.0).is_ok());
}

#[test]
fn default_composition_reports_unconfigured_backend() {
    assert!(matches!(
        composition::build_routing_service().route(request(vec![(0.0, 0.0), (1.0, 1.0)])),
        Err(Error::Routing(RoutingError::NotConfigured))
    ));
}

#[test]
fn cli_rejects_malformed_input_and_recognizes_help() {
    use routeweft_daemon::cli::{parse, Command};
    assert!(matches!(parse(Vec::<String>::new()), Ok(Command::Help)));
    for args in [
        vec!["route", "driving", "invalid"],
        vec!["route", "driving", "0,0;1,no"],
        vec!["unknown"],
    ] {
        assert!(parse(args.into_iter().map(str::to_owned)).is_err());
    }
}
