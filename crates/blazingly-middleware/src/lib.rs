#![forbid(unsafe_code)]

//! Runtime-neutral production HTTP middleware.
//!
//! Every layer is synchronous and thread-local by construction. Native,
//! in-memory, and future Worker adapters therefore share the same behavior
//! without taking a dependency on Tokio.

use blazingly_core::{BodyStream, BodyStreamError, HttpMethod, StreamingBody};
use blazingly_http::{HttpMiddleware, HttpRequestContext, HttpRequestView, Response};
use brotli::CompressorWriter;
use flate2::Compression as GzipLevel;
use flate2::write::GzEncoder;
use serde_json::json;
use std::borrow::Cow;
use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::OsStr;
use std::fmt;
use std::fs;
use std::future::Future;
use std::io::{Read, Seek, SeekFrom, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::path::{Path, PathBuf};
use std::pin::{Pin, pin};
use std::rc::Rc;
use std::str::FromStr;
use std::task::{Context, Poll};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Cross-origin request policy and preflight handler.
#[derive(Clone, Debug)]
pub struct Cors {
    origins: Vec<String>,
    methods: Vec<HttpMethod>,
    headers: Vec<String>,
    expose_headers: Vec<String>,
    allow_credentials: bool,
    max_age: Option<Duration>,
}

impl Cors {
    /// Starts with no cross-origin access.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            origins: Vec::new(),
            methods: Vec::new(),
            headers: Vec::new(),
            expose_headers: Vec::new(),
            allow_credentials: false,
            max_age: None,
        }
    }

    /// Allows every origin, method, and requested header.
    #[must_use]
    pub fn permissive() -> Self {
        Self::new()
            .allow_origin("*")
            .allow_methods([
                HttpMethod::Get,
                HttpMethod::Head,
                HttpMethod::Post,
                HttpMethod::Put,
                HttpMethod::Patch,
                HttpMethod::Delete,
                HttpMethod::Options,
            ])
            .allow_header("*")
    }

    /// Allows an exact origin, `*`, or a wildcard subdomain pattern such as
    /// `https://*.example.com`.
    ///
    /// # Panics
    ///
    /// Panics when `*` is combined with credentialed requests, which browsers
    /// reject and which would otherwise expose every origin.
    #[must_use]
    pub fn allow_origin(mut self, origin: impl Into<String>) -> Self {
        self.origins.push(origin.into());
        self.assert_origin_policy();
        self
    }

    /// Allows the supplied preflight methods.
    #[must_use]
    pub fn allow_methods(mut self, methods: impl IntoIterator<Item = HttpMethod>) -> Self {
        self.methods.extend(methods);
        self
    }

    /// Allows one request header name, or `*` for any requested header.
    #[must_use]
    pub fn allow_header(mut self, header: impl Into<String>) -> Self {
        self.headers.push(header.into().to_ascii_lowercase());
        self
    }

    /// Exposes one response header to the calling script.
    #[must_use]
    pub fn expose_header(mut self, header: impl Into<String>) -> Self {
        self.expose_headers.push(header.into());
        self
    }

    /// Exposes several response headers to the calling script.
    #[must_use]
    pub fn expose_headers(mut self, headers: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.expose_headers
            .extend(headers.into_iter().map(Into::into));
        self
    }

    /// Allows credentialed cross-origin requests.
    ///
    /// # Panics
    ///
    /// Panics when a wildcard origin has already been allowed.
    #[must_use]
    pub fn allow_credentials(mut self, allow: bool) -> Self {
        self.allow_credentials = allow;
        self.assert_origin_policy();
        self
    }

    /// Sets how long a browser may cache a preflight result.
    #[must_use]
    pub const fn max_age(mut self, max_age: Duration) -> Self {
        self.max_age = Some(max_age);
        self
    }

    fn assert_origin_policy(&self) {
        assert!(
            !(self.allow_credentials && self.has_wildcard_origin()),
            "CORS credentials cannot be combined with the wildcard origin"
        );
    }

    fn has_wildcard_origin(&self) -> bool {
        self.origins.iter().any(|allowed| allowed == "*")
    }

    fn has_wildcard_headers(&self) -> bool {
        self.headers.iter().any(|header| header == "*")
    }

    fn allowed_origin<'origin>(&self, origin: &'origin str) -> Option<&'origin str> {
        self.origins
            .iter()
            .any(|allowed| origin_matches(allowed, origin))
            .then_some(origin)
    }

    fn allows_method(&self, method: &str) -> bool {
        self.methods
            .iter()
            .any(|allowed| allowed.as_str().eq_ignore_ascii_case(method))
    }

    fn allows_headers(&self, requested: &str) -> bool {
        let wildcard = self.has_wildcard_headers();
        requested
            .split(',')
            .map(str::trim)
            .filter(|header| !header.is_empty())
            .all(|header| {
                wildcard
                    || self
                        .headers
                        .iter()
                        .any(|allowed| allowed.eq_ignore_ascii_case(header))
            })
    }

    fn intercept(&self, request: &dyn HttpRequestView) -> Option<Response> {
        if request.header_value("origin", 1).is_some() {
            return Some(json_error(
                400,
                "cors_origin_ambiguous",
                "request carries conflicting origin headers",
            ));
        }
        let origin = request.header_value("origin", 0)?;
        let Some(origin) = self.allowed_origin(origin) else {
            return (request.method() == HttpMethod::Options)
                .then(|| json_error(403, "cors_origin_denied", "request origin is not allowed"));
        };
        if request.method() != HttpMethod::Options {
            return None;
        }
        let requested_method = request.header_value("access-control-request-method", 0)?;
        if !self.allows_method(requested_method) {
            return Some(json_error(
                403,
                "cors_method_denied",
                "requested CORS method is not allowed",
            ));
        }
        let requested_headers = joined_header(request, "access-control-request-headers");
        if let Some(requested) = requested_headers.as_deref()
            && !self.allows_headers(requested)
        {
            return Some(json_error(
                403,
                "cors_headers_denied",
                "requested CORS headers are not allowed",
            ));
        }
        let mut response = Response::empty(204);
        self.apply_headers(&mut response, origin);
        self.apply_preflight_headers(&mut response, requested_headers.as_deref());
        Some(response)
    }

    fn apply_headers(&self, response: &mut Response, origin: &str) {
        let allow_origin = if self.has_wildcard_origin() && !self.allow_credentials {
            "*"
        } else {
            append_vary(response, "origin");
            origin
        };
        response.set_header("access-control-allow-origin", allow_origin);
        if self.allow_credentials {
            response.set_header("access-control-allow-credentials", "true");
        }
        if !self.expose_headers.is_empty() {
            response.set_header(
                "access-control-expose-headers",
                self.expose_headers.join(", "),
            );
        }
    }

    fn apply_preflight_headers(&self, response: &mut Response, requested_headers: Option<&str>) {
        if !self.methods.is_empty() {
            response.set_header(
                "access-control-allow-methods",
                self.methods
                    .iter()
                    .map(|method| method.as_str())
                    .collect::<Vec<_>>()
                    .join(", "),
            );
        }
        if let Some(allow_headers) = self.preflight_headers(requested_headers) {
            response.set_header("access-control-allow-headers", allow_headers);
        }
        if let Some(max_age) = self.max_age {
            response.set_header("access-control-max-age", max_age.as_secs().to_string());
        }
        append_vary(response, "access-control-request-method");
        append_vary(response, "access-control-request-headers");
    }

    /// Echoes the concretely requested headers so a wildcard configuration
    /// still works for credentialed requests.
    fn preflight_headers(&self, requested: Option<&str>) -> Option<String> {
        if let Some(requested) = requested {
            let echoed = requested
                .split(',')
                .map(str::trim)
                .filter(|header| !header.is_empty())
                .collect::<Vec<_>>();
            if !echoed.is_empty() {
                return Some(echoed.join(", "));
            }
        }
        if self.headers.is_empty() {
            return None;
        }
        if self.has_wildcard_headers() {
            return (!self.allow_credentials).then(|| "*".to_owned());
        }
        Some(self.headers.join(", "))
    }
}

impl Default for Cors {
    fn default() -> Self {
        Self::new()
    }
}

impl HttpMiddleware for Cors {
    fn on_request(&self, context: &mut HttpRequestContext<'_>) -> Option<Response> {
        self.intercept(context.request())
    }

    fn on_response(
        &self,
        context: &HttpRequestContext<'_>,
        _operation: Option<&blazingly_core::OperationDescriptor>,
        response: &mut Response,
    ) {
        let request = context.request();
        if request.header_value("origin", 1).is_some() {
            return;
        }
        let Some(origin) = request.header_value("origin", 0) else {
            return;
        };
        if self.allowed_origin(origin).is_some() {
            self.apply_headers(response, origin);
        }
    }
}

/// Host-header allowlist protecting absolute-URL generation and routing.
#[derive(Clone, Debug)]
pub struct TrustedHost {
    patterns: Vec<String>,
    allow_missing: bool,
}

impl TrustedHost {
    #[must_use]
    pub fn new(patterns: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self {
            patterns: patterns
                .into_iter()
                .map(|pattern| pattern.into().to_ascii_lowercase())
                .collect(),
            allow_missing: false,
        }
    }

    #[must_use]
    pub const fn allow_missing(mut self, allow: bool) -> Self {
        self.allow_missing = allow;
        self
    }

    fn allows(&self, host: &str) -> bool {
        let host = normalized_host(host);
        self.patterns.iter().any(|pattern| {
            pattern == "*"
                || pattern.eq_ignore_ascii_case(host)
                || pattern
                    .strip_prefix("*.")
                    .is_some_and(|suffix| wildcard_host_matches(host, suffix))
        })
    }
}

impl HttpMiddleware for TrustedHost {
    fn on_request(&self, context: &mut HttpRequestContext<'_>) -> Option<Response> {
        match context.host() {
            Some(host) if self.allows(host) => None,
            None if self.allow_missing => None,
            Some(_) => Some(json_error(
                400,
                "untrusted_host",
                "request host is not trusted",
            )),
            None => Some(json_error(
                400,
                "missing_host",
                "request host header is required",
            )),
        }
    }
}

/// An IPv4 or IPv6 CIDR used to identify trusted reverse proxies.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IpNetwork {
    address: IpAddr,
    prefix: u8,
}

impl IpNetwork {
    #[must_use]
    pub const fn new(address: IpAddr, prefix: u8) -> Option<Self> {
        let valid = match address {
            IpAddr::V4(_) => prefix <= 32,
            IpAddr::V6(_) => prefix <= 128,
        };
        if valid {
            Some(Self { address, prefix })
        } else {
            None
        }
    }

