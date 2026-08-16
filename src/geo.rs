use std::{
    collections::HashMap,
    fs,
    path::PathBuf,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use directories::ProjectDirs;
use iso_country::Country;
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};

const CACHE_TTL: Duration = Duration::from_secs(30 * 24 * 60 * 60);

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct GeoInfo {
    pub country: String,
    pub region: String,
    pub org: String,
    pub is_hosting: bool,
    fetched_at: u64,
}

#[derive(Default, Deserialize, Serialize)]
struct CacheFile {
    entries: HashMap<String, GeoInfo>,
}

pub struct GeoResolver {
    enabled: bool,
    cache_path: Option<PathBuf>,
    cache: CacheFile,
    client: Client,
}

pub fn country_name(code: &str) -> &str {
    if let Some(short_name) = common_country_name(code) {
        return short_name;
    }
    code.parse::<Country>()
        .map(|country| country.name())
        .unwrap_or(code)
}

fn common_country_name(code: &str) -> Option<&'static str> {
    Some(match code {
        "BO" => "Bolivia",
        "BN" => "Brunei",
        "CD" => "DR Congo",
        "GB" => "United Kingdom",
        "IR" => "Iran",
        "KP" => "North Korea",
        "KR" => "South Korea",
        "LA" => "Laos",
        "MD" => "Moldova",
        "PS" => "Palestine",
        "RU" => "Russia",
        "SY" => "Syria",
        "TZ" => "Tanzania",
        "VE" => "Venezuela",
        "VN" => "Vietnam",
        _ => return None,
    })
}

impl GeoResolver {
    pub fn new(enabled: bool) -> Self {
        let cache_path = ProjectDirs::from("com", "synqark", "WarframePeerOverlay")
            .map(|dirs| dirs.cache_dir().join("geo-cache.json"));
        let cache = cache_path
            .as_ref()
            .and_then(|path| fs::read_to_string(path).ok())
            .and_then(|json| serde_json::from_str(&json).ok())
            .unwrap_or_default();
        Self {
            enabled,
            cache_path,
            cache,
            client: Client::builder()
                .timeout(Duration::from_secs(5))
                .user_agent("warframe-peer-overlay/0.1")
                .build()
                .expect("valid HTTP client"),
        }
    }

    pub fn resolve(&mut self, ip: &str) -> Option<GeoInfo> {
        if !self.enabled || is_private_ip(ip) {
            return None;
        }
        if let Some(cached) = self.cache.entries.get(ip)
            && now().saturating_sub(cached.fetched_at) < CACHE_TTL.as_secs()
        {
            return Some(cached.clone());
        }

        let response = self
            .client
            .get(format!("https://ipinfo.io/{ip}/json"))
            .send()
            .ok()?
            .error_for_status()
            .ok()?
            .json::<IpInfoResponse>()
            .ok()?;
        let org = response.org.unwrap_or_default();
        let info = GeoInfo {
            country: response.country.unwrap_or_default(),
            region: response.region.unwrap_or_default(),
            is_hosting: looks_like_hosting_provider(&org),
            org,
            fetched_at: now(),
        };
        self.cache.entries.insert(ip.to_owned(), info.clone());
        self.save();
        Some(info)
    }

    fn save(&self) {
        let Some(path) = &self.cache_path else {
            return;
        };
        let Some(parent) = path.parent() else {
            return;
        };
        if fs::create_dir_all(parent).is_ok()
            && let Ok(json) = serde_json::to_vec_pretty(&self.cache)
        {
            let _ = fs::write(path, json);
        }
    }
}

#[derive(Deserialize)]
struct IpInfoResponse {
    country: Option<String>,
    region: Option<String>,
    org: Option<String>,
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn is_private_ip(ip: &str) -> bool {
    ip.parse::<std::net::IpAddr>()
        .map(|address| match address {
            std::net::IpAddr::V4(address) => {
                address.is_private()
                    || address.is_loopback()
                    || address.is_link_local()
                    || address.is_documentation()
            }
            std::net::IpAddr::V6(address) => address.is_loopback() || address.is_unique_local(),
        })
        .unwrap_or(true)
}

fn looks_like_hosting_provider(org: &str) -> bool {
    const HOSTING_NAMES: &[&str] = &[
        "amazon",
        "aws",
        "google llc",
        "microsoft",
        "azure",
        "cloudflare",
        "digitalocean",
        "ovh",
        "akamai",
        "linode",
        "hetzner",
        "oracle",
        "tencent",
        "alibaba",
        "aliyun",
        "vultr",
        "m247",
        "leaseweb",
        "choopa",
        "psychz",
        "datacamp",
        "colocrossing",
        "contabo",
        "fastly",
        "stackpath",
        "gcore",
        "hostwinds",
        "nforce",
        "datacenter",
        "data center",
        "hosting",
    ];
    let normalized = org.to_ascii_lowercase();
    HOSTING_NAMES.iter().any(|name| normalized.contains(name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_private_and_documentation_addresses() {
        assert!(is_private_ip("192.168.1.1"));
        assert!(is_private_ip("203.0.113.5"));
        assert!(!is_private_ip("8.8.8.8"));
    }

    #[test]
    fn recognizes_hosting_organizations() {
        assert!(looks_like_hosting_provider("AS14618 Amazon.com, Inc."));
        assert!(!looks_like_hosting_provider(
            "AS12345 Example Residential ISP"
        ));
    }

    #[test]
    fn converts_country_codes_to_english_names() {
        assert_eq!(country_name("US"), "United States of America");
        assert_eq!(country_name("GB"), "United Kingdom");
        assert_eq!(country_name("KR"), "South Korea");
        assert_eq!(country_name("JP"), "Japan");
        assert_eq!(country_name("Unknown"), "Unknown");
    }
}
