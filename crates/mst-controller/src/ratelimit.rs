//! Rate limiting and response hardening for the HTTP API.
//!
//! Limits are token buckets. With `api.redis_url` they are kept in Redis and
//! shared by all controller replicas, so a client's budget is the same
//! whichever replica it reaches, and survives restarts. Without it, or
//! while Redis cannot be reached, each replica keeps its own in memory:
//! behind a load balancer spreading clients over N replicas a client then
//! gets up to N times the budget.
//!
//! - Every request: per client address.
//! - Password logins: per client address, and failures per account, so
//!   guessing one account's password from many addresses is slow too. An
//!   account's budget only drains on failures, and refills, so an attacker
//!   can slow a user's logins down but not lock them out for good.

use std::collections::HashMap;
use std::fmt::Display;
use std::hash::Hash;
use std::net::{IpAddr, SocketAddr};
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::{ConnectInfo, Request, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode, Uri, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use crate::state::AppState;

/// A token bucket per key.
pub struct Limiter<K> {
    /// Names the limit in shared storage.
    name: &'static str,
    /// Tokens added per second.
    rate: f64,
    burst: f64,
    buckets: Mutex<HashMap<K, (f64, Instant)>>,
    shared: Option<Arc<Shared>>,
}

impl<K: Hash + Eq + Clone + Display> Limiter<K> {
    pub fn new(name: &'static str, rate: f64, burst: u32, shared: Option<Arc<Shared>>) -> Self {
        Self {
            name,
            rate,
            burst: burst as f64,
            buckets: Mutex::default(),
            shared,
        }
    }

    /// Takes a token for `key`, or says how long until one is available.
    pub async fn take(&self, key: &K) -> Result<(), Duration> {
        if let Some(r) = self.shared_bucket(key, true).await {
            return r;
        }
        self.take_at(key, Instant::now())
    }

    /// Whether `key` has a token left, without taking it.
    pub async fn check(&self, key: &K) -> Result<(), Duration> {
        if let Some(r) = self.shared_bucket(key, false).await {
            return r;
        }
        let now = Instant::now();
        let buckets = self.buckets.lock().unwrap();
        match buckets.get(key) {
            Some(&(tokens, at)) => self.wait(self.refill(tokens, at, now)),
            None => Ok(()),
        }
    }

    /// The answer from shared storage, if there is one and it answers.
    async fn shared_bucket(&self, key: &K, take: bool) -> Option<Result<(), Duration>> {
        let shared = self.shared.as_ref()?;
        // Keys may name accounts; keep those out of Redis.
        let key = format!(
            "mst:ratelimit:{}:{}",
            self.name,
            mst_pki::hash_secret(&key.to_string())
        );
        shared.bucket(&key, self.rate, self.burst, take).await
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

/// Token buckets in Redis, shared by the replicas. Each is a hash updated
/// atomically by a script, on Redis's clock (replicas' clocks may differ),
/// and expires once it would have refilled.
pub struct Shared {
    client: redis::Client,
    conn: tokio::sync::Mutex<Option<redis::aio::ConnectionManager>>,
    /// After a failed connection attempt, when to try again.
    retry_at: Mutex<Option<Instant>>,
    script: redis::Script,
}

const BUCKET: &str = r"
local rate = tonumber(ARGV[1])
local burst = tonumber(ARGV[2])
local t = redis.call('TIME')
local now = tonumber(t[1]) + tonumber(t[2]) / 1000000
local b = redis.call('HMGET', KEYS[1], 'tokens', 'at')
local tokens = tonumber(b[1]) or burst
local at = tonumber(b[2]) or now
tokens = math.min(burst, tokens + math.max(0, now - at) * rate)
if tokens < 1 then
  return {0, math.ceil((1 - tokens) / rate * 1000)}
end
if ARGV[3] == '1' then
  redis.call('HSET', KEYS[1], 'tokens', tostring(tokens - 1), 'at', tostring(now))
  redis.call('PEXPIRE', KEYS[1], math.ceil(burst / rate * 1000))
end
return {1, 0}
";

impl Shared {
    /// For `redis://` or `rediss://` (TLS) URLs. Connects on first use.
    pub fn new(url: &str) -> anyhow::Result<Arc<Self>> {
        Ok(Arc::new(Self {
            client: redis::Client::open(url)?,
            conn: tokio::sync::Mutex::default(),
            retry_at: Mutex::default(),
            script: redis::Script::new(BUCKET),
        }))
    }

    async fn conn(&self) -> Option<redis::aio::ConnectionManager> {
        let mut slot = self.conn.lock().await;
        if let Some(c) = slot.as_ref() {
            return Some(c.clone());
        }
        if self
            .retry_at
            .lock()
            .unwrap()
            .is_some_and(|t| Instant::now() < t)
        {
            return None;
        }
        let attempt =
            tokio::time::timeout(Duration::from_secs(2), self.client.get_connection_manager())
                .await;
        match attempt {
            Ok(Ok(c)) => {
                tracing::info!("rate limits shared through Redis");
                *slot = Some(c.clone());
                Some(c)
            }
            failed => {
                let error = match failed {
                    Ok(Err(e)) => e.to_string(),
                    _ => "timed out".into(),
                };
                tracing::warn!(%error, "cannot reach Redis; rate limits are per replica meanwhile");
                *self.retry_at.lock().unwrap() = Some(Instant::now() + Duration::from_secs(10));
                None
            }
        }
    }

    async fn bucket(
        &self,
        key: &str,
        rate: f64,
        burst: f64,
        take: bool,
    ) -> Option<Result<(), Duration>> {
        let mut conn = self.conn().await?;
        let mut invocation = self.script.key(key);
        invocation
            .arg(rate)
            .arg(burst)
            .arg(if take { "1" } else { "0" });
        let call = invocation.invoke_async::<(i64, i64)>(&mut conn);
        match tokio::time::timeout(Duration::from_secs(1), call).await {
            Ok(Ok((1, _))) => Some(Ok(())),
            Ok(Ok((_, wait_ms))) => Some(Err(Duration::from_millis(wait_ms.max(1) as u64))),
            failed => {
                let error = match failed {
                    Ok(Err(e)) => e.to_string(),
                    _ => "timed out".into(),
                };
                metrics::counter!("mst_rate_limit_store_errors_total").increment(1);
                tracing::debug!(%error, "Redis rate limit; using this replica's");
                None
            }
        }
    }
}

/// The limits the API enforces.
pub struct Limits {
    pub requests: Limiter<IpAddr>,
    pub logins: Limiter<IpAddr>,
    pub login_failures: Limiter<String>,
}

impl Limits {
    /// Kept in `shared` when given, else in this replica's memory.
    pub fn new(shared: Option<Arc<Shared>>) -> Self {
        Self {
            // Generous: the UI fires a burst of requests per page.
            requests: Limiter::new("requests", 50.0, 300, shared.clone()),
            logins: Limiter::new("logins", 0.5, 30, shared.clone()),
            login_failures: Limiter::new("login-failures", 1.0 / 60.0, 5, shared),
        }
    }
}

/// Networks whose `X-Forwarded-For`, `X-Forwarded-Proto` and
/// `X-Forwarded-Host` are believed (the load balancers).
#[derive(Clone, Debug, serde::Deserialize)]
#[serde(try_from = "String")]
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

impl TryFrom<String> for Cidr {
    type Error = String;

    fn try_from(s: String) -> Result<Self, String> {
        s.parse()
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

/// Whether the client reached us over HTTPS: only a trusted proxy can say
/// so, in the `X-Forwarded-Proto` it adds (the last one). The controller
/// itself serves plain HTTP.
pub fn forwarded_https(peer: SocketAddr, headers: &HeaderMap, trusted: &[Cidr]) -> bool {
    if !trusted.iter().any(|c| c.contains(peer.ip())) {
        return false;
    }
    headers
        .get_all("x-forwarded-proto")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .last()
        .is_some_and(|p| p.trim().eq_ignore_ascii_case("https"))
}

/// Where the client reached us, as `scheme://host[:port]`, for links that
/// must lead back here (the OIDC redirect URI). The host is the one a
/// trusted proxy forwarded (`X-Forwarded-Host`, the last one), or else the
/// request's own. None without a host, or with one that is not a plain name
/// or address and port: it ends up in a URL.
pub fn site_url(
    peer: SocketAddr,
    headers: &HeaderMap,
    uri: &Uri,
    trusted: &[Cidr],
) -> Option<String> {
    let last = |name: &str| {
        headers
            .get_all(name)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .flat_map(|v| v.split(','))
            .last()
            .map(|v| v.trim().to_owned())
    };
    let proxied = trusted.iter().any(|c| c.contains(peer.ip()));
    let host = proxied
        .then(|| last("x-forwarded-host"))
        .flatten()
        .or_else(|| last(header::HOST.as_str()))
        .or_else(|| uri.authority().map(|a| a.as_str().to_owned()))?;
    let plain = |c: char| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | ':' | '[' | ']');
    if host.is_empty() || host.len() > 255 || !host.chars().all(plain) {
        return None;
    }
    let scheme = if forwarded_https(peer, headers, trusted) {
        "https"
    } else {
        "http"
    };
    Some(format!("{scheme}://{host}"))
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
    metrics::counter!("mst_rate_limited_total").increment(1);
    resp
}

/// The client's address, as the rate limits see it.
#[derive(Clone, Copy)]
pub struct ClientIp(pub IpAddr);

/// Whether the client reached us over HTTPS (see `forwarded_https`).
#[derive(Clone, Copy)]
pub struct Https(pub bool);

/// Where the client reached us (see `site_url`).
#[derive(Clone)]
pub struct Site(pub Option<String>);

impl Site {
    /// The host name the client used, in lower case, without port or
    /// trailing dot (`mst.acme.com`): what tenant domains are bound to.
    pub fn host(&self) -> Option<String> {
        let authority = self.0.as_deref()?.split_once("://")?.1;
        let host = match authority.strip_prefix('[') {
            Some(v6) => v6.split(']').next()?,
            None => authority.split(':').next()?,
        };
        Some(host.trim_end_matches('.').to_ascii_lowercase()).filter(|h| !h.is_empty())
    }

    /// This site's URL with another host name, keeping scheme and port
    /// (`https://mst.acme.com`): where the same controllers answer under
    /// that name.
    pub fn with_host(&self, host: &str) -> Option<String> {
        let (scheme, authority) = self.0.as_deref()?.split_once("://")?;
        let after_host = match authority.strip_prefix('[') {
            Some(v6) => v6.split_once(']')?.1,
            None => authority.find(':').map_or("", |i| &authority[i..]),
        };
        let port = after_host.strip_prefix(':').map(|p| format!(":{p}"));
        Some(format!("{scheme}://{host}{}", port.unwrap_or_default()))
    }
}

/// Limits requests per client, and records for handlers the client's
/// address, whether it came over HTTPS and where it reached us.
pub async fn limit(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    mut req: Request,
    next: Next,
) -> Response {
    let ip = client_ip(peer, req.headers(), &state.trusted_proxies);
    if let Err(retry) = state.limits.requests.take(&ip).await {
        return too_many(retry);
    }
    let https = forwarded_https(peer, req.headers(), &state.trusted_proxies);
    let site = site_url(peer, req.headers(), req.uri(), &state.trusted_proxies);
    req.extensions_mut().insert(ClientIp(ip));
    req.extensions_mut().insert(Https(https));
    req.extensions_mut().insert(Site(site));
    next.run(req).await
}

/// Headers that keep browsers from misusing what the controller serves.
pub async fn harden(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    req: Request,
    next: Next,
) -> Response {
    let api = req.uri().path().starts_with("/api/");
    let https = forwarded_https(peer, req.headers(), &state.trusted_proxies);
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
    if https {
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
        let l = Limiter::new("test", 1.0, 2, None);
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

    /// Needs a Redis server: `MST_TEST_REDIS_URL=redis://127.0.0.1:56379`
    /// (deploy/dev/compose.yaml).
    #[tokio::test]
    async fn replicas_share_limits_through_redis() {
        let Ok(url) = std::env::var("MST_TEST_REDIS_URL") else {
            eprintln!("MST_TEST_REDIS_URL not set; skipping");
            return;
        };
        // Two replicas, one budget.
        let a = Limiter::new("test", 1.0, 3, Some(Shared::new(&url).unwrap()));
        let b = Limiter::new("test", 1.0, 3, Some(Shared::new(&url).unwrap()));
        let key = uuid::Uuid::now_v7().to_string();
        assert!(a.take(&key).await.is_ok());
        assert!(b.take(&key).await.is_ok());
        assert!(a.check(&key).await.is_ok());
        assert!(a.take(&key).await.is_ok());
        let wait = b.take(&key).await.unwrap_err();
        assert!(wait <= Duration::from_secs(1), "{wait:?}");
        assert!(a.check(&key).await.is_err());
        // Nothing was kept in memory meanwhile.
        assert!(a.buckets.lock().unwrap().is_empty());

        // Without Redis, each replica limits on its own.
        let down = Limiter::new(
            "test",
            1.0,
            1,
            Some(Shared::new("redis://127.0.0.1:1").unwrap()),
        );
        assert!(down.take(&key).await.is_ok());
        assert!(down.take(&key).await.is_err());
    }

    #[test]
    fn forwarded_proto_is_believed_only_from_trusted_proxies() {
        let trusted: Vec<Cidr> = vec!["10.0.0.0/8".parse().unwrap()];
        let https = |peer: &str, values: &[&str]| {
            let mut h = HeaderMap::new();
            for v in values {
                h.append("x-forwarded-proto", v.parse().unwrap());
            }
            forwarded_https(peer.parse().unwrap(), &h, &trusted)
        };
        assert!(https("10.0.0.5:1000", &["https"]));
        assert!(https("10.0.0.5:1000", &["HTTPS"]));
        assert!(!https("10.0.0.5:1000", &["http"]));
        assert!(!https("10.0.0.5:1000", &[]));
        // A client's own header comes first; the proxy's (last) counts.
        assert!(!https("10.0.0.5:1000", &["https, http"]));
        assert!(!https("10.0.0.5:1000", &["https", "http"]));
        assert!(https("10.0.0.5:1000", &["http", "https"]));
        // Not from a proxy: the header is ignored.
        assert!(!https("8.8.8.8:1000", &["https"]));
    }

    #[test]
    fn site_url_comes_from_the_request_or_a_trusted_proxy() {
        let trusted: Vec<Cidr> = vec!["10.0.0.0/8".parse().unwrap()];
        let site = |peer: &str, headers: &[(&str, &str)]| {
            let mut h = HeaderMap::new();
            for (k, v) in headers {
                h.append(
                    HeaderName::from_bytes(k.as_bytes()).unwrap(),
                    v.parse().unwrap(),
                );
            }
            site_url(peer.parse().unwrap(), &h, &Uri::from_static("/"), &trusted)
        };
        let direct = "192.0.2.1:1000";
        let proxy = "10.0.0.5:1000";
        assert_eq!(
            site(direct, &[("host", "127.0.0.1:18080")]).as_deref(),
            Some("http://127.0.0.1:18080")
        );
        assert_eq!(
            site(direct, &[("host", "[::1]:8080")]).as_deref(),
            Some("http://[::1]:8080")
        );
        // Only a trusted proxy can say where the client went, and how.
        let forwarded = [
            ("host", "10.0.0.7:8080"),
            ("x-forwarded-host", "cloud.example.net"),
            ("x-forwarded-proto", "https"),
        ];
        assert_eq!(
            site(proxy, &forwarded).as_deref(),
            Some("https://cloud.example.net")
        );
        assert_eq!(
            site(direct, &forwarded).as_deref(),
            Some("http://10.0.0.7:8080")
        );
        assert_eq!(
            site(
                proxy,
                &[
                    ("host", "cloud.example.net"),
                    ("x-forwarded-proto", "https")
                ]
            )
            .as_deref(),
            Some("https://cloud.example.net")
        );
        // Nothing that would change the URL's shape.
        for bad in ["evil.example/x?", "a@b", "a b", "", "x\\y"] {
            assert_eq!(site(direct, &[("host", bad)]), None, "{bad:?}");
        }
        assert_eq!(site(direct, &[]), None);
        let host = |url: &str| Site(Some(url.to_owned())).host();
        assert_eq!(
            host("https://Mst.Acme.com").as_deref(),
            Some("mst.acme.com")
        );
        assert_eq!(
            host("http://mst.acme.com.:8080").as_deref(),
            Some("mst.acme.com")
        );
        assert_eq!(host("http://[::1]:8080").as_deref(), Some("::1"));
        assert_eq!(Site(None).host(), None);
        let moved = |url: &str| Site(Some(url.to_owned())).with_host("mst.acme.com");
        assert_eq!(
            moved("https://cloud.example.net").as_deref(),
            Some("https://mst.acme.com")
        );
        assert_eq!(
            moved("http://127.0.0.1:18080").as_deref(),
            Some("http://mst.acme.com:18080")
        );
        assert_eq!(
            moved("http://[::1]:8080").as_deref(),
            Some("http://mst.acme.com:8080")
        );
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