    #[must_use]
    pub fn contains(self, candidate: IpAddr) -> bool {
        match (self.address, candidate) {
            (IpAddr::V4(network), IpAddr::V4(candidate)) => {
                masked_v4(network, self.prefix) == masked_v4(candidate, self.prefix)
            }
            (IpAddr::V6(network), IpAddr::V6(candidate)) => {
                masked_v6(network, self.prefix) == masked_v6(candidate, self.prefix)
            }
            _ => false,
        }
    }
}

impl FromStr for IpNetwork {
    type Err = &'static str;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let (address, prefix) = value.split_once('/').ok_or("CIDR must contain a prefix")?;
        let address = address.parse().map_err(|_| "invalid IP address")?;
        let prefix = prefix.parse().map_err(|_| "invalid CIDR prefix")?;
        Self::new(address, prefix).ok_or("CIDR prefix is out of range")
    }
}

/// Applies RFC 7239 and `X-Forwarded-*` values only from trusted peers.
#[derive(Clone, Debug, Default)]
pub struct ProxyHeaders {
    trusted: Vec<IpNetwork>,
    trust_all: bool,
}

impl ProxyHeaders {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            trusted: Vec::new(),
            trust_all: false,
        }
    }

    #[must_use]
    pub fn trust(mut self, network: IpNetwork) -> Self {
        self.trusted.push(network);
        self
    }

    /// Trusts the surrounding proxy unconditionally. This is appropriate for
    /// isolated sidecar/listener deployments, but not for an Internet-facing
    /// socket.
    #[must_use]
    pub const fn trust_all(mut self) -> Self {
        self.trust_all = true;
        self
    }

    fn is_trusted(&self, address: IpAddr) -> bool {
        self.trust_all || self.trusted.iter().any(|network| network.contains(address))
    }

    fn peer_is_trusted(&self, context: &HttpRequestContext<'_>) -> bool {
        self.trust_all || context.client_ip().is_some_and(|ip| self.is_trusted(ip))
    }
}

impl HttpMiddleware for ProxyHeaders {
    fn on_request(&self, context: &mut HttpRequestContext<'_>) -> Option<Response> {
        if !self.peer_is_trusted(context) {
            return None;
        }
        let request = context.request();
        let forwarded = joined_header(request, "forwarded").map(Cow::into_owned);
        let x_forwarded_for = joined_header(request, "x-forwarded-for").map(Cow::into_owned);
        let x_forwarded_proto = joined_header(request, "x-forwarded-proto").map(Cow::into_owned);
        let x_forwarded_host = joined_header(request, "x-forwarded-host").map(Cow::into_owned);
        let forwarded_for = forwarded
            .as_deref()
            .map(forwarded_for_values)
            .filter(|values| !values.is_empty())
            .unwrap_or_else(|| x_forwarded_for.as_deref().map(ip_list).unwrap_or_default());
        if let Some(peer) = context.client_ip()
            && let Some(client) = trusted_client_ip(peer, &forwarded_for, |ip| self.is_trusted(ip))
        {
            context.set_client_ip(client);
        } else if self.trust_all
            && let Some(client) = forwarded_for.first().copied()
        {
            context.set_client_ip(client);
        }

        let proto = forwarded
            .as_deref()
            .and_then(|value| forwarded_parameter(value, "proto"))
            .or_else(|| first_list_value(x_forwarded_proto.as_deref()));
        if let Some(proto) = proto
            && valid_scheme(proto)
        {
            context.set_scheme(proto.to_ascii_lowercase());
        }

        let host = forwarded
            .as_deref()
            .and_then(|value| forwarded_parameter(value, "host"))
            .or_else(|| first_list_value(x_forwarded_host.as_deref()));
        if let Some(host) = host
            && valid_forwarded_host(host)
        {
            context.set_host(host.to_owned());
        }
        None
    }
}

/// A token-bucket quota expressed as `capacity` requests per window.
#[derive(Clone, Copy, Debug)]
pub struct RateLimitQuota {
    capacity: u32,
    refill_per_second: f64,
}

impl RateLimitQuota {
    /// Builds a quota allowing `capacity` requests per `window`.
    #[must_use]
    pub fn new(capacity: u32, window: Duration) -> Self {
        let capacity = capacity.max(1);
        let seconds = window.as_secs_f64().max(0.001);
        Self {
            capacity,
            refill_per_second: f64::from(capacity) / seconds,
        }
    }

    /// Maximum burst size.
    #[must_use]
    pub const fn capacity(self) -> u32 {
        self.capacity
    }

    /// Tokens restored per second.
    #[must_use]
    pub const fn refill_per_second(self) -> f64 {
        self.refill_per_second
    }
}

/// The outcome of consuming one token from a rate-limit bucket.
#[derive(Clone, Copy, Debug)]
pub struct RateLimitDecision {
    allowed: bool,
    remaining: u32,
    retry_after: Duration,
}

impl RateLimitDecision {
    /// Accepts the request with `remaining` tokens left.
    #[must_use]
    pub const fn allow(remaining: u32) -> Self {
        Self {
            allowed: true,
            remaining,
            retry_after: Duration::ZERO,
        }
    }

    /// Rejects the request until `retry_after` has elapsed.
    #[must_use]
    pub const fn deny(retry_after: Duration) -> Self {
        Self {
            allowed: false,
            remaining: 0,
            retry_after,
        }
    }

    /// Whether the request may proceed.
    #[must_use]
    pub const fn allowed(self) -> bool {
        self.allowed
    }

    /// Tokens left in the bucket.
    #[must_use]
    pub const fn remaining(self) -> u32 {
        self.remaining
    }

    /// How long the caller should wait before retrying.
    #[must_use]
    pub const fn retry_after(self) -> Duration {
        self.retry_after
    }
}

/// Backing store for rate-limit buckets.
///
/// [`MemoryRateLimitStore`] is the in-process default; a distributed backend
/// implements the same seam over shared storage.
pub trait RateLimitStore {
    /// Consumes one token for `key` and reports the resulting decision.
    fn consume(&self, key: &str, quota: RateLimitQuota, now: Instant) -> RateLimitDecision;

    /// Number of buckets currently tracked, for diagnostics and tests.
    fn tracked_keys(&self) -> usize {
        0
    }
}

/// In-process bucket store with TTL eviction and a bounded key count.
#[derive(Debug)]
pub struct MemoryRateLimitStore {
    max_keys: usize,
    idle_ttl: Duration,
    table: RefCell<BucketTable>,
}

impl MemoryRateLimitStore {
    /// Tracks at most `max_keys` buckets and evicts buckets idle for longer
    /// than `idle_ttl`.
    #[must_use]
    pub fn new(max_keys: usize, idle_ttl: Duration) -> Self {
        Self {
            max_keys: max_keys.max(1),
            idle_ttl,
            table: RefCell::new(BucketTable::default()),
        }
    }
}

impl RateLimitStore for MemoryRateLimitStore {
    fn consume(&self, key: &str, quota: RateLimitQuota, now: Instant) -> RateLimitDecision {
        let mut table = self.table.borrow_mut();
        table.sweep(now, self.idle_ttl, RATE_LIMIT_SWEEP_BUDGET);
        let slot = if let Some(slot) = table.slot_of(key) {
            table.touch(slot);
            slot
        } else {
            while table.len() >= self.max_keys && table.evict_oldest() {}
            table.insert(
                key,
                Bucket {
                    tokens: f64::from(quota.capacity),
                    updated: now,
                },
            )
        };
        consume_bucket(table.bucket_mut(slot), quota, now)
    }

    fn tracked_keys(&self) -> usize {
        self.table.borrow().len()
    }
}

#[derive(Clone, Copy, Debug)]
struct Bucket {
    tokens: f64,
    updated: Instant,
}

#[derive(Debug)]
struct BucketSlot {
    key: Rc<str>,
    bucket: Bucket,
    older: Option<usize>,
    newer: Option<usize>,
}

/// Buckets in a slab, linked from least to most recently used so both TTL
/// sweeping and capacity eviction are constant work per request.
#[derive(Debug, Default)]
struct BucketTable {
    index: HashMap<Rc<str>, usize>,
    slots: Vec<Option<BucketSlot>>,
    free: Vec<usize>,
    oldest: Option<usize>,
    newest: Option<usize>,
}

impl BucketTable {
    fn len(&self) -> usize {
        self.index.len()
    }

    fn slot_of(&self, key: &str) -> Option<usize> {
        self.index.get(key).copied()
    }

    fn entry(&self, slot: usize) -> &BucketSlot {
        self.slots[slot].as_ref().expect("occupied bucket slot")
    }

    fn entry_mut(&mut self, slot: usize) -> &mut BucketSlot {
        self.slots[slot].as_mut().expect("occupied bucket slot")
    }

    fn bucket_mut(&mut self, slot: usize) -> &mut Bucket {
        &mut self.entry_mut(slot).bucket
    }

    fn touch(&mut self, slot: usize) {
        self.unlink(slot);
        self.link_newest(slot);
    }

    fn insert(&mut self, key: &str, bucket: Bucket) -> usize {
        let key: Rc<str> = Rc::from(key);
        let entry = BucketSlot {
            key: Rc::clone(&key),
            bucket,
            older: None,
            newer: None,
        };
        let slot = if let Some(slot) = self.free.pop() {
            self.slots[slot] = Some(entry);
            slot
        } else {
            self.slots.push(Some(entry));
            self.slots.len() - 1
        };
        self.index.insert(key, slot);
        self.link_newest(slot);
        slot
    }

    fn sweep(&mut self, now: Instant, idle_ttl: Duration, budget: usize) {
        for _ in 0..budget {
            let Some(slot) = self.oldest else {
                return;
            };
            if now.saturating_duration_since(self.entry(slot).bucket.updated) < idle_ttl {
                return;
            }
            self.evict_oldest();
        }
    }

    fn evict_oldest(&mut self) -> bool {
        let Some(slot) = self.oldest else {
            return false;
        };
        self.unlink(slot);
        let entry = self.slots[slot].take().expect("occupied bucket slot");
        self.index.remove(&entry.key);
        self.free.push(slot);
        true
    }

    fn unlink(&mut self, slot: usize) {
        let (older, newer) = {
            let entry = self.entry(slot);
            (entry.older, entry.newer)
        };
        match older {
            Some(older) => self.entry_mut(older).newer = newer,
            None => self.oldest = newer,
        }
        match newer {
            Some(newer) => self.entry_mut(newer).older = older,
            None => self.newest = older,
        }
        let entry = self.entry_mut(slot);
        entry.older = None;
        entry.newer = None;
    }

    fn link_newest(&mut self, slot: usize) {
        let previous = self.newest;
        let entry = self.entry_mut(slot);
        entry.older = previous;
        entry.newer = None;
        match previous {
            Some(previous) => self.entry_mut(previous).newer = Some(slot),
            None => self.oldest = Some(slot),
        }
        self.newest = Some(slot);
    }
}

