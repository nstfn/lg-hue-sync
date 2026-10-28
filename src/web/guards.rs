//! Guards that keep the unauthenticated LAN API bounded on a 2-core TV, and that refuse
//! DNS-rebinding and cross-site requests from a browser.

use axum::{
    extract::{Request, State},
    http::{header, HeaderName, Method, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
    Json,
};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};
use tokio::sync::Semaphore;

/// Host name advertised by `bonjour::service_info`, without its trailing dot. Keep both equal.
pub const BONJOUR_HOST: &str = "lg-hue-sync.local";
pub const MAX_IN_FLIGHT_REQUESTS: usize = 16;
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// Hue pairing waits up to 45 s for the link button.
pub const SETUP_REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
/// Pairing and Hue area routes: they share one setup permit and the longer timeout.
const SETUP_PATHS: [&str; 4] = [
    "/api/pair/hue",
    "/api/pair/nanoleaf",
    "/api/hue/areas",
    "/api/hue/area",
];
/// The daemon serves plain HTTP only, so a Host without a port means port 80.
const HTTP_PORT: u16 = 80;
const HOST_REJECTED: &str =
    "Unknown Host; use the TV's IP address, localhost or lg-hue-sync.local.";
const ORIGIN_REJECTED: &str = "Cross-origin requests may not change the daemon.";

fn error_response(status: StatusCode, message: &str) -> Response {
    let body = Json(serde_json::json!({ "error": message }));
    (status, body).into_response()
}

#[derive(Clone)]
pub struct RequestLimits {
    in_flight: Arc<Semaphore>,
    timeout: Duration,
    setup_timeout: Duration,
}

impl RequestLimits {
    pub fn new(max_in_flight: usize, timeout: Duration, setup_timeout: Duration) -> Self {
        Self {
            in_flight: Arc::new(Semaphore::new(max_in_flight)),
            timeout,
            setup_timeout,
        }
    }

    fn timeout_for(&self, path: &str) -> Duration {
        if SETUP_PATHS.contains(&path) {
            self.setup_timeout
        } else {
            self.timeout
        }
    }
}

impl Default for RequestLimits {
    fn default() -> Self {
        Self::new(
            MAX_IN_FLIGHT_REQUESTS,
            REQUEST_TIMEOUT,
            SETUP_REQUEST_TIMEOUT,
        )
    }
}

/// Answers 503 when all request slots are taken and 504 when the handler runs past its timeout.
pub async fn limit_requests(
    State(limits): State<RequestLimits>,
    request: Request,
    next: Next,
) -> Response {
    let Ok(_permit) = limits.in_flight.try_acquire() else {
        return error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "Too many requests in progress; retry shortly.",
        );
    };
    let timeout = limits.timeout_for(request.uri().path());
    match tokio::time::timeout(timeout, next.run(request)).await {
        Ok(response) => response,
        Err(_) => error_response(StatusCode::GATEWAY_TIMEOUT, "Request timed out."),
    }
}

/// Answers 421 for an unknown Host and 403 for a state-changing request from another origin.
pub async fn check_host_origin(request: Request, next: Next) -> Response {
    let verdict = host_origin_verdict(
        request.method(),
        header_value(&request, header::HOST),
        header_value(&request, header::ORIGIN),
    );
    if let Some((status, message)) = verdict {
        return error_response(status, message);
    }
    next.run(request).await
}

/// A header that is not visible ASCII reads as empty, which no check accepts.
fn header_value(request: &Request, name: HeaderName) -> Option<&str> {
    let value = request.headers().get(name)?;
    Some(value.to_str().unwrap_or(""))
}

#[derive(Debug, PartialEq, Eq)]
enum HostName {
    Ip(IpAddr),
    Name(String),
}

/// Splits `name[:port]` or `[ipv6][:port]`; names are lowercased without a trailing dot.
fn split_authority(authority: &str) -> Option<(HostName, Option<u16>)> {
    let (name, port) = match authority.strip_prefix('[') {
        Some(rest) => {
            let (address, port) = rest.split_once(']')?;
            let address = address.parse::<Ipv6Addr>().ok()?;
            (HostName::Ip(IpAddr::V6(address)), port)
        }
        None => {
            let name_end = authority.find(':').unwrap_or(authority.len());
            let (name, port) = authority.split_at(name_end);
            let name = match name.parse::<Ipv4Addr>() {
                Ok(address) => HostName::Ip(IpAddr::V4(address)),
                Err(_) => {
                    let name = name.strip_suffix('.').unwrap_or(name);
                    if name.is_empty() {
                        return None;
                    }
                    HostName::Name(name.to_ascii_lowercase())
                }
            };
            (name, port)
        }
    };
    let port = match port {
        "" => None,
        port => {
            let digits = port.strip_prefix(':')?;
            if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
                return None;
            }
            Some(digits.parse::<u16>().ok()?)
        }
    };
    Some((name, port))
}

