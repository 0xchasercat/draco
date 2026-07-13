#![cfg(target_os = "linux")]

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use super::upstream::SlotId;

#[derive(Debug, Clone)]
pub struct Tun2SocksSpec {
    pub binary: PathBuf,
    pub extra_args: Vec<String>,
}

impl Default for Tun2SocksSpec {
    fn default() -> Self {
        Self {
            binary: PathBuf::from("tun2socks"),
            extra_args: Vec::new(),
        }
    }
}

#[derive(Debug)]
pub struct NetworkNamespace {
    pub slot_id: SlotId,
    pub name: String,
    pub tun_name: String,
    pub host_veth: String,
    pub namespace_veth: String,
    pub host_ip: Ipv4Addr,
    pub namespace_ip: Ipv4Addr,
    tun2socks: Option<Child>,
}

impl NetworkNamespace {
    /// Box-only provisioning. This requires root plus `ip`, `iptables`, and a
    /// tun2socks implementation accepting the documented universal flags.
    pub fn create(slot_id: SlotId, relay_port: u16, spec: &Tun2SocksSpec) -> Result<Self, String> {
        ensure_root()?;
        ensure_command("ip")?;
        ensure_command("iptables")?;
        ensure_executable(&spec.binary)?;

        let name = format!("draco-h-{slot_id}");
        let host_veth = format!("dh{slot_id}h");
        let namespace_veth = format!("dh{slot_id}n");
        let tun_name = "tun0".to_string();
        let host_ip = slot_host_ip(slot_id);
        let namespace_ip = slot_namespace_ip(slot_id);
        let mut namespace = Self {
            slot_id,
            name,
            tun_name,
            host_veth,
            namespace_veth,
            host_ip,
            namespace_ip,
            tun2socks: None,
        };

        if let Err(error) = namespace.configure(relay_port, spec) {
            namespace.cleanup();
            return Err(error);
        }
        Ok(namespace)
    }

    fn configure(&mut self, relay_port: u16, spec: &Tun2SocksSpec) -> Result<(), String> {
        run("ip", &["netns", "add", &self.name])?;
        run(
            "ip",
            &[
                "link",
                "add",
                &self.host_veth,
                "type",
                "veth",
                "peer",
                "name",
                &self.namespace_veth,
            ],
        )?;
        run(
            "ip",
            &["link", "set", &self.namespace_veth, "netns", &self.name],
        )?;
        run(
            "ip",
            &[
                "addr",
                "add",
                &format!("{}/30", self.host_ip),
                "dev",
                &self.host_veth,
            ],
        )?;
        run("ip", &["link", "set", &self.host_veth, "up"])?;

        self.ip(&["link", "set", "lo", "up"])?;
        self.ip(&[
            "addr",
            "add",
            &format!("{}/30", self.namespace_ip),
            "dev",
            &self.namespace_veth,
        ])?;
        self.ip(&["link", "set", &self.namespace_veth, "up"])?;
        self.ip(&["tuntap", "add", "dev", &self.tun_name, "mode", "tun"])?;
        self.ip(&[
            "addr",
            "add",
            &format!("198.18.{}.1/30", self.slot_id % 250),
            "dev",
            &self.tun_name,
        ])?;
        self.ip(&["link", "set", &self.tun_name, "up"])?;
        self.ip(&["route", "add", "default", "dev", &self.tun_name])?;
        self.ip(&[
            "route",
            "add",
            &format!("{}/32", self.host_ip),
            "dev",
            &self.namespace_veth,
        ])?;

        // Default-deny namespace output. Browser TCP/UDP/DNS can only enter the
        // tun. The sole veth exception is tun2socks reaching this slot's stable
        // relay port on the host; there is no general host or internet route.
        self.iptables(&["-P", "OUTPUT", "DROP"])?;
        self.iptables(&["-A", "OUTPUT", "-o", "lo", "-j", "ACCEPT"])?;
        self.iptables(&["-A", "OUTPUT", "-o", &self.tun_name, "-j", "ACCEPT"])?;
        for protocol in ["tcp", "udp"] {
            self.iptables(&[
                "-A",
                "OUTPUT",
                "-o",
                &self.namespace_veth,
                "-d",
                &self.host_ip.to_string(),
                "-p",
                protocol,
                "--dport",
                &relay_port.to_string(),
                "-j",
                "ACCEPT",
            ])?;
        }

        let mut command = Command::new("ip");
        command
            .arg("netns")
            .arg("exec")
            .arg(&self.name)
            .arg(&spec.binary)
            .arg("-device")
            .arg(format!("tun://{}", self.tun_name))
            .arg("-proxy")
            .arg(format!("socks5://{}:{relay_port}", self.host_ip))
            .arg("-interface")
            .arg(&self.namespace_veth)
            .args(&spec.extra_args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit());
        self.tun2socks = Some(
            command
                .spawn()
                .map_err(|error| format!("spawn tun2socks for {}: {error}", self.name))?,
        );
        Ok(())
    }

