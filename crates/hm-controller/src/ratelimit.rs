//! Rate limiting and response hardening for the HTTP API.
//!
//! Limits are token buckets kept in memory, per replica: behind a load
//! balancer spreading clients over N replicas a client gets up to N times
//! the budget. That bounds password guessing and floods well enough without
//! a round trip to Postgres on every request.
//!
//! - Every request: per client address.
//! - Password logins: per client address, and failures per account, so
//!   guessing one account's password from many addresses is slow too. An
//!   account's budget only drains on failures, and refills, so an attacker
//!   can slow a user's logins down but not lock them out for good.

use std::collections::HashMap;
use std::hash::Hash;
use std::net::{IpAddr, SocketAddr};
use std::str::FromStr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use axum::extract::{ConnectInfo, Request, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use crate::state::AppState;

/// A token bucket per key.
pub struct Limiter<K> {
    /// Tokens added per second.
    rate: f64,
    burst: f64,
    buckets: Mutex<HashMap<K, (f64, Instant)>>,
}

impl<K: Hash + Eq + Clone> Limiter<K> {
    pub fn new(rate: f64, burst: u32) -> Self {
        Self {
            rate,
            burst: burst as f64,
            buckets: Mutex::default(),
        }
    }

    /// Takes a token for `key`, or says how long until one is available.
    pub fn take(&self, key: &K) -> Result<(), Duration> {
        self.take_at(key, Instant::now())
    }

    /// Whether `key` has a token left, without taking it.
    pub fn check(&self, key: &K) -> Result<(), Duration> {
        let now = Instant::now();
        let buckets = self.buckets.lock().unwrap();
        match buckets.get(key) {
            Some(&(tokens, at)) => self.wait(self.refill(tokens, at, now)),
            None => Ok(()),
        }
    }

    fn refill(&self, tokens: f64, at: Instant, now: Instant) -> f64 {
        (tokens + now.duration_since(at).as_secs_f64() * self.rate).min(self.burst)
    }

    fn wait(&self, tokens: f64) -> Result<(), Duration> {
        if tokens >= 1.0 {
            Ok(())
        } else {
            Err(Duration::from_secs_f64((1.0 - tokens) / self.rate))
        }
    }

    fn take_at(&self, key: &K, now: Instant) -> Result<(), Duration> {
        let mut buckets = self.buckets.lock().unwrap();
        if buckets.len() > 50_000 {
            // Forget keys whose buckets have refilled; they lose nothing.
            let full = Duration::from_secs_f64(self.burst / self.rate);
            buckets.retain(|_, (_, at)| now.duration_since(*at) < full);
        }
        let (tokens, at) = buckets.entry(key.clone()).or_insert((self.burst, now));
        let available = self.refill(*tokens, *at, now);
        self.wait(available)?;
        (*tokens, *at) = (available - 1.0, now);
        Ok(())
    }
}

/// The limits the API enforces.
pub struct Limits {
    pub requests: Limiter<IpAddr>,
    pub logins: Limiter<IpAddr>,
    pub login_failures: Limiter<String>,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            // Generous: the UI fires a burst of requests per page.
            requests: Limiter::new(50.0, 300),
            logins: Limiter::new(0.5, 30),
            login_failures: Limiter::new(1.0 / 60.0, 5),
        }
    }
}

/// Networks whose `X-Forwarded-For` is believed (the load balancers).
#[derive(Clone, Debug)]
pub struct Cidr {
    net: IpAddr,
    len: u8,
}

impl FromStr for Cidr {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, String> {
        let (addr, len) = s.split_once('/').unwrap_or((s, ""));
        let net: IpAddr = addr.parse().map_err(|_| format!("bad address in {s}"))?;
        let max = if net.is_ipv4() { 32 } else { 128 };
        let len = if len.is_empty() {
            max
        } else {
            len.parse()
                .map_err(|_| format!("bad prefix length in {s}"))?
        };
        if len > max {
            return Err(format!("prefix length too long in {s}"));
        }
        Ok(Self { net, len })
    }
}

impl Cidr {
    fn contains(&self, ip: IpAddr) -> bool {
        fn bits(ip: IpAddr) -> (u128, u8) {
            match ip {
                IpAddr::V4(a) => (u32::from(a) as u128, 32),
                IpAddr::V6(a) => (u128::from(a), 128),
            }
        }
        let ip = match ip {
            IpAddr::V6(a) => a.to_ipv4_mapped().map_or(ip, IpAddr::V4),
            v4 => v4,
        };
        let ((net, w), (addr, w2)) = (bits(self.net), bits(ip));
        if w != w2 {
            return false;
        }
        let shift = u32::from(w - self.len);
        self.len == 0 || net.checked_shr(shift) == addr.checked_shr(shift)
    }
}