fn name_allowed(name: &HostName) -> bool {
    match name {
        HostName::Ip(_) => true,
        HostName::Name(name) => name == "localhost" || name == BONJOUR_HOST,
    }
}

/// Reduces a serialised Origin (`scheme://host[:port]`) to its host and effective port.
fn origin_authority(origin: &str) -> Option<(HostName, u16)> {
    let (scheme, authority) = origin.split_once("://")?;
    let default_port = match scheme.to_ascii_lowercase().as_str() {
        "http" => 80,
        "https" => 443,
        _ => return None,
    };
    if authority.contains('/') {
        return None;
    }
    let (name, port) = split_authority(authority)?;
    Some((name, port.unwrap_or(default_port)))
}

fn is_state_changing(method: &Method) -> bool {
    matches!(
        *method,
        Method::POST | Method::PUT | Method::PATCH | Method::DELETE
    )
}

/// A request without Host cannot come from a browser, so DNS rebinding cannot use it; such
/// requests pass the Host check. Clients without Origin (Home Assistant, curl, Shortcuts) pass
/// the Origin check.
fn host_origin_verdict(
    method: &Method,
    host: Option<&str>,
    origin: Option<&str>,
) -> Option<(StatusCode, &'static str)> {
    let host = match host.map(split_authority) {
        None => None,
        Some(Some((name, port))) if name_allowed(&name) => Some((name, port.unwrap_or(HTTP_PORT))),
        Some(_) => return Some((StatusCode::MISDIRECTED_REQUEST, HOST_REJECTED)),
    };
    if let Some(origin) = origin.filter(|_| is_state_changing(method)) {
        if host.is_none() || origin_authority(origin) != host {
            return Some((StatusCode::FORBIDDEN, ORIGIN_REJECTED));
        }
    }
    None
}

/// Admits at most one action per interval. A refused claim returns the wait left.
pub struct RateWindow {
    interval: Duration,
    last: Mutex<Option<Instant>>,
}

impl RateWindow {
    pub const fn new(interval: Duration) -> Self {
        Self {
            interval,
            last: Mutex::new(None),
        }
    }

    fn last(&self) -> MutexGuard<'_, Option<Instant>> {
        self.last.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// On success, returns the previous claim so that a failed action can `restore` it.
    pub fn try_claim(&self, now: Instant) -> Result<Option<Instant>, Duration> {
        let mut last = self.last();
        if let Some(previous) = *last {
            let elapsed = now.saturating_duration_since(previous);
            if elapsed < self.interval {
                return Err(self.interval - elapsed);
            }
        }
        Ok(last.replace(now))
    }

    pub fn restore(&self, previous: Option<Instant>) {
        *self.last() = previous;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::Body, routing::get, Router};
    use tower::ServiceExt;

    fn request(method: Method, path: &str, headers: &[(&str, &str)]) -> Request {
        let mut builder = axum::http::Request::builder().method(method).uri(path);
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        builder.body(Body::empty()).unwrap()
    }

    fn guarded_router() -> Router {
        Router::new()
            .route("/api/status", get(|| async { "ok" }).post(|| async { "ok" }))
            .layer(axum::middleware::from_fn(check_host_origin))
    }

    async fn status(app: &Router, request: Request) -> StatusCode {
        app.clone().oneshot(request).await.unwrap().status()
    }

    #[tokio::test]
    async fn host_check_accepts_ip_literals_localhost_and_bonjour_name_only() {
        let app = guarded_router();
        for host in [
            "192.0.2.10",
            "192.0.2.10:8088",
            "[fe80::1]",
            "[::1]:8088",
            "localhost:8088",
            "LocalHost",
            "lg-hue-sync.local",
            "lg-hue-sync.local.:8088",
        ] {
            let probe = request(Method::GET, "/api/status", &[("host", host)]);
            assert_eq!(status(&app, probe).await, StatusCode::OK, "{host}");
        }
        for host in [
            "evil.example",
            "evil.example:8088",
            "lg-hue-sync.local.evil.example",
            "192.0.2.10.nip.io",
            "::1",
            "[::1",
            "192.0.2.10:",
            "192.0.2.10:+80",
            "192.0.2.10:99999",
            "",
        ] {
            let probe = request(Method::GET, "/api/status", &[("host", host)]);
            assert_eq!(
                status(&app, probe).await,
                StatusCode::MISDIRECTED_REQUEST,
                "{host}"
            );
        }
        let no_host = request(Method::GET, "/api/status", &[]);
        assert_eq!(status(&app, no_host).await, StatusCode::OK);
    }