    pub fn exec_command(&self, program: &str) -> Command {
        let mut command = Command::new("ip");
        command
            .arg("netns")
            .arg("exec")
            .arg(&self.name)
            .arg(program);
        command
    }

    fn ip(&self, args: &[&str]) -> Result<(), String> {
        let mut owned = vec![
            "netns".to_string(),
            "exec".into(),
            self.name.clone(),
            "ip".into(),
        ];
        owned.extend(args.iter().map(|value| (*value).to_string()));
        run_owned("ip", &owned)
    }

    fn iptables(&self, args: &[&str]) -> Result<(), String> {
        let mut owned = vec![
            "netns".to_string(),
            "exec".into(),
            self.name.clone(),
            "iptables".into(),
        ];
        owned.extend(args.iter().map(|value| (*value).to_string()));
        run_owned("ip", &owned)
    }

    pub fn cleanup(&mut self) {
        if let Some(mut child) = self.tun2socks.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        let _ = Command::new("ip")
            .args(["netns", "delete", &self.name])
            .status();
        let _ = Command::new("ip")
            .args(["link", "delete", &self.host_veth])
            .status();
    }
}

impl Drop for NetworkNamespace {
    fn drop(&mut self) {
        self.cleanup();
    }
}

fn ensure_root() -> Result<(), String> {
    let output = Command::new("id")
        .arg("-u")
        .output()
        .map_err(|error| format!("run id -u: {error}"))?;
    if String::from_utf8_lossy(&output.stdout).trim() != "0" {
        return Err("pipe provisioning requires root".into());
    }
    Ok(())
}

fn ensure_command(command: &str) -> Result<(), String> {
    let status = Command::new(command)
        .arg("-Version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    if status.is_err() {
        return Err(format!("required box command {command:?} was not found"));
    }
    Ok(())
}

fn ensure_executable(path: &Path) -> Result<(), String> {
    if path.is_absolute() && !path.is_file() {
        return Err(format!("tun2socks binary not found at {}", path.display()));
    }
    Ok(())
}

fn run(program: &str, args: &[&str]) -> Result<(), String> {
    let owned: Vec<String> = args.iter().map(|value| (*value).to_string()).collect();
    run_owned(program, &owned)
}

fn run_owned(program: &str, args: &[String]) -> Result<(), String> {
    let output = Command::new(program)
        .args(args)
        .output()
        .map_err(|error| format!("spawn {program}: {error}"))?;
    if output.status.success() {
        return Ok(());
    }
    Err(format!(
        "{} {} failed: {}",
        program,
        args.join(" "),
        String::from_utf8_lossy(&output.stderr).trim()
    ))
}

pub fn slot_host_ip(slot_id: SlotId) -> Ipv4Addr {
    Ipv4Addr::new(169, 254, (slot_id % 200 + 10) as u8, 1)
}

pub fn slot_namespace_ip(slot_id: SlotId) -> Ipv4Addr {
    Ipv4Addr::new(169, 254, (slot_id % 200 + 10) as u8, 2)
}

pub fn relay_bind_address(slot_id: SlotId, port: u16) -> SocketAddr {
    SocketAddr::new(IpAddr::V4(slot_host_ip(slot_id)), port)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relay_binds_only_the_slot_host_veth() {
        assert_eq!(
            relay_bind_address(0, 41000),
            "169.254.10.1:41000".parse().unwrap()
        );
    }

    #[test]
    #[ignore = "box-only: requires root, iptables, tun, and tun2socks"]
    fn provisions_and_tears_down_fail_closed_namespace() {
        let _namespace = NetworkNamespace::create(190, 41900, &Tun2SocksSpec::default()).unwrap();
    }
}
