pub mod browser;
pub mod leak;
#[cfg(target_os = "linux")]
pub mod namespace;
#[cfg(not(target_os = "linux"))]
#[path = "namespace_stub.rs"]
pub mod namespace;
pub mod socks;
pub mod upstream;

use std::net::{IpAddr, Ipv4Addr};
use std::sync::Arc;

use leak::{ExitProbe, LeakDecision, LeakGate, NetnsExitProbe, SlotServiceState};
use namespace::{relay_bind_address, NetworkNamespace, Tun2SocksSpec};
use socks::RelayServer;
use tokio::task::JoinHandle;
use upstream::{SlotId, Upstream, UpstreamMap};

#[derive(Debug, Clone)]
pub struct PipeConfig {
    pub relay_base_port: u16,
    pub tun2socks: Tun2SocksSpec,
    pub host_public_ip: IpAddr,
    pub tcp_echo_url: String,
    pub quic_echo_url: String,
}

#[derive(Debug)]
pub struct PipeSlot {
    pub id: SlotId,
    pub relay_port: u16,
    upstreams: UpstreamMap,
    gate: LeakGate,
    namespace: Option<NetworkNamespace>,
    relay_task: JoinHandle<()>,
}

impl PipeSlot {
    pub async fn provision(
        id: SlotId,
        config: &PipeConfig,
        upstreams: UpstreamMap,
        gate: LeakGate,
    ) -> Result<Arc<Self>, String> {
        let requested_port = config
            .relay_base_port
            .checked_add(id as u16)
            .ok_or_else(|| "relay port overflow".to_string())?;
        let namespace = NetworkNamespace::create(id, requested_port, &config.tun2socks)?;
        let relay = match RelayServer::bind(
            id,
            upstreams.clone(),
            relay_bind_address(id, requested_port),
        )
        .await
        {
            Ok(relay) => relay,
            Err(error) => {
                drop(namespace);
                return Err(format!("bind slot {id} relay: {error}"));
            }
        };
        let relay_port = relay.local_addr().port();
        let relay_task = tokio::spawn(async move {
            let _ = relay.run().await;
        });
        gate.blackhole(id);
        Ok(Arc::new(Self {
            id,
            relay_port,
            upstreams,
            gate,
            namespace: Some(namespace),
            relay_task,
        }))
    }

    /// Assign or hot-swap this slot's residential upstream and gate service on
    /// the dual-path leak probe. Any failure clears the mapping and quarantines.
    pub async fn assign<P: ExitProbe + ?Sized>(
        &self,
        proxy: &str,
        expected_exit_ip: IpAddr,
        host_public_ip: IpAddr,
        probe: &P,
    ) -> Result<LeakDecision, String> {
        let upstream = Upstream::parse(proxy, expected_exit_ip)?;
        self.gate
            .swap_and_verify(&self.upstreams, self.id, upstream, host_public_ip, probe)
            .await
    }

    pub async fn assign_box(
        &self,
        proxy: &str,
        expected_exit_ip: Option<IpAddr>,
        config: &PipeConfig,
    ) -> Result<LeakDecision, String> {
        let namespace = self
            .namespace
            .as_ref()
            .ok_or_else(|| "slot has no network namespace".to_string())?;
        let probe = NetnsExitProbe {
            namespace: namespace.name.clone(),
            tcp_echo_url: config.tcp_echo_url.clone(),
            quic_echo_url: config.quic_echo_url.clone(),
            timeout: std::time::Duration::from_secs(15),
        };
        let expected_exit_ip = match expected_exit_ip {
            Some(expected) => expected,
            None => {
                // The frozen mint request has no top-level expected-IP field.
                // Bootstrap the mapping solely for the TCP observation, reject
                // the host IP, then use that observed residential IP as the
                // expected value for the full TCP+QUIC gate.
                let upstream = Upstream::parse(proxy, config.host_public_ip)?;
                self.upstreams.clear(self.id);
                self.gate.blackhole(self.id);
                self.upstreams.swap(self.id, upstream);
                let observed = match probe.tcp_exit_ip().await {
                    Ok(observed) if observed != config.host_public_ip => observed,
                    Ok(_) => {
                        let error = format!("TCP leak: observed host IP {}", config.host_public_ip);
                        self.upstreams.clear(self.id);
                        self.gate.quarantine(self.id, error.clone());
                        return Err(error);
                    }
                    Err(error) => {
                        self.upstreams.clear(self.id);
                        self.gate.quarantine(self.id, error.clone());
                        return Err(error);
                    }
                };
                observed
            }
        };
        self.assign(proxy, expected_exit_ip, config.host_public_ip, &probe)
            .await
    }

    pub fn release(&self) {
        self.upstreams.clear(self.id);
        if !matches!(self.gate.state(self.id), SlotServiceState::Quarantined(_)) {
            self.gate.blackhole(self.id);
        }
    }

    pub fn state(&self) -> SlotServiceState {
        self.gate.state(self.id)
    }

    pub fn namespace_name(&self) -> Option<&str> {
        self.namespace
            .as_ref()
            .map(|namespace| namespace.name.as_str())
    }
}

impl Drop for PipeSlot {
    fn drop(&mut self) {
        self.release();
        self.relay_task.abort();
        if let Some(mut namespace) = self.namespace.take() {
            namespace.cleanup();
        }
    }
}

impl Default for PipeConfig {
    fn default() -> Self {
        Self {
            relay_base_port: 41_000,
            tun2socks: Tun2SocksSpec::default(),
            host_public_ip: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            tcp_echo_url: "https://api.ipify.org".into(),
            quic_echo_url: "https://cloudflare-quic.com/cdn-cgi/trace".into(),
        }
    }
}