/// The address a request came from: the peer, or when the peer is a trusted
/// proxy, the last address in `X-Forwarded-For` that is not one.
pub fn client_ip(peer: SocketAddr, headers: &HeaderMap, trusted: &[Cidr]) -> IpAddr {
    let peer = peer.ip();
    let trusted_ip = |ip: IpAddr| trusted.iter().any(|c| c.contains(ip));
    if !trusted_ip(peer) {
        return peer;
    }
    let forwarded: Vec<IpAddr> = headers
        .get_all("x-forwarded-for")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .filter_map(|a| a.trim().parse().ok())
        .collect();
    forwarded
        .into_iter()
        .rev()
        .find(|ip| !trusted_ip(*ip))
        .unwrap_or(peer)
}

pub fn too_many(retry: Duration) -> Response {
    let mut resp = crate::error::ApiError::new(
        StatusCode::TOO_MANY_REQUESTS,
        "rate_limited",
        "too many requests; try again shortly",
    )
    .into_response();
    resp.headers_mut().insert(
        header::RETRY_AFTER,
        HeaderValue::from(retry.as_secs().max(1)),
    );
    metrics::counter!("hm_rate_limited_total").increment(1);
    resp
}

/// The client's address, as the rate limits see it.
#[derive(Clone, Copy)]
pub struct ClientIp(pub IpAddr);

/// Limits requests per client, and records the client's address for
/// handlers.
pub async fn limit(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    mut req: Request,
    next: Next,
) -> Response {
    let ip = client_ip(peer, req.headers(), &state.trusted_proxies);
    if let Err(retry) = state.limits.requests.take(&ip) {
        return too_many(retry);
    }
    req.extensions_mut().insert(ClientIp(ip));
    next.run(req).await
}

/// Headers that keep browsers from misusing what the controller serves.
pub async fn harden(State(state): State<AppState>, req: Request, next: Next) -> Response {
    let api = req.uri().path().starts_with("/api/");
    let mut resp = next.run(req).await;
    let h = resp.headers_mut();
    let mut set = |name: HeaderName, value: &'static str| {
        h.entry(name).or_insert(HeaderValue::from_static(value));
    };
    set(header::X_CONTENT_TYPE_OPTIONS, "nosniff");
    set(header::X_FRAME_OPTIONS, "DENY");
    set(header::REFERRER_POLICY, "same-origin");
    set(
        HeaderName::from_static("cross-origin-opener-policy"),
        "same-origin",
    );
    // Scripts only from the controller; xterm.js and Vue set inline styles.
    set(
        header::CONTENT_SECURITY_POLICY,
        "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; \
         img-src 'self' data: blob:; font-src 'self' data:; connect-src 'self'; \
         object-src 'none'; base-uri 'self'; form-action 'self'; frame-ancestors 'none'",
    );
    if state.secure_cookies {
        set(header::STRICT_TRANSPORT_SECURITY, "max-age=31536000");
    }
    if api {
        set(header::CACHE_CONTROL, "no-store");
    }
    resp
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buckets_drain_and_refill() {
        let l = Limiter::new(1.0, 2);
        let t0 = Instant::now();
        assert!(l.take_at(&"a", t0).is_ok());
        assert!(l.take_at(&"a", t0).is_ok());
        let wait = l.take_at(&"a", t0).unwrap_err();
        assert!(wait > Duration::from_millis(900), "{wait:?}");
        // Other keys are unaffected.
        assert!(l.take_at(&"b", t0).is_ok());
        assert!(l.take_at(&"a", t0 + Duration::from_secs(1)).is_ok());
        assert!(l.take_at(&"a", t0 + Duration::from_secs(1)).is_err());
        // Never more than the burst.
        assert!(l.take_at(&"a", t0 + Duration::from_secs(100)).is_ok());
        assert!(l.take_at(&"a", t0 + Duration::from_secs(100)).is_ok());
        assert!(l.take_at(&"a", t0 + Duration::from_secs(100)).is_err());
    }

    #[test]
    fn forwarded_for_is_believed_only_from_trusted_proxies() {
        let trusted: Vec<Cidr> = vec!["10.0.0.0/8".parse().unwrap(), "::1".parse().unwrap()];
        let mut h = HeaderMap::new();
        h.insert(
            "x-forwarded-for",
            "1.2.3.4, 6.6.6.6, 10.1.1.1".parse().unwrap(),
        );
        let from = |a: &str| client_ip(a.parse().unwrap(), &h, &trusted).to_string();
        // A client may put anything in the header; only the hop the proxy
        // added counts.
        assert_eq!(from("10.0.0.5:1000"), "6.6.6.6");
        assert_eq!(from("[::1]:1000"), "6.6.6.6");
        // Not from a proxy: the header is ignored.
        assert_eq!(from("8.8.8.8:1000"), "8.8.8.8");
        assert!(
            !"10.0.0.0/8"
                .parse::<Cidr>()
                .unwrap()
                .contains("11.0.0.1".parse().unwrap())
        );
        assert!(
            "0.0.0.0/0"
                .parse::<Cidr>()
                .unwrap()
                .contains("11.0.0.1".parse().unwrap())
        );
        assert!(
            "10.0.0.0/8"
                .parse::<Cidr>()
                .unwrap()
                .contains("::ffff:10.2.3.4".parse().unwrap())
        );
        assert!("1.2.3.4/33".parse::<Cidr>().is_err());
    }
}
