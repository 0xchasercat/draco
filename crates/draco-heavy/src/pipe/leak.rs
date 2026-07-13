use std::future::Future;
use std::net::IpAddr;
use std::pin::Pin;
use std::process::Stdio;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use tokio::process::Command;

use super::upstream::{SlotId, Upstream, UpstreamMap};

pub type ProbeFuture<'a> = Pin<Box<dyn Future<Output = Result<IpAddr, String>> + Send + 'a>>;

pub trait ExitProbe: Send + Sync {
    fn tcp_exit_ip(&self) -> ProbeFuture<'_>;
    fn quic_exit_ip(&self) -> ProbeFuture<'_>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeakDecision {
    pub exit_ip: IpAddr,
    pub quic_enabled: bool,
    pub quic_detail: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlotServiceState {
    Blackholed,
    Ready(LeakDecision),
    Quarantined(String),
}

#[derive(Debug, Clone, Default)]
pub struct LeakGate {
    states: Arc<RwLock<std::collections::HashMap<SlotId, SlotServiceState>>>,
}

impl LeakGate {
    pub fn state(&self, slot: SlotId) -> SlotServiceState {
        self.states
            .read()
            .expect("leak gate poisoned")
            .get(&slot)
            .cloned()
            .unwrap_or(SlotServiceState::Blackholed)
    }

    pub fn blackhole(&self, slot: SlotId) {
        self.states
            .write()
            .expect("leak gate poisoned")
            .insert(slot, SlotServiceState::Blackholed);
    }

    pub fn quarantine(&self, slot: SlotId, detail: String) {
        self.states
            .write()
            .expect("leak gate poisoned")
            .insert(slot, SlotServiceState::Quarantined(detail));
    }

    pub async fn swap_and_verify<P: ExitProbe + ?Sized>(
        &self,
        upstreams: &UpstreamMap,
        slot: SlotId,
        upstream: Upstream,
        host_ip: IpAddr,
        probe: &P,
    ) -> Result<LeakDecision, String> {
        // Fail closed during every swap. The mapping exists only while the probe
        // executes and is removed again on any quarantine outcome.
        upstreams.clear(slot);
        self.blackhole(slot);
        let expected = upstream.expected_exit_ip;
        upstreams.swap(slot, upstream);

        match verify(expected, host_ip, probe).await {
            Ok(decision) => {
                self.states
                    .write()
                    .expect("leak gate poisoned")
                    .insert(slot, SlotServiceState::Ready(decision.clone()));
                Ok(decision)
            }
            Err(error) => {
                upstreams.clear(slot);
                self.states
                    .write()
                    .expect("leak gate poisoned")
                    .insert(slot, SlotServiceState::Quarantined(error.clone()));
                Err(error)
            }
        }
    }
}

pub async fn verify<P: ExitProbe + ?Sized>(
    expected: IpAddr,
    host_ip: IpAddr,
    probe: &P,
) -> Result<LeakDecision, String> {
    let tcp = probe.tcp_exit_ip().await?;
    if tcp == host_ip {
        return Err(format!("TCP leak: observed host IP {host_ip}"));
    }
    if tcp != expected {
        return Err(format!(
            "TCP exit mismatch: expected {expected}, observed {tcp}"
        ));
    }

    match probe.quic_exit_ip().await {
        Ok(quic) if quic == host_ip => Err(format!("QUIC leak: observed host IP {host_ip}")),
        Ok(quic) if quic == expected => Ok(LeakDecision {
            exit_ip: expected,
            quic_enabled: true,
            quic_detail: None,
        }),
        Ok(quic) => Ok(LeakDecision {
            exit_ip: expected,
            quic_enabled: false,
            quic_detail: Some(format!(
                "QUIC exit mismatch ({quic}); disable QUIC for this job"
            )),
        }),
        Err(error) => Ok(LeakDecision {
            exit_ip: expected,
            quic_enabled: false,
            quic_detail: Some(format!(
                "QUIC probe failed ({error}); disable QUIC for this job"
            )),
        }),
    }
}

/// Box-only probe implementation. Both requests run inside the slot namespace;
/// its only default route is the tun device, so successful results exercise the
/// entire tun2socks → relay → residential upstream path.
#[derive(Debug, Clone)]
pub struct NetnsExitProbe {
    pub namespace: String,
    pub tcp_echo_url: String,
    pub quic_echo_url: String,
    pub timeout: Duration,
}

impl ExitProbe for NetnsExitProbe {
    fn tcp_exit_ip(&self) -> ProbeFuture<'_> {
        Box::pin(self.run(false, &self.tcp_echo_url))
    }

    fn quic_exit_ip(&self) -> ProbeFuture<'_> {
        Box::pin(self.run(true, &self.quic_echo_url))
    }
}