    #[tokio::test]
    async fn origin_check_refuses_cross_origin_state_changes_only() {
        let app = guarded_router();
        let host = ("host", "192.0.2.10:8088");
        let cross = request(
            Method::POST,
            "/api/status",
            &[host, ("origin", "http://evil.example")],
        );
        assert_eq!(status(&app, cross).await, StatusCode::FORBIDDEN);
        let other_port = request(
            Method::POST,
            "/api/status",
            &[host, ("origin", "http://192.0.2.10:9000")],
        );
        assert_eq!(status(&app, other_port).await, StatusCode::FORBIDDEN);
        let opaque = request(Method::POST, "/api/status", &[host, ("origin", "null")]);
        assert_eq!(status(&app, opaque).await, StatusCode::FORBIDDEN);
        let same = request(
            Method::POST,
            "/api/status",
            &[host, ("origin", "http://192.0.2.10:8088")],
        );
        assert_eq!(status(&app, same).await, StatusCode::OK);
        let bonjour = request(
            Method::POST,
            "/api/status",
            &[
                ("host", "lg-hue-sync.local"),
                ("origin", "http://LG-Hue-Sync.local:80"),
            ],
        );
        assert_eq!(status(&app, bonjour).await, StatusCode::OK);
        let without_origin = request(Method::POST, "/api/status", &[host]);
        assert_eq!(status(&app, without_origin).await, StatusCode::OK);
        let cross_get = request(
            Method::GET,
            "/api/status",
            &[host, ("origin", "http://evil.example")],
        );
        assert_eq!(status(&app, cross_get).await, StatusCode::OK);
        assert_eq!(
            host_origin_verdict(&Method::DELETE, None, Some("http://192.0.2.10")),
            Some((StatusCode::FORBIDDEN, ORIGIN_REJECTED))
        );
    }

    #[tokio::test]
    async fn in_flight_limit_answers_503_while_all_slots_are_taken() {
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let handler = {
            let (entered, release) = (entered.clone(), release.clone());
            move || {
                let (entered, release) = (entered.clone(), release.clone());
                async move {
                    entered.notify_one();
                    release.notified().await;
                    "done"
                }
            }
        };
        let limits = RequestLimits::new(1, Duration::from_secs(5), Duration::from_secs(5));
        let app = Router::new()
            .route("/hold", get(handler))
            .layer(axum::middleware::from_fn_with_state(limits, limit_requests));
        let held = tokio::spawn(app.clone().oneshot(request(Method::GET, "/hold", &[])));
        entered.notified().await;
        assert_eq!(
            status(&app, request(Method::GET, "/hold", &[])).await,
            StatusCode::SERVICE_UNAVAILABLE
        );
        release.notify_one();
        assert_eq!(held.await.unwrap().unwrap().status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn request_timeout_fires_and_setup_routes_get_the_longer_timeout() {
        let slow = || async {
            tokio::time::sleep(Duration::from_millis(200)).await;
            "late"
        };
        let limits = RequestLimits::new(4, Duration::from_millis(50), Duration::from_secs(5));
        let app = Router::new()
            .route("/api/status", get(slow))
            .route("/api/pair/hue", get(slow))
            .layer(axum::middleware::from_fn_with_state(limits, limit_requests));
        assert_eq!(
            status(&app, request(Method::GET, "/api/status", &[])).await,
            StatusCode::GATEWAY_TIMEOUT
        );
        assert_eq!(
            status(&app, request(Method::GET, "/api/pair/hue", &[])).await,
            StatusCode::OK
        );
        let defaults = RequestLimits::default();
        assert_eq!(defaults.timeout_for("/api/start"), REQUEST_TIMEOUT);
        let setup = defaults.timeout_for("/api/pair/hue");
        assert_eq!(setup, SETUP_REQUEST_TIMEOUT);
    }

    #[test]
    fn rate_window_admits_one_claim_per_interval_and_restores_failures() {
        let window = RateWindow::new(Duration::from_secs(10));
        let start = Instant::now();
        assert_eq!(window.try_claim(start), Ok(None));
        assert_eq!(
            window.try_claim(start + Duration::from_secs(4)),
            Err(Duration::from_secs(6))
        );
        let later = start + Duration::from_secs(10);
        assert_eq!(window.try_claim(later), Ok(Some(start)));
        window.restore(Some(start));
        assert_eq!(window.try_claim(later), Ok(Some(start)));
    }
}