/// Token-bucket rate limiter with bounded, evicting per-client state.
pub struct RateLimit {
    quota: RateLimitQuota,
    scope: RateLimitScope,
    max_clients: usize,
    idle_ttl: Duration,
    store: Rc<dyn RateLimitStore>,
}

type RateLimitKeyFn = Box<dyn Fn(&HttpRequestContext<'_>) -> Option<String>>;

enum RateLimitScope {
    Global,
    ClientIp,
    Custom(RateLimitKeyFn),
}

impl RateLimit {
    /// Applies one shared bucket to every request.
    #[must_use]
    pub fn global(capacity: u32, window: Duration) -> Self {
        Self::new(capacity, window, RateLimitScope::Global)
    }

    /// Applies one bucket per effective client IP.
    #[must_use]
    pub fn per_client(capacity: u32, window: Duration) -> Self {
        Self::new(capacity, window, RateLimitScope::ClientIp)
    }

    /// Applies one bucket per caller-supplied key, such as an API key or user
    /// id. Requests without a key share the global bucket.
    #[must_use]
    pub fn keyed<F>(capacity: u32, window: Duration, key: F) -> Self
    where
        F: Fn(&HttpRequestContext<'_>) -> Option<String> + 'static,
    {
        Self::new(capacity, window, RateLimitScope::Custom(Box::new(key)))
    }

    fn new(capacity: u32, window: Duration, scope: RateLimitScope) -> Self {
        // A bucket idle for a full window is indistinguishable from a fresh
        // one, so the window is the shortest lossless idle TTL.
        let idle_ttl = window.max(Duration::from_secs(1));
        Self {
            quota: RateLimitQuota::new(capacity, window),
            scope,
            max_clients: DEFAULT_MAX_CLIENTS,
            idle_ttl,
            store: Rc::new(MemoryRateLimitStore::new(DEFAULT_MAX_CLIENTS, idle_ttl)),
        }
    }

    /// Bounds tracked buckets; the least recently used bucket is evicted when
    /// the bound is reached. Call before [`Self::with_store`].
    #[must_use]
    pub fn max_clients(mut self, max_clients: usize) -> Self {
        self.max_clients = max_clients.max(1);
        self.store = Rc::new(MemoryRateLimitStore::new(self.max_clients, self.idle_ttl));
        self
    }

    /// Evicts buckets idle for longer than `idle_ttl`. Call before
    /// [`Self::with_store`].
    #[must_use]
    pub fn idle_ttl(mut self, idle_ttl: Duration) -> Self {
        self.idle_ttl = idle_ttl;
        self.store = Rc::new(MemoryRateLimitStore::new(self.max_clients, self.idle_ttl));
        self
    }

    /// Replaces the default in-process store, for example with a shared
    /// distributed backend.
    #[must_use]
    pub fn with_store(mut self, store: Rc<dyn RateLimitStore>) -> Self {
        self.store = store;
        self
    }

    /// Buckets currently tracked by the configured store.
    #[must_use]
    pub fn tracked_keys(&self) -> usize {
        self.store.tracked_keys()
    }

    fn key(&self, context: &HttpRequestContext<'_>) -> Cow<'static, str> {
        match &self.scope {
            RateLimitScope::Global => Cow::Borrowed(GLOBAL_RATE_LIMIT_KEY),
            RateLimitScope::ClientIp => context
                .client_ip()
                .map_or(Cow::Borrowed(GLOBAL_RATE_LIMIT_KEY), |ip| {
                    Cow::Owned(ip.to_string())
                }),
            RateLimitScope::Custom(extract) => {
                extract(context).map_or(Cow::Borrowed(GLOBAL_RATE_LIMIT_KEY), Cow::Owned)
            }
        }
    }
}

impl fmt::Debug for RateLimit {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RateLimit")
            .field("quota", &self.quota)
            .field("scope", &self.scope)
            .field("max_clients", &self.max_clients)
            .field("idle_ttl", &self.idle_ttl)
            .finish_non_exhaustive()
    }
}

impl fmt::Debug for RateLimitScope {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Global => "Global",
            Self::ClientIp => "ClientIp",
            Self::Custom(_) => "Custom",
        })
    }
}

impl HttpMiddleware for RateLimit {
    fn on_request(&self, context: &mut HttpRequestContext<'_>) -> Option<Response> {
        let key = self.key(context);
        let decision = self.store.consume(&key, self.quota, Instant::now());
        if decision.allowed {
            return None;
        }
        Some(
            json_error(429, "rate_limit_exceeded", "request rate limit exceeded")
                .with_header(
                    "retry-after",
                    decision.retry_after.as_secs().max(1).to_string(),
                )
                .with_header("x-ratelimit-limit", self.quota.capacity.to_string())
                .with_header("x-ratelimit-remaining", decision.remaining.to_string()),
        )
    }
}

/// Negotiated buffered response compression using Brotli or `GZip`.
#[derive(Clone, Copy, Debug)]
pub struct Compression {
    minimum_size: usize,
    gzip_level: u32,
    brotli_quality: u32,
}

impl Compression {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            minimum_size: 1024,
            gzip_level: 6,
            brotli_quality: 5,
        }
    }

    #[must_use]
    pub const fn minimum_size(mut self, bytes: usize) -> Self {
        self.minimum_size = bytes;
        self
    }

    #[must_use]
    pub const fn gzip_level(mut self, level: u32) -> Self {
        self.gzip_level = level;
        self
    }

    #[must_use]
    pub const fn brotli_quality(mut self, quality: u32) -> Self {
        self.brotli_quality = quality;
        self
    }

    /// Wraps a pull-based body in a chunk-wise encoder for `encoding`.
    ///
    /// Every source chunk is written, flushed out of the encoder, and yielded
    /// before the next chunk is pulled, so a live stream such as Server-Sent
    /// Events is delivered event by event instead of sitting in the encoder's
    /// buffer. The encoder trailer is emitted as a final chunk when the source
    /// ends. A producer error terminates the compressed stream without that
    /// trailer, so a truncated body never decodes as a complete one.
    ///
    /// The caller owns the `Content-Encoding` and `Vary` headers.
    /// [`Compression::on_response`] applies this automatically to a streaming
    /// response; this method is public so a handler can encode a stream it
    /// builds itself.
    #[must_use]
    pub fn compress_stream(
        &self,
        source: StreamingBody,
        encoding: ContentEncoding,
    ) -> StreamingBody {
        StreamingBody::new(CompressedStream {
            source,
            encoder: Some(ChunkEncoder::new(
                encoding,
                self.gzip_level,
                self.brotli_quality,
            )),
        })
    }

    /// Wraps a streamed response body in a chunk-wise encoder.
    ///
    /// The buffered path's `minimum_size` check cannot apply here: the length
    /// is unknown before the producer finishes, and waiting for it would defeat
    /// streaming. Every other guard is the same, and the encoder flushes per
    /// chunk so a live stream such as SSE stays live.
    fn compress_streaming(
        &self,
        method: HttpMethod,
        accepted: Option<&str>,
        response: &mut Response,
    ) {
        if method == HttpMethod::Head
            || response.status() < 200
            || matches!(response.status(), 204 | 206 | 304)
            || response.get_header("content-encoding").is_some()
            || response
                .get_header("cache-control")
                .is_some_and(|value| value.split(',').any(|part| part.trim() == "no-transform"))
            || !compressible(response.get_header("content-type"))
        {
            return;
        }
        let Some(encoding) = preferred_encoding(accepted) else {
            return;
        };
        let Some(source) = response.take_body_stream() else {
            return;
        };
        response.set_body_stream(self.compress_stream(source, encoding));
        // An encoded stream has no length known in advance, so a stale
        // `content-length` would frame the response incorrectly.
        response.remove_header("content-length");
        response.set_header("content-encoding", encoding.as_str());
        append_vary(response, "accept-encoding");
        weaken_validator(response);
    }

    fn compress_buffered(
        &self,
        method: HttpMethod,
        accepted: Option<&str>,
        response: &mut Response,
    ) {
        if method == HttpMethod::Head
            || response.is_streaming()
            || response.body().len() < self.minimum_size
            || response.status() < 200
            // 206 carries a Content-Range measured in the selected
            // representation's bytes, which re-encoding would invalidate.
            || matches!(response.status(), 204 | 206 | 304)
            || response.get_header("content-encoding").is_some()
            || response
                .get_header("cache-control")
                .is_some_and(|value| value.split(',').any(|part| part.trim() == "no-transform"))
            || !compressible(response.get_header("content-type"))
        {
            return;
        }
        let Some(encoding) = preferred_encoding(accepted) else {
            return;
        };
        let compressed = match encoding {
            ContentEncoding::Brotli => compress_brotli(response.body(), self.brotli_quality),
            ContentEncoding::Gzip => compress_gzip(response.body(), self.gzip_level),
        };
        let Ok(compressed) = compressed else {
            return;
        };
        if compressed.len() >= response.body().len() {
            return;
        }
        if response.replace_body(compressed) {
            response.remove_header("content-length");
            response.set_header("content-encoding", encoding.as_str());
            append_vary(response, "accept-encoding");
            weaken_validator(response);
        }
    }
}

impl Default for Compression {
    fn default() -> Self {
        Self::new()
    }
}

impl HttpMiddleware for Compression {
    fn on_response(
        &self,
        context: &HttpRequestContext<'_>,
        _operation: Option<&blazingly_core::OperationDescriptor>,
        response: &mut Response,
    ) {
        let request = context.request();
        let accepted = joined_header(request, "accept-encoding");
        if response.is_streaming() {
            self.compress_streaming(request.method(), accepted.as_deref(), response);
        } else {
            self.compress_buffered(request.method(), accepted.as_deref(), response);
        }
    }
}

/// A negotiated response content coding.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ContentEncoding {
    /// Brotli, sent as `Content-Encoding: br`.
    Brotli,
    /// Deflate in a gzip wrapper, sent as `Content-Encoding: gzip`.
    Gzip,
}

impl ContentEncoding {
    /// Token used in `Accept-Encoding` and `Content-Encoding`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Brotli => "br",
            Self::Gzip => "gzip",
        }
    }
}

/// Picks the best supported coding for an `Accept-Encoding` field value.
///
/// Returns `None` when the client accepts neither Brotli nor gzip.
#[must_use]
pub fn negotiate_encoding(accepted: Option<&str>) -> Option<ContentEncoding> {
    preferred_encoding(accepted)
}

/// A stateful encoder that flushes after every chunk.
///
/// Both variants are boxed so the enum stays small; the Brotli encoder state is
/// far larger than the gzip one.
enum ChunkEncoder {
    Brotli(Box<CompressorWriter<Vec<u8>>>),
    Gzip(Box<GzEncoder<Vec<u8>>>),
}

