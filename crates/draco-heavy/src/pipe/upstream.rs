use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Arc, RwLock};

use url::Url;

pub type SlotId = usize;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Upstream {
    pub host: String,
    pub port: u16,
    pub username: Option<String>,
    pub password: Option<String>,
    pub expected_exit_ip: IpAddr,
}

impl Upstream {
    pub fn parse(proxy: &str, expected_exit_ip: IpAddr) -> Result<Self, String> {
        let url = Url::parse(proxy).map_err(|error| format!("invalid upstream proxy: {error}"))?;
        if !matches!(url.scheme(), "socks5" | "socks5h") {
            return Err("upstream proxy must use socks5:// or socks5h://".into());
        }
        let host = url
            .host_str()
            .filter(|host| !host.is_empty())
            .ok_or_else(|| "upstream proxy is missing a host".to_string())?;
        let port = url
            .port_or_known_default()
            .ok_or_else(|| "upstream proxy is missing a port".to_string())?;
        let username = (!url.username().is_empty()).then(|| url.username().to_owned());
        let password = url.password().map(str::to_owned);
        Ok(Self {
            host: host.to_owned(),
            port,
            username,
            password,
            expected_exit_ip,
        })
    }
}

#[derive(Debug, Default)]
struct UpstreamState {
    values: HashMap<SlotId, Upstream>,
    versions: HashMap<SlotId, u64>,
}

#[derive(Debug, Clone, Default)]
pub struct UpstreamMap {
    inner: Arc<RwLock<UpstreamState>>,
}

impl UpstreamMap {
    pub fn get(&self, slot: SlotId) -> Option<Upstream> {
        self.inner
            .read()
            .expect("upstream map poisoned")
            .values
            .get(&slot)
            .cloned()
    }

    pub fn swap(&self, slot: SlotId, upstream: Upstream) -> Option<Upstream> {
        let mut state = self.inner.write().expect("upstream map poisoned");
        *state.versions.entry(slot).or_default() += 1;
        state.values.insert(slot, upstream)
    }

    pub fn clear(&self, slot: SlotId) -> Option<Upstream> {
        let mut state = self.inner.write().expect("upstream map poisoned");
        *state.versions.entry(slot).or_default() += 1;
        state.values.remove(&slot)
    }

    pub fn version(&self, slot: SlotId) -> u64 {
        self.inner
            .read()
            .expect("upstream map poisoned")
            .versions
            .get(&slot)
            .copied()
            .unwrap_or_default()
    }

    pub fn configured_slots(&self) -> usize {
        self.inner
            .read()
            .expect("upstream map poisoned")
            .values
            .len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn upstream(host: &str, exit: &str) -> Upstream {
        Upstream {
            host: host.into(),
            port: 1080,
            username: None,
            password: None,
            expected_exit_ip: exit.parse().unwrap(),
        }
    }

    #[test]
    fn parses_sticky_socks5h_proxy() {
        let parsed = Upstream::parse(
            "socks5h://sticky-user:secret@residential.example:1080",
            "203.0.113.10".parse().unwrap(),
        )
        .unwrap();
        assert_eq!(parsed.host, "residential.example");
        assert_eq!(parsed.port, 1080);
        assert_eq!(parsed.username.as_deref(), Some("sticky-user"));
        assert_eq!(parsed.password.as_deref(), Some("secret"));
    }

    #[test]
    fn slots_hold_independent_hot_swappable_upstreams() {
        let map = UpstreamMap::default();
        map.swap(0, upstream("a", "203.0.113.1"));
        map.swap(1, upstream("b", "203.0.113.2"));
        assert_eq!(map.get(0).unwrap().host, "a");
        assert_eq!(map.get(1).unwrap().host, "b");
        let slot_zero_version = map.version(0);

        let old = map.swap(0, upstream("c", "203.0.113.3")).unwrap();
        assert_eq!(old.host, "a");
        assert_eq!(map.get(0).unwrap().host, "c");
        assert_eq!(map.get(1).unwrap().host, "b");
        assert!(map.version(0) > slot_zero_version);

        map.clear(0);
        assert!(map.get(0).is_none());
        assert_eq!(map.configured_slots(), 1);
    }
}
