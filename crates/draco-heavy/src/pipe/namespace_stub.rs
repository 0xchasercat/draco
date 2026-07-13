use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::process::Command;

use super::upstream::SlotId;

#[derive(Debug, Clone, Default)]
pub struct Tun2SocksSpec {
    pub binary: PathBuf,
    pub extra_args: Vec<String>,
}

#[derive(Debug)]
pub struct NetworkNamespace {
    pub slot_id: SlotId,
    pub name: String,
}

impl NetworkNamespace {
    pub fn create(
        _slot_id: SlotId,
        _relay_port: u16,
        _spec: &Tun2SocksSpec,
    ) -> Result<Self, String> {
        Err("the pipe feature can only provision namespaces on Linux".into())
    }

    pub fn exec_command(&self, program: &str) -> Command {
        Command::new(program)
    }

    pub fn cleanup(&mut self) {}
}

pub fn slot_host_ip(_slot_id: SlotId) -> Ipv4Addr {
    Ipv4Addr::LOCALHOST
}

pub fn slot_namespace_ip(_slot_id: SlotId) -> Ipv4Addr {
    Ipv4Addr::LOCALHOST
}

pub fn relay_bind_address(slot_id: SlotId, port: u16) -> SocketAddr {
    SocketAddr::new(IpAddr::V4(slot_host_ip(slot_id)), port)
}