impl ChunkEncoder {
    fn new(encoding: ContentEncoding, gzip_level: u32, brotli_quality: u32) -> Self {
        match encoding {
            ContentEncoding::Brotli => Self::Brotli(Box::new(CompressorWriter::new(
                Vec::new(),
                4096,
                brotli_quality.min(11),
                22,
            ))),
            ContentEncoding::Gzip => Self::Gzip(Box::new(GzEncoder::new(
                Vec::new(),
                GzipLevel::new(gzip_level.min(9)),
            ))),
        }
    }

    /// Encodes one chunk and flushes it, so the bytes are decodable now rather
    /// than after the encoder's window fills.
    fn encode(&mut self, chunk: &[u8]) -> std::io::Result<Vec<u8>> {
        match self {
            Self::Brotli(encoder) => {
                encoder.write_all(chunk)?;
                encoder.flush()?;
                Ok(std::mem::take(encoder.get_mut()))
            }
            Self::Gzip(encoder) => {
                encoder.write_all(chunk)?;
                encoder.flush()?;
                Ok(std::mem::take(encoder.get_mut()))
            }
        }
    }

    /// Writes the encoder trailer once the source stream has ended.
    fn finish(self) -> std::io::Result<Vec<u8>> {
        match self {
            // `into_inner` finishes the Brotli stream. It cannot surface a write
            // failure, and the sink is an in-memory buffer that cannot fail.
            Self::Brotli(encoder) => Ok(encoder.into_inner()),
            Self::Gzip(encoder) => (*encoder).finish(),
        }
    }
}

/// Pull stream that compresses each source chunk as it arrives.
struct CompressedStream {
    source: StreamingBody,
    /// Taken once the stream is finished or has failed, which is also the
    /// signal that no trailer may be written.
    encoder: Option<ChunkEncoder>,
}

impl BodyStream for CompressedStream {
    fn poll_next(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<Option<Result<Vec<u8>, BodyStreamError>>> {
        let this = self.get_mut();
        loop {
            if this.encoder.is_none() {
                return Poll::Ready(None);
            }
            // `StreamingBody::next_chunk` forwards exactly one `poll_next` and
            // keeps no state between calls, so polling a fresh future once is
            // the same as pulling the source once.
            let mut next = pin!(this.source.next_chunk());
            match next.as_mut().poll(context) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Some(Ok(bytes))) => {
                    let coder = this.encoder.as_mut().expect("encoder is present");
                    match coder.encode(&bytes) {
                        // A zero-length chunk terminates a chunked transfer
                        // body, so pull again instead of yielding one.
                        Ok(encoded) if encoded.is_empty() => {}
                        Ok(encoded) => return Poll::Ready(Some(Ok(encoded))),
                        Err(error) => {
                            this.encoder = None;
                            return Poll::Ready(Some(Err(BodyStreamError::new(
                                "compression_failed",
                                error.to_string(),
                            ))));
                        }
                    }
                }
                // The producer failed. Dropping the encoder without its trailer
                // keeps the truncated body from decoding as a complete one.
                Poll::Ready(Some(Err(error))) => {
                    this.encoder = None;
                    return Poll::Ready(Some(Err(error)));
                }
                Poll::Ready(None) => {
                    let encoder = this.encoder.take().expect("encoder is present");
                    return Poll::Ready(Some(encoder.finish().map_err(|error| {
                        BodyStreamError::new("compression_failed", error.to_string())
                    })));
                }
            }
        }
    }
}

/// Directory-backed static asset serving mounted at a URL prefix.
#[derive(Clone, Debug)]
pub struct StaticFiles {
    prefix: String,
    root: PathBuf,
    index: Option<String>,
    spa_fallback: bool,
    cache_control: Option<String>,
}

/// A static mount that could not be resolved.
#[derive(Debug)]
pub enum StaticFilesError {
    /// The mount prefix is not an absolute path.
    Prefix(String),
    /// The root directory could not be resolved.
    Root(PathBuf, std::io::Error),
    /// The resolved root is not a directory.
    NotADirectory(PathBuf),
}

impl fmt::Display for StaticFilesError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Prefix(prefix) => {
                write!(formatter, "static mount prefix {prefix} is not absolute")
            }
            Self::Root(path, error) => {
                write!(
                    formatter,
                    "static root {} is unusable: {error}",
                    path.display()
                )
            }
            Self::NotADirectory(path) => {
                write!(
                    formatter,
                    "static root {} is not a directory",
                    path.display()
                )
            }
        }
    }
}

impl std::error::Error for StaticFilesError {}

impl StaticFiles {
    /// Mounts `root` at the URL `prefix`, resolving the directory once.
    ///
    /// # Errors
    ///
    /// Returns an error when `prefix` is not absolute, or when `root` cannot be
    /// resolved to a readable directory.
    pub fn new(
        prefix: impl Into<String>,
        root: impl AsRef<Path>,
    ) -> Result<Self, StaticFilesError> {
        let prefix = prefix.into();
        if !prefix.starts_with('/') {
            return Err(StaticFilesError::Prefix(prefix));
        }
        let root = root.as_ref();
        let resolved = fs::canonicalize(root)
            .map_err(|error| StaticFilesError::Root(root.to_path_buf(), error))?;
        if !resolved.is_dir() {
            return Err(StaticFilesError::NotADirectory(resolved));
        }
        Ok(Self {
            prefix: prefix.trim_end_matches('/').to_owned(),
            root: resolved,
            index: None,
            spa_fallback: false,
            cache_control: None,
        })
    }

    /// Serves `name` for directory requests.
    #[must_use]
    pub fn index_file(mut self, name: impl Into<String>) -> Self {
        self.index = Some(name.into());
        self
    }

    /// Serves the index file for paths with no matching asset so a
    /// single-page application can route on the client.
    #[must_use]
    pub const fn spa_fallback(mut self, enabled: bool) -> Self {
        self.spa_fallback = enabled;
        self
    }

    /// Sets the `Cache-Control` header emitted with served assets.
    #[must_use]
    pub fn cache_control(mut self, value: impl Into<String>) -> Self {
        self.cache_control = Some(value.into());
        self
    }

    fn respond(&self, request: &dyn HttpRequestView) -> Option<Response> {
        let target = request.target();
        let path = target.split_once('?').map_or(target, |(path, _)| path);
        let relative = self.relative_path(path)?;
        let head = match request.method() {
            HttpMethod::Get => false,
            HttpMethod::Head => true,
            HttpMethod::Options => return None,
            _ => {
                return Some(
                    json_error(
                        405,
                        "method_not_allowed",
                        "static assets accept GET and HEAD",
                    )
                    .with_header("allow", "GET, HEAD"),
                );
            }
        };
        let Some(segments) = safe_segments(relative) else {
            return Some(json_error(
                400,
                "invalid_static_path",
                "static asset path is not allowed",
            ));
        };
        Some(self.locate(&segments).map_or_else(
            || json_error(404, "not_found", "static asset not found"),
            |file| self.serve(&file, request, head),
        ))
    }

    fn relative_path<'path>(&self, path: &'path str) -> Option<&'path str> {
        let rest = path.strip_prefix(self.prefix.as_str())?;
        if rest.is_empty() {
            return Some("");
        }
        rest.strip_prefix('/')
    }

    fn locate(&self, segments: &[String]) -> Option<PathBuf> {
        let mut candidate = self.root.clone();
        for segment in segments {
            candidate.push(segment);
        }
        if let Some(file) = self.resolve_file(&candidate) {
            return Some(file);
        }
        if !self.spa_fallback {
            return None;
        }
        let index = self.index.as_ref()?;
        self.resolve_file(&self.root.join(index))
    }

    /// Resolves symlinks and confirms the target is still inside the mount.
    fn resolve_file(&self, candidate: &Path) -> Option<PathBuf> {
        let resolved = fs::canonicalize(candidate).ok()?;
        if !resolved.starts_with(&self.root) {
            return None;
        }
        let metadata = fs::metadata(&resolved).ok()?;
        if metadata.is_dir() {
            let index = self.index.as_ref()?;
            let nested = fs::canonicalize(resolved.join(index)).ok()?;
            return (nested.starts_with(&self.root) && nested.is_file()).then_some(nested);
        }
        metadata.is_file().then_some(resolved)
    }

    fn serve(&self, path: &Path, request: &dyn HttpRequestView, head: bool) -> Response {
        let Ok(metadata) = fs::metadata(path) else {
            return json_error(404, "not_found", "static asset not found");
        };
        let modified = metadata.modified().ok();
        let etag = entity_tag(metadata.len(), modified);
        if not_modified(request, &etag, modified) {
            return self.decorate(Response::empty(304), &etag, modified);
        }
        let total = metadata.len();
        // A range applies to GET only, and only where the alternative would be a
        // 200; the 304 and error paths have already returned.
        let outcome = if head {
            RangeOutcome::Full
        } else {
            requested_range(request, total, &etag, modified)
        };
        let (mut response, length) = match outcome {
            RangeOutcome::Unsatisfiable => {
                let response = json_error(
                    416,
                    "range_not_satisfiable",
                    "requested range is outside the asset",
                )
                .with_header("accept-ranges", "bytes")
                .with_header("content-range", format!("bytes */{total}"));
                return self.decorate(response, &etag, modified);
            }
            RangeOutcome::Partial { start, end } => {
                let length = end - start + 1;
                let wanted = usize::try_from(length).unwrap_or(usize::MAX);
                // A short read means the file changed after its metadata was
                // read, so the promised Content-Range no longer holds.
                let body = read_range(path, start, length)
                    .ok()
                    .filter(|body| body.len() == wanted);
                let Some(body) = body else {
                    return json_error(500, "static_read_failed", "static asset could not be read");
                };
                let mut response = Response::from_bytes(206, body);
                response.set_header("content-range", format!("bytes {start}-{end}/{total}"));
                (response, length)
            }
            RangeOutcome::Full if head => (Response::empty(200), total),
            RangeOutcome::Full => {
                let Ok(body) = fs::read(path) else {
                    return json_error(500, "static_read_failed", "static asset could not be read");
                };
                let length = body.len() as u64;
                (Response::from_bytes(200, body), length)
            }
        };
        response.set_header("content-type", mime_type(path));
        response.set_header("content-length", length.to_string());
        response.set_header("accept-ranges", "bytes");
        self.decorate(response, &etag, modified)
    }

    fn decorate(
        &self,
        mut response: Response,
        etag: &str,
        modified: Option<SystemTime>,
    ) -> Response {
        response.set_header("etag", etag);
        if let Some(modified) = modified {
            response.set_header("last-modified", httpdate::fmt_http_date(modified));
        }
        if let Some(cache_control) = self.cache_control.as_deref() {
            response.set_header("cache-control", cache_control);
        }
        response
    }
}

impl HttpMiddleware for StaticFiles {
    fn on_request(&self, context: &mut HttpRequestContext<'_>) -> Option<Response> {
        self.respond(context.request())
    }
}

