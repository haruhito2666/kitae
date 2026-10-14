use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use url::Url;

use crate::FetchError;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SearchProvider {
    #[default]
    DuckDuckGo,
    Wiby,
    Marginalia,
    Wikipedia,
}

impl SearchProvider {
    pub const DDG: Self = Self::DuckDuckGo;

    pub fn search_url(self, query: &str) -> String {
        let query =
            percent_encoding::utf8_percent_encode(query, percent_encoding::NON_ALPHANUMERIC);
        match self {
            Self::DuckDuckGo => {
                format!("https://html.duckduckgo.com/html/?q={query}")
            }
            Self::Wiby => format!("https://wiby.me/?q={query}"),
            Self::Marginalia => {
                format!("https://search.marginalia.nu/search?query={query}")
            }
            Self::Wikipedia => {
                format!("https://en.wikipedia.org/w/index.php?search={query}")
            }
        }
    }

    pub fn url(self, query: &str) -> String {
        self.search_url(query)
    }
}

pub fn search_url(provider: SearchProvider, query: &str) -> String {
    provider.search_url(query)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SearchConfig {
    pub provider: SearchProvider,
}

pub fn search_config() -> SearchConfig {
    SearchConfig {
        provider: search_provider(),
    }
}

pub fn set_search_config(config: SearchConfig) {
    set_search_provider(config.provider);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PrivacyConfig {
    pub dnt: bool,
    pub block_third_party_cookies: bool,
    pub https_only: bool,
    pub warn_insecure_http: bool,
    pub search_provider: SearchProvider,
}

pub type PrivacyPolicy = PrivacyConfig;
pub type PrivacySettings = PrivacyConfig;
pub type SearchEngine = SearchProvider;

impl Default for PrivacyConfig {
    fn default() -> Self {
        Self {
            dnt: false,
            block_third_party_cookies: false,
            https_only: false,
            warn_insecure_http: false,
            search_provider: SearchProvider::DuckDuckGo,
        }
    }
}

fn policy() -> &'static Mutex<PrivacyConfig> {
    static POLICY: OnceLock<Mutex<PrivacyConfig>> = OnceLock::new();
    POLICY.get_or_init(|| Mutex::new(PrivacyConfig::default()))
}

fn policy_value() -> PrivacyConfig {
    *policy().lock().unwrap_or_else(|e| e.into_inner())
}

pub fn privacy_config() -> PrivacyConfig {
    policy_value()
}

pub fn set_privacy_config(config: PrivacyConfig) {
    *policy().lock().unwrap_or_else(|e| e.into_inner()) = config;
}

pub fn set_privacy_policy(config: PrivacyConfig) {
    set_privacy_config(config);
}

pub fn privacy_policy() -> PrivacyConfig {
    privacy_config()
}

pub fn reset_privacy_config() {
    set_privacy_config(PrivacyConfig::default());
}

pub fn set_dnt_enabled(enabled: bool) {
    policy().lock().unwrap_or_else(|e| e.into_inner()).dnt = enabled;
}

pub fn dnt_enabled() -> bool {
    policy_value().dnt
}

pub fn set_dnt(enabled: bool) {
    set_dnt_enabled(enabled);
}

pub fn dnt() -> bool {
    dnt_enabled()
}

pub fn set_block_third_party_cookies(enabled: bool) {
    policy()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .block_third_party_cookies = enabled;
}

pub fn block_third_party_cookies() -> bool {
    policy_value().block_third_party_cookies
}

pub fn set_third_party_cookies(enabled: bool) {
    set_block_third_party_cookies(enabled);
}

pub fn third_party_cookies_enabled() -> bool {
    block_third_party_cookies()
}

pub fn set_third_party_cookie_policy(enabled: bool) {
    set_block_third_party_cookies(enabled);
}

pub fn third_party_cookie_policy() -> bool {
    block_third_party_cookies()
}

pub fn set_https_only(enabled: bool) {
    policy()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .https_only = enabled;
}

pub fn https_only() -> bool {
    policy_value().https_only
}

pub fn https_only_enabled() -> bool {
    https_only()
}

pub fn set_https_only_enabled(enabled: bool) {
    set_https_only(enabled);
}

pub fn https_only_policy() -> bool {
    https_only()
}

pub fn set_warn_insecure_http(enabled: bool) {
    policy()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .warn_insecure_http = enabled;
}

pub fn warn_insecure_http() -> bool {
    policy_value().warn_insecure_http
}

pub fn is_insecure_navigation(raw_url: &str) -> bool {
    let Ok(url) = Url::parse(raw_url.trim()) else {
        return false;
    };
    url.scheme() == "http"
        && !hsts_covers(url.as_str())
        && url
            .host_str()
            .is_some_and(|host| !is_https_exempt_host(host))
}

pub fn set_search_provider(provider: SearchProvider) {
    policy()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .search_provider = provider;
}

pub fn search_provider() -> SearchProvider {
    policy_value().search_provider
}

pub fn set_search_engine(provider: SearchProvider) {
    set_search_provider(provider);
}

pub fn search_engine() -> SearchProvider {
    search_provider()
}

pub fn add_dnt_header(headers: &mut Vec<(String, String)>) {
    if dnt_enabled()
        && !headers
            .iter()
            .any(|(name, _)| name.eq_ignore_ascii_case("dnt"))
    {
        headers.push(("DNT".to_string(), "1".to_string()));
    }
}

pub fn dnt_header() -> Option<(&'static str, &'static str)> {
    dnt_enabled().then_some(("DNT", "1"))
}

fn normalized_host(host: &str) -> Option<String> {
    let host = host.trim().trim_end_matches('.').to_ascii_lowercase();
    if host.is_empty() {
        return None;
    }
    let bare = host
        .strip_prefix('[')
        .and_then(|value| value.strip_suffix(']'))
        .unwrap_or(&host);
    if bare.parse::<IpAddr>().is_ok() {
        return Some(bare.to_string());
    }
    Some(bare.to_string())
}

fn host_from_url(url: &Url) -> Option<String> {
    normalized_host(url.host_str().unwrap_or(""))
}

fn host_argument(value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    if let Ok(url) = Url::parse(value) {
        if let Some(host) = host_from_url(&url) {
            return Some(host);
        }
    }
    let candidate = format!("https://{value}");
    Url::parse(&candidate)
        .ok()
        .and_then(|url| host_from_url(&url))
}

fn site_key(host: &str) -> Option<String> {
    let host = normalized_host(host)?;
    if host.parse::<IpAddr>().is_ok() {
        return Some(host);
    }
    if host == "localhost" || host.ends_with(".localhost") {
        return Some("localhost".to_string());
    }
    psl::domain_str(&host).map(|domain| domain.to_ascii_lowercase())
}

pub fn registrable_domain(value: &str) -> Option<String> {
    let host = host_argument(value)?;
    if host.parse::<IpAddr>().is_ok() || host == "localhost" {
        return Some(host);
    }
    Some(
        psl::domain_str(&host)
            .map(|domain| domain.to_ascii_lowercase())
            .unwrap_or(host),
    )
}

pub fn same_site(page_url: &str, request_url: &str) -> bool {
    let (Ok(page), Ok(request)) = (Url::parse(page_url.trim()), Url::parse(request_url.trim()))
    else {
        return false;
    };
    if !page.scheme().eq_ignore_ascii_case(request.scheme()) {
        return false;
    }
    let (Some(page_host), Some(request_host)) = (host_from_url(&page), host_from_url(&request))
    else {
        return false;
    };
    if !matches!(page.scheme(), "http" | "https") {
        return page_host == request_host;
    }
    site_key(&page_host) == site_key(&request_host)
}

pub fn is_same_site(page_url: &str, request_url: &str) -> bool {
    same_site(page_url, request_url)
}

pub fn same_site_urls(page_url: &str, request_url: &str) -> bool {
    same_site(page_url, request_url)
}

pub fn is_third_party_request(page_url: &str, request_url: &str) -> bool {
    !same_site(page_url, request_url)
}

pub fn is_third_party(page_url: &str, request_url: &str) -> bool {
    is_third_party_request(page_url, request_url)
}

pub fn third_party_cookie_allowed(page_url: &str, request_url: &str) -> bool {
    same_site(page_url, request_url)
}

pub fn should_send_cookies(page_url: &str, request_url: &str, block_third_party: bool) -> bool {
    page_url.trim().is_empty() || !block_third_party || same_site(page_url, request_url)
}

pub fn should_store_cookies(page_url: &str, request_url: &str, block_third_party: bool) -> bool {
    should_send_cookies(page_url, request_url, block_third_party)
}

pub fn cookies_allowed_for_request(page_url: &str, request_url: &str) -> bool {
    should_send_cookies(page_url, request_url, block_third_party_cookies())
}

pub fn cookies_allowed_for_host(page_url: &str, secure: bool, host: &str) -> bool {
    let Some(host) = normalized_host(host) else {
        return false;
    };
    let scheme = if secure { "https" } else { "http" };
    let request = if host.contains(':') {
        format!("{scheme}://[{host}]/")
    } else {
        format!("{scheme}://{host}/")
    };
    should_send_cookies(page_url, &request, block_third_party_cookies())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HstsDirective {
    pub max_age: u64,
    pub include_subdomains: bool,
}

pub fn parse_hsts_header(value: &str) -> Option<HstsDirective> {
    let mut max_age = None;
    let mut include_subdomains = false;
    for directive in value.split(';') {
        let directive = directive.trim();
        if directive.is_empty() {
            continue;
        }
        let (name, value) = match directive.split_once('=') {
            Some((name, value)) => (name.trim(), Some(value.trim())),
            None => (directive, None),
        };
        if name.eq_ignore_ascii_case("max-age") {
            if max_age.is_some() {
                return None;
            }
            let value = value?;
            if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            max_age = Some(value.parse::<u64>().ok()?);
        } else if name.eq_ignore_ascii_case("includesubdomains") {
            if value.is_some() || include_subdomains {
                return None;
            }
            include_subdomains = true;
        }
    }
    Some(HstsDirective {
        max_age: max_age?,
        include_subdomains,
    })
}

pub fn parse_strict_transport_security(value: &str) -> Option<HstsDirective> {
    parse_hsts_header(value)
}

pub fn parse_hsts(value: &str) -> Option<HstsDirective> {
    parse_hsts_header(value)
}

pub fn parse_hsts_max_age(value: &str) -> Option<u64> {
    parse_hsts_header(value).map(|directive| directive.max_age)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HstsPolicy {
    pub host: String,
    pub max_age: Duration,
    pub include_subdomains: bool,
    pub expires_at: Instant,
}

impl HstsPolicy {
    pub fn include_sub_domains(&self) -> bool {
        self.include_subdomains
    }

    pub fn includes_subdomains(&self) -> bool {
        self.include_subdomains
    }

    pub fn max_age_secs(&self) -> u64 {
        self.max_age.as_secs()
    }

    pub fn expires_in(&self, now: Instant) -> Duration {
        self.expires_at.saturating_duration_since(now)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HstsSnapshotEntry {
    pub host: String,
    pub include_subdomains: bool,
    pub expires_at_unix: u64,
}

pub const HSTS_MAX_ENTRIES: usize = 1024;
pub const HSTS_MAX_AGE_SECS: u64 = 2_000_000_000;

struct HstsEntry {
    max_age: Duration,
    include_subdomains: bool,
    expires_at: Instant,
}

#[derive(Default)]
struct HstsStore {
    entries: HashMap<String, HstsEntry>,
}

impl HstsStore {
    fn prune(&mut self, now: Instant) {
        self.entries.retain(|_, entry| now < entry.expires_at);
    }

    fn insert(&mut self, host: String, max_age: Duration, include_subdomains: bool, now: Instant) {
        self.prune(now);
        if max_age.is_zero() {
            self.entries.remove(&host);
            return;
        }
        let effective = max_age.min(Duration::from_secs(HSTS_MAX_AGE_SECS));
        let expires_at = now
            .checked_add(effective)
            .unwrap_or_else(|| now + Duration::from_secs(HSTS_MAX_AGE_SECS));
        self.entries.insert(
            host,
            HstsEntry {
                max_age,
                include_subdomains,
                expires_at,
            },
        );
        while self.entries.len() > HSTS_MAX_ENTRIES {
            let Some(oldest) = self
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.expires_at)
                .map(|(host, _)| host.clone())
            else {
                break;
            };
            self.entries.remove(&oldest);
        }
    }
}

fn hsts_store() -> &'static Mutex<HstsStore> {
    static STORE: OnceLock<Mutex<HstsStore>> = OnceLock::new();
    STORE.get_or_init(|| Mutex::new(HstsStore::default()))
}

fn hsts_host(value: &str) -> Option<String> {
    let value = value.trim();
    if value.contains("://") {
        let url = Url::parse(value).ok()?;
        if !matches!(url.scheme(), "http" | "https" | "ws" | "wss") {
            return None;
        }
        return host_from_url(&url);
    }
    host_argument(value)
}

fn hsts_matches(entry_host: &str, host: &str, include_subdomains: bool) -> bool {
    entry_host == host || (include_subdomains && host.ends_with(&format!(".{entry_host}")))
}

pub fn hsts_policy(host: &str) -> Option<HstsPolicy> {
    hsts_policy_at(host, Instant::now())
}

pub fn hsts_policy_at(host: &str, now: Instant) -> Option<HstsPolicy> {
    let host = hsts_host(host)?;
    let mut store = hsts_store().lock().unwrap_or_else(|e| e.into_inner());
    store.prune(now);
    let entry = store.entries.get(&host)?;
    Some(HstsPolicy {
        host,
        max_age: entry.max_age,
        include_subdomains: entry.include_subdomains,
        expires_at: entry.expires_at,
    })
}

pub fn hsts_policy_for_host(host: &str) -> Option<HstsPolicy> {
    hsts_policy(host)
}

pub fn hsts_effective_policy(value: &str) -> Option<HstsPolicy> {
    hsts_effective_policy_at(value, Instant::now())
}

pub fn hsts_effective_policy_at(value: &str, now: Instant) -> Option<HstsPolicy> {
    let host = hsts_host(value)?;
    let mut store = hsts_store().lock().unwrap_or_else(|e| e.into_inner());
    store.prune(now);
    store
        .entries
        .iter()
        .filter(|(entry_host, entry)| hsts_matches(entry_host, &host, entry.include_subdomains))
        .max_by_key(|(entry_host, _)| entry_host.len())
        .map(|(entry_host, entry)| HstsPolicy {
            host: entry_host.clone(),
            max_age: entry.max_age,
            include_subdomains: entry.include_subdomains,
            expires_at: entry.expires_at,
        })
}

pub fn hsts_policies() -> Vec<HstsPolicy> {
    hsts_policies_at(Instant::now())
}

pub fn hsts_policies_at(now: Instant) -> Vec<HstsPolicy> {
    let mut store = hsts_store().lock().unwrap_or_else(|e| e.into_inner());
    store.prune(now);
    let mut out: Vec<HstsPolicy> = store
        .entries
        .iter()
        .map(|(host, entry)| HstsPolicy {
            host: host.clone(),
            max_age: entry.max_age,
            include_subdomains: entry.include_subdomains,
            expires_at: entry.expires_at,
        })
        .collect();
    out.sort_by(|a, b| a.host.cmp(&b.host));
    out
}

fn unix_now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

pub fn hsts_snapshot() -> Vec<HstsSnapshotEntry> {
    let now = Instant::now();
    let unix_now = unix_now_secs();
    hsts_policies_at(now)
        .into_iter()
        .filter_map(|policy| {
            let remaining = policy.expires_in(now).as_secs();
            (remaining > 0).then_some(HstsSnapshotEntry {
                host: policy.host,
                include_subdomains: policy.include_subdomains,
                expires_at_unix: unix_now.saturating_add(remaining),
            })
        })
        .collect()
}

pub fn replace_hsts_snapshot(entries: &[HstsSnapshotEntry]) -> usize {
    let now = Instant::now();
    let unix_now = unix_now_secs();
    let mut normalized = Vec::with_capacity(entries.len().min(HSTS_MAX_ENTRIES));
    for entry in entries {
        if normalized.len() >= HSTS_MAX_ENTRIES {
            break;
        }
        let Some(host) = hsts_host(&entry.host) else {
            continue;
        };
        if host.parse::<IpAddr>().is_ok() {
            continue;
        }
        let Some(remaining) = entry.expires_at_unix.checked_sub(unix_now) else {
            continue;
        };
        if remaining == 0 {
            continue;
        }
        normalized.push((
            host,
            Duration::from_secs(remaining.min(HSTS_MAX_AGE_SECS)),
            entry.include_subdomains,
        ));
    }
    let mut store = hsts_store().lock().unwrap_or_else(|e| e.into_inner());
    store.entries.clear();
    for (host, max_age, include_subdomains) in &normalized {
        store.insert(host.clone(), *max_age, *include_subdomains, now);
    }
    normalized.len()
}

pub fn import_hsts_snapshot(entries: &[HstsSnapshotEntry]) -> usize {
    replace_hsts_snapshot(entries)
}

pub fn hsts_len() -> usize {
    let mut store = hsts_store().lock().unwrap_or_else(|e| e.into_inner());
    store.prune(Instant::now());
    store.entries.len()
}

pub fn clear_hsts() {
    hsts_store()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .entries
        .clear();
}

pub fn clear_hsts_for_host(value: &str) -> bool {
    let Some(host) = hsts_host(value) else {
        return false;
    };
    let mut store = hsts_store().lock().unwrap_or_else(|e| e.into_inner());
    store.prune(Instant::now());
    let target = store
        .entries
        .iter()
        .filter(|(entry_host, entry)| hsts_matches(entry_host, &host, entry.include_subdomains))
        .max_by_key(|(entry_host, _)| entry_host.len())
        .map(|(entry_host, _)| entry_host.clone())
        .unwrap_or(host);
    store.entries.remove(&target).is_some()
}

pub fn hsts_clear_for_host(value: &str) -> bool {
    clear_hsts_for_host(value)
}

pub fn clear_hsts_policies() {
    clear_hsts();
}

pub fn clear_hsts_cache() {
    clear_hsts();
}

pub fn hsts_cache_len() -> usize {
    hsts_len()
}

pub fn hsts_store_len() -> usize {
    hsts_len()
}

pub fn hsts_covers(value: &str) -> bool {
    hsts_covers_at(value, Instant::now())
}

pub fn hsts_covers_at(value: &str, now: Instant) -> bool {
    let Some(host) = hsts_host(value) else {
        return false;
    };
    let mut store = hsts_store().lock().unwrap_or_else(|e| e.into_inner());
    store.prune(now);
    store
        .entries
        .iter()
        .any(|(entry_host, entry)| hsts_matches(entry_host, &host, entry.include_subdomains))
}

pub fn hsts_covers_host(host: &str) -> bool {
    hsts_covers(host)
}

pub fn hsts_covers_url(value: &str) -> bool {
    hsts_covers(value)
}

pub fn hsts_applies_to(value: &str) -> bool {
    hsts_covers(value)
}

pub fn hsts_covers_host_at(host: &str, now: Instant) -> bool {
    hsts_covers_at(host, now)
}

pub fn observe_hsts(response_url: &str, header: &str) -> bool {
    let Ok(url) = Url::parse(response_url.trim()) else {
        return false;
    };
    if url.scheme() != "https" {
        return false;
    }
    let Some(host) = host_from_url(&url) else {
        return false;
    };
    if host.parse::<IpAddr>().is_ok() {
        return false;
    }
    let Some(directive) = parse_hsts_header(header) else {
        return false;
    };
    let now = Instant::now();
    hsts_store()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(
            host,
            Duration::from_secs(directive.max_age),
            directive.include_subdomains,
            now,
        );
    true
}

pub fn process_hsts_header(response_url: &str, header: &str) -> bool {
    observe_hsts(response_url, header)
}

pub fn record_hsts(response_url: &str, header: &str) -> bool {
    observe_hsts(response_url, header)
}

pub trait HstsMaxAge {
    fn into_hsts_duration(self) -> Duration;
}

impl HstsMaxAge for Duration {
    fn into_hsts_duration(self) -> Duration {
        self
    }
}

macro_rules! impl_hsts_max_age {
    ($($ty:ty),+ $(,)?) => {
        $(
            impl HstsMaxAge for $ty {
                fn into_hsts_duration(self) -> Duration {
                    Duration::from_secs(self as u64)
                }
            }
        )+
    };
}

macro_rules! impl_signed_hsts_max_age {
    ($($ty:ty),+ $(,)?) => {
        $(
            impl HstsMaxAge for $ty {
                fn into_hsts_duration(self) -> Duration {
                    if self < 0 {
                        Duration::ZERO
                    } else {
                        Duration::from_secs(self as u64)
                    }
                }
            }
        )+
    };
}

impl_hsts_max_age!(u8, u16, u32, u64, usize);
impl_signed_hsts_max_age!(i8, i16, i32, i64, isize);

pub fn store_hsts_for_test<M: HstsMaxAge>(host: &str, max_age: M, include_subdomains: bool) {
    if let Some(host) = hsts_host(host) {
        hsts_store()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(
                host,
                max_age.into_hsts_duration(),
                include_subdomains,
                Instant::now(),
            );
    }
}

pub fn store_hsts_for_test_secs(host: &str, max_age: u64, include_subdomains: bool) {
    store_hsts_for_test(host, Duration::from_secs(max_age), include_subdomains);
}

pub fn store_hsts_for_test_seconds(host: &str, max_age: u64, include_subdomains: bool) {
    store_hsts_for_test_secs(host, max_age, include_subdomains);
}

pub fn set_hsts_for_test<M: HstsMaxAge>(host: &str, max_age: M, include_subdomains: bool) {
    store_hsts_for_test(host, max_age, include_subdomains);
}

pub fn set_hsts_for_test_secs(host: &str, max_age: u64, include_subdomains: bool) {
    store_hsts_for_test_secs(host, max_age, include_subdomains);
}

pub fn set_hsts_for_test_seconds(host: &str, max_age: u64, include_subdomains: bool) {
    set_hsts_for_test_secs(host, max_age, include_subdomains);
}

pub fn insert_hsts_for_test<M: HstsMaxAge>(host: &str, max_age: M, include_subdomains: bool) {
    store_hsts_for_test(host, max_age, include_subdomains);
}

pub fn is_https_exempt_host(host: &str) -> bool {
    let Some(host) = normalized_host(host) else {
        return false;
    };
    host == "localhost" || host.ends_with(".localhost") || host.parse::<IpAddr>().is_ok()
}

pub fn apply_https_only(raw_url: &str) -> Result<String, FetchError> {
    let parsed =
        Url::parse(raw_url).map_err(|e| FetchError::HttpsUpgrade(format!("{raw_url}: {e}")))?;
    if parsed.scheme() != "http" {
        return Ok(raw_url.to_string());
    }
    let host = parsed
        .host_str()
        .ok_or_else(|| FetchError::HttpsUpgrade(format!("{raw_url}: no host")))?;
    if is_https_exempt_host(host) {
        return Ok(raw_url.to_string());
    }
    let mut upgraded = parsed;
    upgraded
        .set_scheme("https")
        .map_err(|_| FetchError::HttpsUpgrade(format!("{raw_url}: cannot upgrade to HTTPS")))?;
    Ok(upgraded.to_string())
}

pub fn https_only_url(raw_url: &str) -> Result<String, FetchError> {
    apply_https_only(raw_url)
}

pub fn upgrade_url_if_needed(raw_url: &str) -> Result<String, FetchError> {
    apply_https_only(raw_url)
}

pub fn enforce_https_only(raw_url: &str) -> Result<String, FetchError> {
    apply_https_only(raw_url)
}

pub fn upgrade_to_https(raw_url: &str) -> Result<String, FetchError> {
    apply_https_only(raw_url)
}

pub fn hsts_upgrade_url(raw_url: &str) -> Result<String, FetchError> {
    let parsed =
        Url::parse(raw_url).map_err(|e| FetchError::HttpsUpgrade(format!("{raw_url}: {e}")))?;
    if parsed.scheme() != "http" || !hsts_covers(parsed.as_str()) {
        return Ok(raw_url.to_string());
    }
    let mut upgraded = parsed;
    upgraded
        .set_scheme("https")
        .map_err(|_| FetchError::HttpsUpgrade(format!("{raw_url}: HSTS upgrade failed")))?;
    Ok(upgraded.to_string())
}

pub fn hsts_upgraded_url(raw_url: &str) -> Result<String, FetchError> {
    hsts_upgrade_url(raw_url)
}

pub(crate) fn apply_request_url_policy(url: &mut Url, top_level: bool) -> Result<(), FetchError> {
    if url.scheme() == "http" && hsts_covers(url.as_str()) {
        url.set_scheme("https")
            .map_err(|_| FetchError::HttpsUpgrade(format!("{url}: HSTS upgrade failed")))?;
    }
    if top_level && https_only() && url.scheme() == "http" {
        let raw = url.to_string();
        let upgraded = apply_https_only(&raw)?;
        *url = Url::parse(&upgraded)
            .map_err(|e| FetchError::HttpsUpgrade(format!("{upgraded}: {e}")))?;
    }
    Ok(())
}

pub fn validate_final_url(raw_url: &str, top_level: bool) -> Result<(), FetchError> {
    let parsed =
        Url::parse(raw_url).map_err(|e| FetchError::InvalidUrl(format!("{raw_url}: {e}")))?;
    if parsed.scheme() != "http" {
        return Ok(());
    }
    if hsts_covers(parsed.as_str()) {
        return Err(FetchError::HttpsUpgrade(format!(
            "{raw_url}: HTTPS is required by HSTS"
        )));
    }
    if top_level && https_only() {
        let upgraded = apply_https_only(raw_url)?;
        if Url::parse(&upgraded)
            .map_err(|e| FetchError::HttpsUpgrade(format!("{upgraded}: {e}")))?
            .scheme()
            == "https"
        {
            return Err(FetchError::HttpsUpgrade(format!(
                "{raw_url}: HTTPS-only policy blocked a redirect"
            )));
        }
    }
    if top_level && warn_insecure_http() && !is_https_exempt_host(parsed.host_str().unwrap_or("")) {
        return Err(FetchError::InsecureRedirect(raw_url.to_string()));
    }
    Ok(())
}

pub fn check_final_url(raw_url: &str, top_level: bool) -> Result<(), FetchError> {
    validate_final_url(raw_url, top_level)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::support::{http_ok, privacy_test_lock, start_server};

    #[test]
    fn defaults_and_runtime_setters_are_independent() {
        let _lock = privacy_test_lock();
        reset_privacy_config();
        assert_eq!(privacy_config(), PrivacyConfig::default());
        assert!(!dnt_enabled());
        assert!(!block_third_party_cookies());
        assert!(!https_only());
        assert!(!warn_insecure_http());
        assert_eq!(search_provider(), SearchProvider::DuckDuckGo);
        set_dnt_enabled(true);
        set_block_third_party_cookies(true);
        set_https_only(true);
        set_warn_insecure_http(true);
        set_search_provider(SearchProvider::Wiby);
        assert!(dnt_enabled());
        assert!(block_third_party_cookies());
        assert!(https_only());
        assert_eq!(search_provider(), SearchProvider::Wiby);
        reset_privacy_config();
    }

    #[test]
    fn search_providers_encode_queries() {
        let _lock = privacy_test_lock();
        reset_privacy_config();
        set_search_provider(SearchProvider::Marginalia);
        assert_eq!(
            crate::normalize_input("a b&c"),
            "https://search.marginalia.nu/search?query=a%20b%26c"
        );
        set_search_provider(SearchProvider::DuckDuckGo);
        assert_eq!(
            SearchProvider::DuckDuckGo.search_url("a b&c"),
            "https://html.duckduckgo.com/html/?q=a%20b%26c"
        );
        assert_eq!(
            SearchProvider::Wiby.search_url("a b"),
            "https://wiby.me/?q=a%20b"
        );
        assert_eq!(
            SearchProvider::Marginalia.search_url("a b"),
            "https://search.marginalia.nu/search?query=a%20b"
        );
        assert_eq!(
            SearchProvider::Wikipedia.search_url("a b"),
            "https://en.wikipedia.org/w/index.php?search=a%20b"
        );
    }

    #[test]
    fn site_comparison_is_schemeful_and_subdomain_aware() {
        assert!(same_site(
            "https://example.com/",
            "https://example.com/other"
        ));
        assert!(same_site(
            "https://example.com/",
            "https://www.example.com/"
        ));
        assert!(same_site("https://example.com/", "https://a.example.com/"));
        assert!(same_site(
            "https://a.example.com/",
            "https://b.example.com/"
        ));
        assert!(same_site(
            "https://example.com:443/",
            "https://example.com/"
        ));
        assert!(!same_site("https://example.com/", "http://example.com/"));
        assert!(!same_site("https://example.com/", "https://other.com/"));
        assert!(!same_site(
            "https://example.com/",
            "https://badexample.com/"
        ));
        assert!(!same_site(
            "https://example.com/",
            "https://example.com.evil/"
        ));
        assert!(!same_site("https://127.0.0.1/", "https://127.0.0.2/"));
        assert!(should_send_cookies("", "https://other.com/", true));
        assert!(!should_send_cookies(
            "https://example.com/",
            "https://other.com/",
            true
        ));
        assert_eq!(
            registrable_domain("https://www.example.co.uk/path").as_deref(),
            Some("example.co.uk")
        );
        assert_eq!(
            registrable_domain("http://127.0.0.1:8080/").as_deref(),
            Some("127.0.0.1")
        );
    }

    #[test]
    fn https_policy_upgrades_hosts_but_keeps_local_and_ip_exempt() {
        assert_eq!(
            apply_https_only("http://example.com/path").unwrap(),
            "https://example.com/path"
        );
        assert_eq!(
            apply_https_only("http://localhost:8080/path").unwrap(),
            "http://localhost:8080/path"
        );
        assert_eq!(
            apply_https_only("http://127.0.0.1:8080/path").unwrap(),
            "http://127.0.0.1:8080/path"
        );
        assert_eq!(
            apply_https_only("http://[::1]:8080/path").unwrap(),
            "http://[::1]:8080/path"
        );
        assert!(matches!(
            apply_https_only("http://"),
            Err(FetchError::HttpsUpgrade(_))
        ));
    }

    #[test]
    fn final_urls_are_checked_before_return() {
        let _lock = privacy_test_lock();
        reset_privacy_config();
        set_https_only(true);
        assert!(matches!(
            check_final_url("http://example.com/redirected", true),
            Err(FetchError::HttpsUpgrade(_))
        ));
        set_https_only(false);
        store_hsts_for_test("example.com", Duration::from_secs(60), false);
        assert!(matches!(
            check_final_url("http://example.com/redirected", false),
            Err(FetchError::HttpsUpgrade(_))
        ));
        clear_hsts();
        reset_privacy_config();
    }

    #[test]
    fn insecure_navigation_warning_is_top_level_only() {
        let _lock = privacy_test_lock();
        reset_privacy_config();
        assert!(is_insecure_navigation("http://example.com/path"));
        assert!(!is_insecure_navigation("http://localhost:8080/path"));
        assert!(!is_insecure_navigation("https://example.com/path"));
        set_warn_insecure_http(true);
        assert!(matches!(
            check_final_url("http://example.com/redirected", true),
            Err(FetchError::InsecureRedirect(_))
        ));
        assert!(check_final_url("http://example.com/image.png", false).is_ok());
        reset_privacy_config();
    }

    #[test]
    fn hsts_parser_rejects_invalid_values() {
        assert_eq!(
            parse_hsts_header("max-age=31536000; includeSubDomains"),
            Some(HstsDirective {
                max_age: 31536000,
                include_subdomains: true,
            })
        );
        assert!(parse_hsts_header("max-age=\"60\"").is_none());
        assert!(parse_hsts_header("includeSubDomains").is_none());
        assert!(parse_hsts_header("max-age=abc").is_none());
        assert!(parse_hsts_header("max-age=10=20").is_none());
        assert!(parse_hsts_header("max-age=10; max-age=20").is_none());
        assert!(parse_hsts_header("includeSubDomains=yes").is_none());
    }

    #[test]
    fn hsts_store_honors_expiry_and_subdomains() {
        let _lock = privacy_test_lock();
        clear_hsts();
        let now = Instant::now();
        assert!(!observe_hsts("http://secure.example/", "max-age=60"));
        assert!(!hsts_covers("http://secure.example/"));
        assert!(observe_hsts(
            "https://secure.example/",
            "max-age=60; includeSubDomains"
        ));
        assert!(hsts_covers("http://secure.example/"));
        assert_eq!(
            hsts_upgrade_url("http://secure.example/path").unwrap(),
            "https://secure.example/path"
        );
        assert!(hsts_covers("http://www.secure.example/"));
        assert!(!hsts_covers("http://notsecure.example/"));
        assert!(!observe_hsts("http://secure.example/", "max-age=0"));
        assert!(!observe_hsts("https://secure.example/", "max-age=bad"));
        assert!(hsts_covers("http://secure.example/"));
        let policy = hsts_policy("secure.example").unwrap();
        assert_eq!(policy.max_age, Duration::from_secs(60));
        assert!(policy.include_subdomains);
        assert!(!hsts_covers_at(
            "http://secure.example/",
            now + Duration::from_secs(61)
        ));
        clear_hsts();
        assert_eq!(hsts_len(), 0);
    }

    #[test]
    fn hsts_covers_insecure_warning_and_supports_effective_policy() {
        let _lock = privacy_test_lock();
        clear_hsts();
        set_warn_insecure_http(true);
        assert!(is_insecure_navigation("http://hsts.example/"));
        store_hsts_for_test("hsts.example", Duration::from_secs(60), true);
        assert!(!is_insecure_navigation("http://hsts.example/"));
        assert!(!is_insecure_navigation("http://sub.hsts.example/"));
        let policy = hsts_effective_policy("http://sub.hsts.example/").unwrap();
        assert_eq!(policy.host, "hsts.example");
        assert!(clear_hsts_for_host("hsts.example"));
        assert!(!hsts_covers("http://sub.hsts.example/"));
        assert!(!observe_hsts("https://127.0.0.1/", "max-age=60"));
        assert!(observe_hsts("https://delete.example/", "max-age=60"));
        assert!(observe_hsts("https://delete.example/", "max-age=0"));
        assert!(!hsts_covers("https://delete.example/"));
        clear_hsts();
        reset_privacy_config();
    }

    #[test]
    fn clearing_an_inherited_hsts_policy_removes_its_parent() {
        let _lock = privacy_test_lock();
        clear_hsts();
        store_hsts_for_test("hsts.example", Duration::from_secs(60), true);
        assert!(clear_hsts_for_host("sub.hsts.example"));
        assert!(!hsts_covers("http://sub.hsts.example/"));
        clear_hsts();
    }

    #[test]
    fn hsts_store_is_capped() {
        let _lock = privacy_test_lock();
        clear_hsts();
        for i in 0..(HSTS_MAX_ENTRIES + 1) {
            store_hsts_for_test(&format!("host{i}.example"), Duration::from_secs(60), false);
        }
        assert!(hsts_len() <= HSTS_MAX_ENTRIES);
        clear_hsts();
    }

    #[test]
    fn hsts_snapshot_round_trip_preserves_active_policies() {
        let _lock = privacy_test_lock();
        clear_hsts();
        store_hsts_for_test("snapshot.example", Duration::from_secs(120), true);
        let snapshot = hsts_snapshot();
        assert_eq!(snapshot.len(), 1);
        assert_eq!(snapshot[0].host, "snapshot.example");
        clear_hsts();
        assert_eq!(replace_hsts_snapshot(&snapshot), 1);
        assert!(hsts_covers("http://sub.snapshot.example/"));
        clear_hsts();
    }

    #[test]
    fn dnt_is_added_without_overwriting_caller_value() {
        let _lock = privacy_test_lock();
        reset_privacy_config();
        let (base, log, handle) = start_server(2, |_req| http_ok("ok", ""));
        set_dnt_enabled(true);
        crate::fetch(&format!("{base}/one")).unwrap();
        crate::fetch_with(
            "GET",
            &format!("{base}/two"),
            &[("dNt".to_string(), "0".to_string())],
            None,
        )
        .unwrap();
        handle.join().unwrap();
        let requests = log.lock().unwrap();
        assert!(requests[0].to_ascii_lowercase().contains("dnt: 1"));
        assert!(requests[1].to_ascii_lowercase().contains("dnt: 0"));
        assert!(!requests[1].to_ascii_lowercase().contains("dnt: 1"));
        reset_privacy_config();
        crate::set_page_origin("");
    }

    #[test]
    fn dnt_reaches_download_requests() {
        let _lock = privacy_test_lock();
        reset_privacy_config();
        let (base, log, handle) = start_server(1, |_req| http_ok("download", ""));
        set_dnt_enabled(true);
        let path = std::env::temp_dir().join(format!("kitae_dnt_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        crate::download_to_file(&format!("{base}/file"), &path, None).unwrap();
        handle.join().unwrap();
        assert!(log.lock().unwrap()[0]
            .to_ascii_lowercase()
            .contains("dnt: 1"));
        let _ = std::fs::remove_dir_all(&path);
        reset_privacy_config();
        crate::set_page_origin("");
    }

    #[test]
    fn third_party_cookies_are_not_sent_or_stored() {
        let _lock = privacy_test_lock();
        reset_privacy_config();
        crate::clear_cookies();
        set_block_third_party_cookies(true);
        let (base, log, handle) = start_server(2, |_req| {
            http_ok("ok", "Set-Cookie: sid=secret; Path=/\r\n")
        });
        crate::set_page_origin("https://page.example/");
        let url = format!("{base}/resource");
        crate::fetch(&url).unwrap();
        crate::fetch(&url).unwrap();
        handle.join().unwrap();
        let requests = log.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert!(!requests[0].to_ascii_lowercase().contains("cookie:"));
        assert!(!requests[1].to_ascii_lowercase().contains("cookie:"));
        assert_eq!(crate::cookie_len(), 0);
        reset_privacy_config();
        crate::clear_cookies();
        crate::set_page_origin("");
    }

    #[test]
    fn explicit_cross_site_cookie_headers_are_blocked() {
        let _lock = privacy_test_lock();
        reset_privacy_config();
        set_block_third_party_cookies(true);
        let (base, log, handle) = start_server(1, |_req| http_ok("ok", ""));
        crate::set_page_origin("https://page.example/");
        crate::fetch_with(
            "GET",
            &format!("{base}/resource"),
            &[("Cookie".to_string(), "caller=1".to_string())],
            None,
        )
        .unwrap();
        handle.join().unwrap();
        assert!(!log.lock().unwrap()[0]
            .to_ascii_lowercase()
            .contains("cookie:"));
        reset_privacy_config();
        crate::set_page_origin("");
    }

    #[test]
    fn first_party_cookies_keep_sending_and_storing() {
        let _lock = privacy_test_lock();
        reset_privacy_config();
        crate::clear_cookies();
        set_block_third_party_cookies(true);
        let (base, log, handle) = start_server(2, |_req| {
            http_ok("ok", "Set-Cookie: sid=secret; Path=/\r\n")
        });
        crate::set_page_origin(&format!("{base}/page"));
        let url = format!("{base}/resource");
        crate::fetch(&url).unwrap();
        crate::fetch(&url).unwrap();
        handle.join().unwrap();
        let requests = log.lock().unwrap();
        assert!(!requests[0].to_ascii_lowercase().contains("cookie:"));
        assert!(requests[1]
            .to_ascii_lowercase()
            .contains("cookie: sid=secret"));
        assert_eq!(crate::cookie_len(), 1);
        reset_privacy_config();
        crate::clear_cookies();
        crate::set_page_origin("");
    }
}