impl NetnsExitProbe {
    async fn run(&self, http3: bool, url: &str) -> Result<IpAddr, String> {
        let mut command = Command::new("ip");
        command
            .arg("netns")
            .arg("exec")
            .arg(&self.namespace)
            .arg("curl")
            .arg("--fail")
            .arg("--silent")
            .arg("--show-error")
            .arg("--max-time")
            .arg(self.timeout.as_secs().max(1).to_string());
        if http3 {
            command.arg("--http3-only");
        }
        let output = command
            .arg(url)
            .stdin(Stdio::null())
            .stderr(Stdio::piped())
            .stdout(Stdio::piped())
            .output()
            .await
            .map_err(|error| format!("spawn netns curl: {error}"))?;
        if !output.status.success() {
            return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
        }
        parse_echo_ip(&output.stdout)
    }
}

fn parse_echo_ip(bytes: &[u8]) -> Result<IpAddr, String> {
    let body = String::from_utf8_lossy(bytes);
    let trimmed = body.trim().trim_matches('"');
    if let Ok(ip) = trimmed.parse() {
        return Ok(ip);
    }
    for line in body.lines() {
        if let Some(candidate) = line.strip_prefix("ip=") {
            if let Ok(ip) = candidate.trim().parse() {
                return Ok(ip);
            }
        }
    }
    if let Ok(value) = serde_json::from_slice::<serde_json::Value>(bytes) {
        for key in ["ip", "origin", "address"] {
            if let Some(candidate) = value.get(key).and_then(|value| value.as_str()) {
                if let Some(first) = candidate.split(',').next() {
                    if let Ok(ip) = first.trim().parse() {
                        return Ok(ip);
                    }
                }
            }
        }
    }
    Err("IP echo response contained no parseable address".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeProbe {
        tcp: Result<IpAddr, String>,
        quic: Result<IpAddr, String>,
    }

    impl ExitProbe for FakeProbe {
        fn tcp_exit_ip(&self) -> ProbeFuture<'_> {
            Box::pin(async { self.tcp.clone() })
        }

        fn quic_exit_ip(&self) -> ProbeFuture<'_> {
            Box::pin(async { self.quic.clone() })
        }
    }

    fn upstream(exit: IpAddr) -> Upstream {
        Upstream {
            host: "proxy".into(),
            port: 1080,
            username: None,
            password: None,
            expected_exit_ip: exit,
        }
    }

    #[tokio::test]
    async fn both_paths_verified_enable_quic() {
        let expected = "203.0.113.10".parse().unwrap();
        let decision = verify(
            expected,
            "192.0.2.1".parse().unwrap(),
            &FakeProbe {
                tcp: Ok(expected),
                quic: Ok(expected),
            },
        )
        .await
        .unwrap();
        assert!(decision.quic_enabled);
    }

    #[tokio::test]
    async fn unavailable_quic_degrades_to_tcp_only() {
        let expected = "203.0.113.10".parse().unwrap();
        let decision = verify(
            expected,
            "192.0.2.1".parse().unwrap(),
            &FakeProbe {
                tcp: Ok(expected),
                quic: Err("HTTP/3 unavailable".into()),
            },
        )
        .await
        .unwrap();
        assert!(!decision.quic_enabled);
        assert!(decision.quic_detail.unwrap().contains("disable QUIC"));
    }

    #[tokio::test]
    async fn simulated_quic_host_leak_quarantines_and_blackholes() {
        let expected = "203.0.113.10".parse().unwrap();
        let host = "192.0.2.1".parse().unwrap();
        let map = UpstreamMap::default();
        let gate = LeakGate::default();
        let result = gate
            .swap_and_verify(
                &map,
                7,
                upstream(expected),
                host,
                &FakeProbe {
                    tcp: Ok(expected),
                    quic: Ok(host),
                },
            )
            .await;
        assert!(result.is_err());
        assert!(map.get(7).is_none(), "quarantine must blackhole upstream");
        assert!(matches!(
            gate.state(7),
            SlotServiceState::Quarantined(ref detail) if detail.contains("QUIC leak")
        ));
    }

    #[test]
    fn parses_plain_and_json_echo_shapes() {
        assert_eq!(
            parse_echo_ip(b"203.0.113.5\n").unwrap(),
            "203.0.113.5".parse::<IpAddr>().unwrap()
        );
        assert_eq!(
            parse_echo_ip(br#"{"origin":"203.0.113.6"}"#).unwrap(),
            "203.0.113.6".parse::<IpAddr>().unwrap()
        );
        assert_eq!(
            parse_echo_ip(b"fl=1\nip=203.0.113.7\nh3=on\n").unwrap(),
            "203.0.113.7".parse::<IpAddr>().unwrap()
        );
    }
}