const GLOBAL_RATE_LIMIT_KEY: &str = "global";
const DEFAULT_MAX_CLIENTS: usize = 65_536;
const RATE_LIMIT_SWEEP_BUDGET: usize = 8;

const MIME_TYPES: &[(&str, &str)] = &[
    ("css", "text/css; charset=utf-8"),
    ("csv", "text/csv; charset=utf-8"),
    ("gif", "image/gif"),
    ("htm", "text/html; charset=utf-8"),
    ("html", "text/html; charset=utf-8"),
    ("ico", "image/vnd.microsoft.icon"),
    ("jpeg", "image/jpeg"),
    ("jpg", "image/jpeg"),
    ("js", "text/javascript; charset=utf-8"),
    ("json", "application/json"),
    ("map", "application/json"),
    ("mjs", "text/javascript; charset=utf-8"),
    ("pdf", "application/pdf"),
    ("png", "image/png"),
    ("svg", "image/svg+xml"),
    ("txt", "text/plain; charset=utf-8"),
    ("wasm", "application/wasm"),
    ("webp", "image/webp"),
    ("woff", "font/woff"),
    ("woff2", "font/woff2"),
    ("xml", "application/xml"),
    ("zip", "application/zip"),
];

fn json_error(status: u16, code: &str, message: &str) -> Response {
    Response::from_bytes(
        status,
        serde_json::to_vec(&json!({
            "error": {
                "code": code,
                "message": message,
            }
        }))
        .unwrap_or_else(|_| b"{\"error\":{\"code\":\"middleware_error\"}}".to_vec()),
    )
    .with_header("content-type", "application/json")
}

/// Combines every field with the supplied name, as RFC 9110 allows for
/// list-valued headers. Reading only the first field would silently drop a
/// split `X-Forwarded-For` chain.
fn joined_header<'request>(
    request: &'request dyn HttpRequestView,
    name: &str,
) -> Option<Cow<'request, str>> {
    let first = request.header_value(name, 0)?;
    let Some(second) = request.header_value(name, 1) else {
        return Some(Cow::Borrowed(first));
    };
    let mut joined = format!("{first}, {second}");
    let mut index = 2;
    while let Some(value) = request.header_value(name, index) {
        joined.push_str(", ");
        joined.push_str(value);
        index += 1;
    }
    Some(Cow::Owned(joined))
}

fn append_vary(response: &mut Response, value: &str) {
    let merged = match response.get_header("vary") {
        Some(existing)
            if existing
                .split(',')
                .any(|part| part.trim().eq_ignore_ascii_case(value)) =>
        {
            return;
        }
        Some(existing) => format!("{existing}, {value}"),
        None => value.to_owned(),
    };
    response.set_header("vary", merged);
}

fn normalized_host(host: &str) -> &str {
    let host = host.trim().trim_end_matches('.');
    if host.starts_with('[') {
        return host
            .split_once(']')
            .map_or(host, |(address, _)| &host[..=address.len()]);
    }
    host.rsplit_once(':')
        .filter(|(_, port)| port.bytes().all(|byte| byte.is_ascii_digit()))
        .map_or(host, |(name, _)| name)
}

fn wildcard_host_matches(host: &str, suffix: &str) -> bool {
    let Some(prefix_length) = host.len().checked_sub(suffix.len()) else {
        return false;
    };
    prefix_length > 1
        && host.as_bytes().get(prefix_length - 1) == Some(&b'.')
        && host
            .get(prefix_length..)
            .is_some_and(|tail| tail.eq_ignore_ascii_case(suffix))
}

/// Matches `*`, an exact origin, or a `scheme://*.domain` wildcard.
fn origin_matches(pattern: &str, origin: &str) -> bool {
    if pattern == "*" || pattern.eq_ignore_ascii_case(origin) {
        return true;
    }
    let Some((scheme, domain)) = pattern.split_once("*.") else {
        return false;
    };
    if domain.is_empty() || !scheme.ends_with("//") {
        return false;
    }
    let matching_scheme = origin
        .get(..scheme.len())
        .is_some_and(|head| head.eq_ignore_ascii_case(scheme));
    if !matching_scheme {
        return false;
    }
    origin
        .get(scheme.len()..)
        .is_some_and(|host| !host.contains('/') && wildcard_host_matches(host, domain))
}

fn masked_v4(address: Ipv4Addr, prefix: u8) -> u32 {
    let mask = if prefix == 0 {
        0
    } else {
        u32::MAX << (32 - prefix)
    };
    u32::from(address) & mask
}

fn masked_v6(address: Ipv6Addr, prefix: u8) -> u128 {
    let mask = if prefix == 0 {
        0
    } else {
        u128::MAX << (128 - prefix)
    };
    u128::from(address) & mask
}

fn ip_list(value: &str) -> Vec<IpAddr> {
    value
        .split(',')
        .filter_map(|part| parse_forwarded_ip(part.trim()))
        .collect()
}

fn forwarded_for_values(value: &str) -> Vec<IpAddr> {
    value
        .split(',')
        .filter_map(|entry| forwarded_parameter(entry, "for"))
        .filter_map(parse_forwarded_ip)
        .collect()
}

fn forwarded_parameter<'value>(value: &'value str, name: &str) -> Option<&'value str> {
    value
        .split(',')
        .next()?
        .split(';')
        .filter_map(|parameter| parameter.trim().split_once('='))
        .find(|(candidate, _)| candidate.trim().eq_ignore_ascii_case(name))
        .map(|(_, value)| value.trim().trim_matches('"'))
}

fn parse_forwarded_ip(value: &str) -> Option<IpAddr> {
    let value = value.trim().trim_matches('"');
    if value.eq_ignore_ascii_case("unknown") || value.starts_with('_') {
        return None;
    }
    if let Some(address) = value
        .strip_prefix('[')
        .and_then(|value| value.split_once(']'))
    {
        return address.0.parse().ok();
    }
    value
        .parse()
        .ok()
        .or_else(|| value.rsplit_once(':')?.0.parse().ok())
}

fn trusted_client_ip(
    peer: IpAddr,
    forwarded: &[IpAddr],
    trusted: impl Fn(IpAddr) -> bool,
) -> Option<IpAddr> {
    let mut current = peer;
    for candidate in forwarded.iter().rev().copied() {
        if !trusted(current) {
            break;
        }
        current = candidate;
    }
    (current != peer).then_some(current)
}

fn first_list_value(value: Option<&str>) -> Option<&str> {
    value?
        .split(',')
        .next()
        .map(str::trim)
        .filter(|v| !v.is_empty())
}

fn valid_scheme(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 16
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'-' | b'.'))
}

fn valid_forwarded_host(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 255
        && !value.bytes().any(|byte| {
            byte.is_ascii_control() || byte.is_ascii_whitespace() || matches!(byte, b'/' | b'\\')
        })
}

// Token counts are clamped to `capacity`, so the truncating cast is exact.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn consume_bucket(bucket: &mut Bucket, quota: RateLimitQuota, now: Instant) -> RateLimitDecision {
    let elapsed = now.saturating_duration_since(bucket.updated).as_secs_f64();
    bucket.tokens =
        (bucket.tokens + elapsed * quota.refill_per_second).min(f64::from(quota.capacity));
    bucket.updated = now;
    if bucket.tokens >= 1.0 {
        bucket.tokens -= 1.0;
        RateLimitDecision::allow(bucket.tokens.floor() as u32)
    } else {
        let seconds = (1.0 - bucket.tokens) / quota.refill_per_second;
        RateLimitDecision::deny(Duration::from_secs_f64(seconds.max(0.001)))
    }
}

fn compressible(content_type: Option<&str>) -> bool {
    content_type.is_none_or(|value| {
        let media_type = value.split(';').next().unwrap_or(value).trim();
        media_type.starts_with("text/")
            || media_type == "application/json"
            || media_type == "application/javascript"
            || media_type == "application/xml"
            || media_type == "image/svg+xml"
            || media_type.ends_with("+json")
            || media_type.ends_with("+xml")
    })
}

fn preferred_encoding(value: Option<&str>) -> Option<ContentEncoding> {
    let value = value?;
    let mut brotli = None;
    let mut gzip = None;
    let mut wildcard = None;
    for item in value.split(',') {
        let mut parts = item.trim().split(';');
        let name = parts.next()?.trim();
        let quality = parts
            .find_map(|part| part.trim().strip_prefix("q="))
            .and_then(|value| value.parse::<f32>().ok())
            .unwrap_or(1.0)
            .clamp(0.0, 1.0);
        match name {
            "br" => brotli = Some(quality),
            "gzip" => gzip = Some(quality),
            "*" => wildcard = Some(quality),
            _ => {}
        }
    }
    let brotli = brotli.or(wildcard).unwrap_or(0.0);
    let gzip = gzip.or(wildcard).unwrap_or(0.0);
    if brotli <= 0.0 && gzip <= 0.0 {
        None
    } else if brotli >= gzip {
        Some(ContentEncoding::Brotli)
    } else {
        Some(ContentEncoding::Gzip)
    }
}

/// Downgrades a strong validator after re-encoding, because the bytes no longer
/// match the entity tag the origin computed for the identity coding.
fn weaken_validator(response: &mut Response) {
    let weakened = response
        .get_header("etag")
        .filter(|etag| !etag.starts_with("W/"))
        .map(|etag| format!("W/{etag}"));
    if let Some(weakened) = weakened {
        response.set_header("etag", weakened);
    }
}

fn compress_gzip(body: &[u8], level: u32) -> std::io::Result<Vec<u8>> {
    let mut encoder = GzEncoder::new(Vec::new(), GzipLevel::new(level.min(9)));
    encoder.write_all(body)?;
    encoder.finish()
}

fn compress_brotli(body: &[u8], quality: u32) -> std::io::Result<Vec<u8>> {
    let mut output = Vec::new();
    {
        let mut encoder = CompressorWriter::new(&mut output, 4096, quality.min(11), 22);
        encoder.write_all(body)?;
    }
    Ok(output)
}

/// Rejects traversal, absolute, and platform-hostile path components before
/// any filesystem access.
fn safe_segments(relative: &str) -> Option<Vec<String>> {
    let decoded = percent_decode(relative)?;
    let mut segments = Vec::new();
    for segment in decoded.split('/') {
        match segment {
            "" | "." => continue,
            ".." => return None,
            _ => {}
        }
        let hostile = segment.bytes().any(|byte| {
            byte.is_ascii_control()
                || matches!(byte, b'\\' | b':' | b'*' | b'?' | b'"' | b'<' | b'>' | b'|')
        });
        if hostile {
            return None;
        }
        segments.push(segment.to_owned());
    }
    Some(segments)
}

fn percent_decode(value: &str) -> Option<String> {
    if !value.as_bytes().contains(&b'%') {
        return Some(value.to_owned());
    }
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let high = bytes.get(index + 1).copied().and_then(hex_value)?;
            let low = bytes.get(index + 2).copied().and_then(hex_value)?;
            decoded.push((high << 4) | low);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(decoded).ok()
}

const fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn mime_type(path: &Path) -> &'static str {
    let extension = path
        .extension()
        .and_then(OsStr::to_str)
        .unwrap_or_default()
        .to_ascii_lowercase();
    MIME_TYPES
        .iter()
        .find(|(candidate, _)| *candidate == extension)
        .map_or("application/octet-stream", |(_, media_type)| *media_type)
}

fn entity_tag(length: u64, modified: Option<SystemTime>) -> String {
    match modified.and_then(|time| time.duration_since(UNIX_EPOCH).ok()) {
        Some(age) => format!(
            "\"{length:x}-{:x}{:08x}\"",
            age.as_secs(),
            age.subsec_nanos()
        ),
        None => format!("\"{length:x}\""),
    }
}

fn not_modified(request: &dyn HttpRequestView, etag: &str, modified: Option<SystemTime>) -> bool {
    if let Some(if_none_match) = joined_header(request, "if-none-match") {
        return etag_matches(&if_none_match, etag);
    }
    let Some(header) = request.header_value("if-modified-since", 0) else {
        return false;
    };
    let (Ok(since), Some(modified)) = (httpdate::parse_http_date(header), modified) else {
        return false;
    };
    matches!(
        (unix_seconds(modified), unix_seconds(since)),
        (Some(modified), Some(since)) if modified <= since
    )
}

/// How a `Range` request header applies to a representation of known length.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RangeOutcome {
    /// No usable range: answer with the complete representation.
    Full,
    /// A satisfiable single range, inclusive at both ends.
    Partial { start: u64, end: u64 },
    /// A well-formed range that lies outside the representation.
    Unsatisfiable,
}

/// One parsed `byte-range-spec`, before it is resolved against a length.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RangeSpec {
    /// `first-pos "-" last-pos`.
    Bounded { first: u64, last: u64 },
    /// `first-pos "-"`.
    From(u64),
    /// `"-" suffix-length`.
    Suffix(u64),
}

/// Resolves the `Range` and `If-Range` headers of a GET against the asset.
fn requested_range(
    request: &dyn HttpRequestView,
    total: u64,
    etag: &str,
    modified: Option<SystemTime>,
) -> RangeOutcome {
    let Some(header) = request.header_value("range", 0) else {
        return RangeOutcome::Full;
    };
    // Conflicting Range fields are ambiguous, and a stale If-Range validator
    // means the client's cached prefix no longer belongs to this asset.
    if request.header_value("range", 1).is_some() || !if_range_matches(request, etag, modified) {
        return RangeOutcome::Full;
    }
    parse_range(header).map_or(RangeOutcome::Full, |spec| resolve_range(spec, total))
}

/// Parses a single-range `bytes=` header. A malformed or multi-range header
/// yields `None`, which RFC 9110 lets the origin answer with the full body.
fn parse_range(header: &str) -> Option<RangeSpec> {
    let (unit, spec) = header.trim().split_once('=')?;
    if !unit.trim().eq_ignore_ascii_case("bytes") {
        return None;
    }
    let spec = spec.trim();
    if spec.contains(',') {
        return None;
    }
    let (first, last) = spec.split_once('-')?;
    let (first, last) = (first.trim(), last.trim());
    if first.is_empty() {
        return Some(RangeSpec::Suffix(range_number(last)?));
    }
    let first = range_number(first)?;
    if last.is_empty() {
        return Some(RangeSpec::From(first));
    }
    let last = range_number(last)?;
    (last >= first).then_some(RangeSpec::Bounded { first, last })
}

const fn resolve_range(spec: RangeSpec, total: u64) -> RangeOutcome {
    match spec {
        // A zero-length suffix selects nothing, and no range of a zero-length
        // representation is satisfiable.
        RangeSpec::Suffix(0) => RangeOutcome::Unsatisfiable,
        RangeSpec::Suffix(_) if total == 0 => RangeOutcome::Unsatisfiable,
        RangeSpec::Suffix(length) => RangeOutcome::Partial {
            start: total.saturating_sub(length),
            end: total - 1,
        },
        RangeSpec::From(first) | RangeSpec::Bounded { first, .. } if first >= total => {
            RangeOutcome::Unsatisfiable
        }
        RangeSpec::From(first) => RangeOutcome::Partial {
            start: first,
            end: total - 1,
        },
        RangeSpec::Bounded { first, last } => RangeOutcome::Partial {
            start: first,
            end: if last < total { last } else { total - 1 },
        },
    }
}

fn range_number(value: &str) -> Option<u64> {
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    value.parse().ok()
}

/// RFC 9110 section 13.1.5: a range is honoured only while the client's
/// validator still matches, and only a strong validator qualifies.
fn if_range_matches(
    request: &dyn HttpRequestView,
    etag: &str,
    modified: Option<SystemTime>,
) -> bool {
    let Some(validator) = request.header_value("if-range", 0) else {
        return true;
    };
    let validator = validator.trim();
    if validator.starts_with('"') {
        return validator == etag;
    }
    if validator.starts_with("W/") {
        return false;
    }
    let (Ok(supplied), Some(modified)) = (httpdate::parse_http_date(validator), modified) else {
        return false;
    };
    matches!(
        (unix_seconds(supplied), unix_seconds(modified)),
        (Some(supplied), Some(modified)) if supplied == modified
    )
}

/// Reads only the requested slice so a range over a large asset never loads the
/// whole file.
fn read_range(path: &Path, start: u64, length: u64) -> std::io::Result<Vec<u8>> {
    let mut file = fs::File::open(path)?;
    file.seek(SeekFrom::Start(start))?;
    let mut body = Vec::with_capacity(usize::try_from(length).unwrap_or(0));
    file.take(length).read_to_end(&mut body)?;
    Ok(body)
}

fn unix_seconds(time: SystemTime) -> Option<u64> {
    time.duration_since(UNIX_EPOCH)
        .ok()
        .map(|age| age.as_secs())
}

fn etag_matches(header: &str, etag: &str) -> bool {
    header.split(',').map(str::trim).any(|candidate| {
        candidate == "*"
            || candidate == etag
            || candidate
                .strip_prefix("W/")
                .is_some_and(|value| value == etag)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use blazingly_http::Request;
    use std::collections::VecDeque;
    use std::net::{Ipv4Addr, SocketAddrV4};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::task::Waker;

    /// The exact bytes of the asset every static test mount serves.
    const ASSET: &str = "export const answer = 42;";

    /// A request view that can carry repeated header fields, which the
    /// map-backed test [`Request`] cannot express.
    struct MultiHeaderRequest {
        method: HttpMethod,
        target: String,
        headers: Vec<(String, String)>,
    }

    impl MultiHeaderRequest {
        fn new(method: HttpMethod, target: &str) -> Self {
            Self {
                method,
                target: target.to_owned(),
                headers: Vec::new(),
            }
        }

        fn header(mut self, name: &str, value: &str) -> Self {
            self.headers.push((name.to_owned(), value.to_owned()));
            self
        }
    }

    impl HttpRequestView for MultiHeaderRequest {
        fn method(&self) -> HttpMethod {
            self.method
        }

        fn target(&self) -> &str {
            &self.target
        }

        fn header_value(&self, name: &str, index: usize) -> Option<&str> {
            self.headers
                .iter()
                .filter(|(header, _)| header.eq_ignore_ascii_case(name))
                .nth(index)
                .map(|(_, value)| value.as_str())
        }

        fn body(&self) -> &[u8] {
            &[]
        }
    }

    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new(label: &str) -> Self {
            static COUNTER: AtomicUsize = AtomicUsize::new(0);
            let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "blazingly-middleware-{label}-{}-{unique}",
                std::process::id()
            ));
            fs::create_dir_all(&path).expect("temporary directory");
            Self { path }
        }

        fn write(&self, name: &str, contents: &str) {
            fs::write(self.path.join(name), contents).expect("temporary file");
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            drop(fs::remove_dir_all(&self.path));
        }
    }

    /// A source stream that stays pending until the test hands it more data,
    /// which is how a live event stream behaves between events.
    #[derive(Default)]
    struct PacedState {
        ready: VecDeque<Result<Vec<u8>, BodyStreamError>>,
        ended: bool,
    }

    struct PacedSource {
        state: Rc<RefCell<PacedState>>,
    }

    impl BodyStream for PacedSource {
        fn poll_next(
            self: Pin<&mut Self>,
            _context: &mut Context<'_>,
        ) -> Poll<Option<Result<Vec<u8>, BodyStreamError>>> {
            let mut state = self.state.borrow_mut();
            if let Some(item) = state.ready.pop_front() {
                return Poll::Ready(Some(item));
            }
            if state.ended {
                Poll::Ready(None)
            } else {
                Poll::Pending
            }
        }
    }

    fn paced_stream(state: &Rc<RefCell<PacedState>>) -> StreamingBody {
        StreamingBody::new(PacedSource {
            state: Rc::clone(state),
        })
    }

    fn poll_chunk(stream: &mut StreamingBody) -> Poll<Option<Result<Vec<u8>, BodyStreamError>>> {
        let mut context = Context::from_waker(Waker::noop());
        let mut chunk = pin!(stream.next_chunk());
        chunk.as_mut().poll(&mut context)
    }

    fn ready_chunk(stream: &mut StreamingBody) -> Vec<u8> {
        match poll_chunk(stream) {
            Poll::Ready(Some(Ok(chunk))) => chunk,
            other => panic!("expected a ready chunk, got {other:?}"),
        }
    }

    fn gunzip(encoded: &[u8]) -> std::io::Result<Vec<u8>> {
        let mut decoded = Vec::new();
        flate2::read::GzDecoder::new(encoded).read_to_end(&mut decoded)?;
        Ok(decoded)
    }

    fn static_mount(label: &str) -> (TempDir, StaticFiles) {
        let directory = TempDir::new(label);
        directory.write("app.js", ASSET);
        directory.write("index.html", "<!doctype html><title>app</title>");
        let files = StaticFiles::new("/static", &directory.path)
            .expect("mount")
            .index_file("index.html");
        (directory, files)
    }

    #[test]
    fn cidr_contains_only_its_network() {
        let network: IpNetwork = "10.0.0.0/8".parse().expect("valid CIDR");
        assert!(network.contains(IpAddr::V4(Ipv4Addr::new(10, 2, 3, 4))));
        assert!(!network.contains(IpAddr::V4(Ipv4Addr::new(11, 2, 3, 4))));
    }

    #[test]
    fn wildcard_hosts_require_a_real_subdomain_boundary() {
        let policy = TrustedHost::new(["*.example.com"]);
        assert!(policy.allows("api.example.com"));
        assert!(policy.allows("API.EXAMPLE.COM:443"));
        assert!(!policy.allows("evil-example.com"));
        assert!(!policy.allows("example.com"));
    }

    #[test]
    fn proxy_chain_stops_at_first_untrusted_hop() {
        let forwarded = [
            IpAddr::V4(Ipv4Addr::new(198, 51, 100, 7)),
            IpAddr::V4(Ipv4Addr::new(10, 0, 0, 2)),
        ];
        let client = trusted_client_ip(
            IpAddr::V4(Ipv4Addr::new(10, 0, 0, 3)),
            &forwarded,
            |ip| matches!(ip, IpAddr::V4(address) if address.is_private()),
        );
        assert_eq!(client, Some(IpAddr::V4(Ipv4Addr::new(198, 51, 100, 7))));
    }

    #[test]
    fn request_test_peer_is_available_to_proxy_policy() {
        let request =
            Request::get("/").peer_addr(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 1234).into());
        assert_eq!(
            HttpRequestView::peer_addr(&request).map(|address| address.ip()),
            Some(IpAddr::V4(Ipv4Addr::LOCALHOST))
        );
    }

    #[test]
    fn brotli_wins_equal_quality_negotiation() {
        assert!(matches!(
            preferred_encoding(Some("gzip, br")),
            Some(ContentEncoding::Brotli)
        ));
        assert!(matches!(
            preferred_encoding(Some("br;q=0.2, gzip;q=0.8")),
            Some(ContentEncoding::Gzip)
        ));
    }

    #[test]
    fn split_forwarded_for_fields_are_all_read() {
        let request = MultiHeaderRequest::new(HttpMethod::Get, "/")
            .header("x-forwarded-for", "198.51.100.7")
            .header("x-forwarded-for", "10.0.0.2, 10.0.0.3");
        let joined = joined_header(&request, "x-forwarded-for").expect("forwarded chain");
        assert_eq!(joined, "198.51.100.7, 10.0.0.2, 10.0.0.3");
        assert_eq!(ip_list(&joined).len(), 3);
        let client = trusted_client_ip(
            IpAddr::V4(Ipv4Addr::new(10, 0, 0, 4)),
            &ip_list(&joined),
            |ip| matches!(ip, IpAddr::V4(address) if address.is_private()),
        );
        assert_eq!(client, Some(IpAddr::V4(Ipv4Addr::new(198, 51, 100, 7))));
    }

    #[test]
    fn preflight_echoes_the_requested_headers() {
        let policy = Cors::permissive();
        let request = MultiHeaderRequest::new(HttpMethod::Options, "/orders")
            .header("origin", "https://app.example")
            .header("access-control-request-method", "POST")
            .header("access-control-request-headers", "x-trace-id")
            .header("access-control-request-headers", "authorization");
        let response = policy.intercept(&request).expect("preflight response");
        assert_eq!(response.status(), 204);
        assert_eq!(
            response.get_header("access-control-allow-headers"),
            Some("x-trace-id, authorization")
        );
        assert!(
            response
                .get_header("vary")
                .is_some_and(|vary| vary.contains("access-control-request-headers"))
        );
    }

    #[test]
    fn preflight_denies_unlisted_headers() {
        let policy = Cors::new()
            .allow_origin("https://app.example")
            .allow_methods([HttpMethod::Post])
            .allow_header("authorization");
        let request = MultiHeaderRequest::new(HttpMethod::Options, "/orders")
            .header("origin", "https://app.example")
            .header("access-control-request-method", "POST")
            .header("access-control-request-headers", "x-secret");
        let response = policy.intercept(&request).expect("preflight response");
        assert_eq!(response.status(), 403);
    }

    #[test]
    fn wildcard_subdomain_origins_require_a_label_boundary() {
        assert!(origin_matches(
            "https://*.example.com",
            "https://api.example.com"
        ));
        assert!(!origin_matches(
            "https://*.example.com",
            "https://example.com"
        ));
        assert!(!origin_matches(
            "https://*.example.com",
            "http://api.example.com"
        ));
        assert!(!origin_matches(
            "https://*.example.com",
            "https://evil-example.com"
        ));
        assert!(origin_matches("*", "https://anything.test"));
    }

    #[test]
    fn conflicting_origin_headers_are_rejected() {
        let policy = Cors::permissive();
        let request = MultiHeaderRequest::new(HttpMethod::Get, "/orders")
            .header("origin", "https://app.example")
            .header("origin", "https://evil.example");
        let response = policy.intercept(&request).expect("ambiguity rejection");
        assert_eq!(response.status(), 400);
    }

    #[test]
    #[should_panic(expected = "CORS credentials cannot be combined with the wildcard origin")]
    fn credentialed_wildcard_origins_are_rejected_at_construction() {
        let _policy = Cors::new().allow_origin("*").allow_credentials(true);
    }

    #[test]
    fn memory_store_evicts_the_oldest_bucket_under_churn() {
        let store = MemoryRateLimitStore::new(4, Duration::from_secs(3600));
        let quota = RateLimitQuota::new(1, Duration::from_secs(60));
        let start = Instant::now();
        for client in 0..64 {
            let key = format!("client-{client}");
            assert!(store.consume(&key, quota, start).allowed());
            assert!(store.tracked_keys() <= 4);
        }
        assert_eq!(store.tracked_keys(), 4);
        // The oldest keys were evicted, so they start from a full bucket.
        assert!(store.consume("client-0", quota, start).allowed());
        // The most recent key kept its state and is still limited.
        assert!(!store.consume("client-63", quota, start).allowed());
    }

    #[test]
    fn memory_store_evicts_buckets_idle_past_the_ttl() {
        let store = MemoryRateLimitStore::new(1024, Duration::from_secs(60));
        let quota = RateLimitQuota::new(2, Duration::from_secs(60));
        let start = Instant::now();
        store.consume("idle", quota, start);
        assert_eq!(store.tracked_keys(), 1);
        store.consume("fresh", quota, start + Duration::from_secs(120));
        assert_eq!(store.tracked_keys(), 1);
        assert!(
            store
                .consume("idle", quota, start + Duration::from_secs(120))
                .allowed()
        );
        assert_eq!(store.tracked_keys(), 2);
    }

    #[test]
    fn memory_store_refills_over_time() {
        let store = MemoryRateLimitStore::new(8, Duration::from_secs(3600));
        let quota = RateLimitQuota::new(1, Duration::from_secs(10));
        let start = Instant::now();
        assert!(store.consume("client", quota, start).allowed());
        assert!(!store.consume("client", quota, start).allowed());
        assert!(
            store
                .consume("client", quota, start + Duration::from_secs(10))
                .allowed()
        );
    }

    #[test]
    fn rate_limit_uses_the_configured_store() {
        let store = Rc::new(MemoryRateLimitStore::new(2, Duration::from_secs(60)));
        let limit = RateLimit::keyed(5, Duration::from_secs(60), |context| {
            context
                .request()
                .header_value("x-api-key", 0)
                .map(ToOwned::to_owned)
        })
        .with_store(store);
        assert_eq!(limit.tracked_keys(), 0);
        assert!(format!("{limit:?}").contains("Custom"));
    }

    #[test]
    fn traversal_attempts_never_escape_the_static_root() {
        let (_directory, files) = static_mount("traversal");
        for target in [
            "/static/..%2f..%2fCargo.toml",
            "/static/../../Cargo.toml",
            "/static/%2e%2e/%2e%2e/Cargo.toml",
            "/static/..\\..\\Cargo.toml",
        ] {
            let response = files
                .respond(&Request::get(target))
                .expect("static mount handles its prefix");
            assert!(
                matches!(response.status(), 400 | 404),
                "{target} produced {}",
                response.status()
            );
            let body = String::from_utf8_lossy(response.body()).into_owned();
            assert!(!body.contains("[package]"), "{target} leaked file contents");
        }
    }

    #[test]
    fn matching_if_none_match_returns_not_modified() {
        let (_directory, files) = static_mount("conditional");
        let response = files
            .respond(&Request::get("/static/app.js"))
            .expect("static asset");
        assert_eq!(response.status(), 200);
        assert_eq!(
            response.get_header("content-type"),
            Some("text/javascript; charset=utf-8")
        );
        assert_eq!(response.get_header("content-length"), Some("25"));
        let etag = response.get_header("etag").expect("etag").to_owned();

        let cached = files
            .respond(&Request::get("/static/app.js").header("if-none-match", &etag))
            .expect("static asset");
        assert_eq!(cached.status(), 304);
        assert!(cached.body().is_empty());
        assert_eq!(cached.get_header("etag"), Some(etag.as_str()));
    }

    #[test]
    fn matching_if_modified_since_returns_not_modified() {
        let (_directory, files) = static_mount("modified");
        let response = files
            .respond(&Request::get("/static/app.js"))
            .expect("static asset");
        let last_modified = response
            .get_header("last-modified")
            .expect("last-modified")
            .to_owned();
        let cached = files
            .respond(&Request::get("/static/app.js").header("if-modified-since", &last_modified))
            .expect("static asset");
        assert_eq!(cached.status(), 304);
    }

    #[test]
    fn head_reports_the_length_without_a_body() {
        let (_directory, files) = static_mount("head");
        let response = files
            .respond(&Request::head("/static/app.js"))
            .expect("static asset");
        assert_eq!(response.status(), 200);
        assert_eq!(response.get_header("content-length"), Some("25"));
        assert!(response.body().is_empty());
    }

    #[test]
    fn directory_requests_serve_the_index_file() {
        let (_directory, files) = static_mount("index");
        let response = files
            .respond(&Request::get("/static"))
            .expect("static asset");
        assert_eq!(response.status(), 200);
        assert_eq!(
            response.get_header("content-type"),
            Some("text/html; charset=utf-8")
        );
    }

    #[test]
    fn spa_fallback_serves_the_index_for_unknown_paths() {
        let (_directory, files) = static_mount("spa");
        let response = files
            .respond(&Request::get("/static/orders/42"))
            .expect("static asset");
        assert_eq!(response.status(), 404);

        let (_spa_directory, spa) = static_mount("spa-enabled");
        let spa = spa.spa_fallback(true).cache_control("no-cache");
        let response = spa
            .respond(&Request::get("/static/orders/42"))
            .expect("static asset");
        assert_eq!(response.status(), 200);
        assert_eq!(response.get_header("cache-control"), Some("no-cache"));
    }

    #[test]
    fn unrelated_paths_and_methods_are_left_to_the_router() {
        let (_directory, files) = static_mount("passthrough");
        assert!(files.respond(&Request::get("/orders")).is_none());
        assert!(files.respond(&Request::get("/staticfoo")).is_none());
        assert!(files.respond(&Request::options("/static/app.js")).is_none());
        let rejected = files
            .respond(&Request::post("/static/app.js"))
            .expect("static mount rejects writes");
        assert_eq!(rejected.status(), 405);
        assert_eq!(rejected.get_header("allow"), Some("GET, HEAD"));
    }

    #[test]
    fn extensions_map_to_known_media_types() {
        assert_eq!(mime_type(Path::new("a/b.WOFF2")), "font/woff2");
        assert_eq!(mime_type(Path::new("a/b.svg")), "image/svg+xml");
        assert_eq!(mime_type(Path::new("a/b.bin")), "application/octet-stream");
        assert_eq!(mime_type(Path::new("a/b")), "application/octet-stream");
    }

    #[test]
    fn a_satisfiable_range_returns_partial_content() {
        let (_directory, files) = static_mount("range");
        let response = files
            .respond(&Request::get("/static/app.js").header("range", "bytes=7-11"))
            .expect("static asset");
        assert_eq!(response.status(), 206);
        assert_eq!(response.body(), b"const");
        assert_eq!(response.get_header("content-range"), Some("bytes 7-11/25"));
        assert_eq!(response.get_header("content-length"), Some("5"));
        assert_eq!(response.get_header("accept-ranges"), Some("bytes"));
        assert_eq!(
            response.get_header("content-type"),
            Some("text/javascript; charset=utf-8")
        );
    }

    #[test]
    fn open_ended_and_suffix_ranges_clamp_to_the_asset() {
        let (_directory, files) = static_mount("range-forms");
        let open = files
            .respond(&Request::get("/static/app.js").header("range", "bytes=20-"))
            .expect("static asset");
        assert_eq!(open.status(), 206);
        assert_eq!(open.body(), b"= 42;");
        assert_eq!(open.get_header("content-range"), Some("bytes 20-24/25"));

        let suffix = files
            .respond(&Request::get("/static/app.js").header("range", "bytes=-3"))
            .expect("static asset");
        assert_eq!(suffix.status(), 206);
        assert_eq!(suffix.body(), b"42;");
        assert_eq!(suffix.get_header("content-range"), Some("bytes 22-24/25"));

        // A suffix longer than the asset, and a last-pos past its end, both
        // clamp instead of failing.
        let clamped = files
            .respond(&Request::get("/static/app.js").header("range", "bytes=-900"))
            .expect("static asset");
        assert_eq!(clamped.status(), 206);
        assert_eq!(clamped.body(), ASSET.as_bytes());
        assert_eq!(clamped.get_header("content-range"), Some("bytes 0-24/25"));

        let past_end = files
            .respond(&Request::get("/static/app.js").header("range", "bytes=22-900"))
            .expect("static asset");
        assert_eq!(past_end.status(), 206);
        assert_eq!(past_end.get_header("content-range"), Some("bytes 22-24/25"));
    }

    #[test]
    fn an_unsatisfiable_range_returns_416() {
        let (_directory, files) = static_mount("range-unsatisfiable");
        for header in ["bytes=25-40", "bytes=100-", "bytes=-0"] {
            let response = files
                .respond(&Request::get("/static/app.js").header("range", header))
                .expect("static asset");
            assert_eq!(response.status(), 416, "{header}");
            assert_eq!(response.get_header("content-range"), Some("bytes */25"));
            assert_eq!(response.get_header("accept-ranges"), Some("bytes"));
        }
    }

    #[test]
    fn a_stale_if_range_validator_returns_the_full_representation() {
        let (_directory, files) = static_mount("if-range-stale");
        let response = files
            .respond(
                &Request::get("/static/app.js")
                    .header("range", "bytes=0-4")
                    .header("if-range", "\"0-deadbeef\""),
            )
            .expect("static asset");
        assert_eq!(response.status(), 200);
        assert_eq!(response.body(), ASSET.as_bytes());
        assert_eq!(response.get_header("content-length"), Some("25"));
        assert_eq!(response.get_header("accept-ranges"), Some("bytes"));
        assert!(response.get_header("content-range").is_none());
    }

    #[test]
    fn a_matching_if_range_validator_keeps_the_range() {
        let (_directory, files) = static_mount("if-range-fresh");
        let full = files
            .respond(&Request::get("/static/app.js"))
            .expect("static asset");
        let etag = full.get_header("etag").expect("etag").to_owned();
        let last_modified = full
            .get_header("last-modified")
            .expect("last-modified")
            .to_owned();

        let tagged = files
            .respond(
                &Request::get("/static/app.js")
                    .header("range", "bytes=0-5")
                    .header("if-range", &etag),
            )
            .expect("static asset");
        assert_eq!(tagged.status(), 206);
        assert_eq!(tagged.body(), b"export");

        let dated = files
            .respond(
                &Request::get("/static/app.js")
                    .header("range", "bytes=0-5")
                    .header("if-range", &last_modified),
            )
            .expect("static asset");
        assert_eq!(dated.status(), 206);

        // A weak validator never satisfies If-Range.
        let weak = files
            .respond(
                &Request::get("/static/app.js")
                    .header("range", "bytes=0-5")
                    .header("if-range", format!("W/{etag}")),
            )
            .expect("static asset");
        assert_eq!(weak.status(), 200);
    }

    #[test]
    fn unusable_range_headers_fall_back_to_the_full_representation() {
        let (_directory, files) = static_mount("range-fallback");
        for header in [
            // Multi-range is answered with the whole representation instead of
            // a multipart/byteranges payload.
            "bytes=0-10,20-30",
            "bytes=abc-def",
            "bytes=5-2",
            "items=0-10",
            "bytes=-",
        ] {
            let response = files
                .respond(&Request::get("/static/app.js").header("range", header))
                .expect("static asset");
            assert_eq!(response.status(), 200, "{header}");
            assert_eq!(response.body(), ASSET.as_bytes(), "{header}");
        }
    }

    #[test]
    fn range_is_ignored_outside_get() {
        let (_directory, files) = static_mount("range-head");
        let response = files
            .respond(&Request::head("/static/app.js").header("range", "bytes=0-4"))
            .expect("static asset");
        assert_eq!(response.status(), 200);
        assert_eq!(response.get_header("content-length"), Some("25"));
        assert!(response.body().is_empty());
        assert!(response.get_header("content-range").is_none());
    }

    #[test]
    fn a_conditional_hit_still_wins_over_a_range() {
        let (_directory, files) = static_mount("range-conditional");
        let full = files
            .respond(&Request::get("/static/app.js"))
            .expect("static asset");
        let etag = full.get_header("etag").expect("etag").to_owned();
        let response = files
            .respond(
                &Request::get("/static/app.js")
                    .header("if-none-match", &etag)
                    .header("range", "bytes=0-4"),
            )
            .expect("static asset");
        assert_eq!(response.status(), 304);
    }

    #[test]
    fn compression_downgrades_a_strong_validator() {
        let mut response = Response::from_bytes(200, "a".repeat(4096));
        response.set_header("content-type", "text/plain; charset=utf-8");
        response.set_header("etag", "\"1000-abc\"");
        Compression::new().compress_buffered(HttpMethod::Get, Some("gzip"), &mut response);
        assert_eq!(response.get_header("content-encoding"), Some("gzip"));
        assert_eq!(response.get_header("etag"), Some("W/\"1000-abc\""));
    }

    #[test]
    fn compression_leaves_partial_content_alone() {
        let mut response = Response::from_bytes(206, "a".repeat(4096));
        response.set_header("content-type", "text/plain; charset=utf-8");
        response.set_header("content-range", "bytes 0-4095/10000");
        Compression::new().compress_buffered(HttpMethod::Get, Some("gzip"), &mut response);
        assert!(response.get_header("content-encoding").is_none());
        assert_eq!(response.body().len(), 4096);
    }

    #[test]
    fn streaming_compression_emits_a_chunk_before_the_source_ends() {
        let state = Rc::new(RefCell::new(PacedState::default()));
        state
            .borrow_mut()
            .ready
            .push_back(Ok(b"event: tick\ndata: 1\n\n".to_vec()));
        let mut stream =
            Compression::new().compress_stream(paced_stream(&state), ContentEncoding::Gzip);

        let first = ready_chunk(&mut stream);
        assert!(!first.is_empty());
        // The source has not produced another event and has not ended, so the
        // first event was delivered while the stream is still live.
        assert!(poll_chunk(&mut stream).is_pending());
        assert!(!state.borrow().ended);

        state.borrow_mut().ended = true;
        let trailer = ready_chunk(&mut stream);
        assert!(matches!(poll_chunk(&mut stream), Poll::Ready(None)));

        let mut encoded = first;
        encoded.extend_from_slice(&trailer);
        assert_eq!(
            gunzip(&encoded).expect("gzip member"),
            b"event: tick\ndata: 1\n\n"
        );
    }

    #[test]
    fn a_producer_error_terminates_the_compressed_stream() {
        let state = Rc::new(RefCell::new(PacedState::default()));
        {
            let mut state = state.borrow_mut();
            state.ready.push_back(Ok(b"data: one\n\n".to_vec()));
            state.ready.push_back(Err(BodyStreamError::new(
                "upstream_failed",
                "producer gone",
            )));
        }
        let mut stream =
            Compression::new().compress_stream(paced_stream(&state), ContentEncoding::Gzip);

        let first = ready_chunk(&mut stream);
        match poll_chunk(&mut stream) {
            Poll::Ready(Some(Err(error))) => assert_eq!(error.code, "upstream_failed"),
            other => panic!("expected the producer error, got {other:?}"),
        }
        assert!(matches!(poll_chunk(&mut stream), Poll::Ready(None)));
        // No trailer was written, so the truncated body is not a complete
        // member and cannot be mistaken for one.
        assert!(gunzip(&first).is_err());
    }

    #[test]
    fn brotli_streaming_round_trips_every_chunk() {
        let state = Rc::new(RefCell::new(PacedState::default()));
        {
            let mut state = state.borrow_mut();
            state.ready.push_back(Ok(b"first ".to_vec()));
            state.ready.push_back(Ok(b"second".to_vec()));
            state.ended = true;
        }
        let mut stream =
            Compression::new().compress_stream(paced_stream(&state), ContentEncoding::Brotli);

        let mut encoded = Vec::new();
        let mut chunks = 0;
        while let Poll::Ready(Some(chunk)) = poll_chunk(&mut stream) {
            encoded.extend_from_slice(&chunk.expect("chunk"));
            chunks += 1;
        }
        assert!(chunks >= 2, "each source chunk should be flushed out");
        let mut decoded = Vec::new();
        brotli::Decompressor::new(encoded.as_slice(), 4096)
            .read_to_end(&mut decoded)
            .expect("brotli stream");
        assert_eq!(decoded, b"first second");
    }

    #[test]
    fn encoding_negotiation_is_public() {
        assert_eq!(
            negotiate_encoding(Some("gzip, br")),
            Some(ContentEncoding::Brotli)
        );
        assert_eq!(negotiate_encoding(Some("identity")), None);
        assert_eq!(ContentEncoding::Gzip.as_str(), "gzip");
    }
}
