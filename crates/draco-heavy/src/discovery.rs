use std::env;
use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

const CACHE_SCHEMA: u32 = 1;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum RenderMode {
    Gpu,
    Swiftshader,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RenderTier {
    NativeDisplayGpu,
    VirtualDisplayGpu,
    Swiftshader,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct HostConfig {
    pub schema_version: u32,
    pub discovered_at_unix: u64,
    pub fingerprint: String,
    pub render_mode: RenderMode,
    pub render_tier: RenderTier,
    pub display: Option<String>,
    pub command_prefix: Vec<String>,
    pub chrome_args: Vec<String>,
    pub gpu_present: bool,
    pub xvfb_path: Option<PathBuf>,
    pub vglrun_path: Option<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct ResolvedHostConfig {
    pub config: HostConfig,
    pub cache_hit: bool,
    pub cache_present: bool,
    pub cache_error: Option<String>,
}

pub fn default_cache_path() -> PathBuf {
    if let Some(dir) = env::var_os("XDG_CACHE_HOME").filter(|value| !value.is_empty()) {
        return PathBuf::from(dir).join("draco/heavy-host.json");
    }
    if let Some(home) = env::var_os("HOME").filter(|value| !value.is_empty()) {
        return PathBuf::from(home).join(".cache/draco/heavy-host.json");
    }
    env::temp_dir().join("draco/heavy-host.json")
}

pub fn resolve(cache_path: &Path, ttl: Duration, refresh: bool) -> ResolvedHostConfig {
    let fingerprint = host_fingerprint();
    if !refresh {
        if let Some(config) = read_valid_cache(cache_path, ttl, &fingerprint) {
            return ResolvedHostConfig {
                config,
                cache_hit: true,
                cache_present: true,
                cache_error: None,
            };
        }
    }

    let config = discover(fingerprint);
    let cache_error = write_cache(cache_path, &config)
        .err()
        .map(|error| error.to_string());
    ResolvedHostConfig {
        config,
        cache_hit: false,
        cache_present: cache_error.is_none(),
        cache_error,
    }
}

pub fn discover(fingerprint: String) -> HostConfig {
    let display = env::var("DISPLAY")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| {
            env::var("WAYLAND_DISPLAY")
                .ok()
                .filter(|value| !value.trim().is_empty())
        });
    let xvfb_path = find_executable("Xvfb");
    let vglrun_path = find_executable("vglrun");
    let dri_present = Path::new("/dev/dri").exists();
    let nvidia_present = command_succeeds("nvidia-smi", &["-L"]);
    let platform_gpu = matches!(env::consts::OS, "macos" | "windows");
    let gpu_present = (dri_present || nvidia_present || platform_gpu)
        && env::var_os("LIBGL_ALWAYS_SOFTWARE").is_none();

    let (render_mode, render_tier, resolved_display, command_prefix, chrome_args) =
        if let Some(display) = display.filter(|_| gpu_present) {
            (
                RenderMode::Gpu,
                RenderTier::NativeDisplayGpu,
                Some(display),
                Vec::new(),
                gpu_chrome_args(),
            )
        } else if gpu_present && xvfb_path.is_some() {
            let command_prefix = vglrun_path
                .as_ref()
                .map(|path| vec![path.display().to_string(), "-d".into(), "egl0".into()])
                .unwrap_or_default();
            (
                RenderMode::Gpu,
                RenderTier::VirtualDisplayGpu,
                Some(":99".into()),
                command_prefix,
                gpu_chrome_args(),
            )
        } else {
            (
                RenderMode::Swiftshader,
                RenderTier::Swiftshader,
                xvfb_path.as_ref().map(|_| ":99".into()),
                Vec::new(),
                vec!["--use-angle=swiftshader".into()],
            )
        };

    HostConfig {
        schema_version: CACHE_SCHEMA,
        discovered_at_unix: unix_now(),
        fingerprint,
        render_mode,
        render_tier,
        display: resolved_display,
        command_prefix,
        chrome_args,
        gpu_present,
        xvfb_path,
        vglrun_path,
    }
}

fn gpu_chrome_args() -> Vec<String> {
    [
        "--use-gl=angle",
        "--use-angle=gl",
        "--enable-gpu-rasterization",
        "--ignore-gpu-blocklist",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect()
}

fn read_valid_cache(path: &Path, ttl: Duration, fingerprint: &str) -> Option<HostConfig> {
    let bytes = fs::read(path).ok()?;
    let config: HostConfig = serde_json::from_slice(&bytes).ok()?;
    if config.schema_version != CACHE_SCHEMA || config.fingerprint != fingerprint {
        return None;
    }
    let age = unix_now().saturating_sub(config.discovered_at_unix);
    (age <= ttl.as_secs()).then_some(config)
}

fn write_cache(path: &Path, config: &HostConfig) -> io::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    let bytes = serde_json::to_vec_pretty(config)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    let temp = path.with_extension(format!("json.tmp-{}", std::process::id()));
    fs::write(&temp, bytes)?;
    if let Err(first_error) = fs::rename(&temp, path) {
        // Windows does not replace an existing destination with rename. Retry
        // after removing the stale cache; Unix normally succeeds above.
        if path.exists() {
            fs::remove_file(path)?;
            fs::rename(&temp, path)?;
        } else {
            let _ = fs::remove_file(&temp);
            return Err(first_error);
        }
    }
    Ok(())
}

fn host_fingerprint() -> String {
    let display = env::var("DISPLAY")
        .or_else(|_| env::var("WAYLAND_DISPLAY"))
        .unwrap_or_default();
    let dri = Path::new("/dev/dri").exists();
    let nvidia = find_executable("nvidia-smi");
    let xvfb = find_executable("Xvfb");
    let vglrun = find_executable("vglrun");
    format!(
        "v{CACHE_SCHEMA}|{}|{}|display={display}|dri={dri}|nvidia={}|xvfb={}|vglrun={}",
        env::consts::OS,
        env::consts::ARCH,
        path_marker(nvidia),
        path_marker(xvfb),
        path_marker(vglrun),
    )
}

fn path_marker(path: Option<PathBuf>) -> String {
    path.map(|value| value.display().to_string())
        .unwrap_or_else(|| "-".into())
}

fn find_executable(name: &str) -> Option<PathBuf> {
    let path: OsString = env::var_os("PATH")?;
    env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

fn command_succeeds(program: &str, args: &[&str]) -> bool {
    find_executable(program)
        .and_then(|path| Command::new(path).args(args).output().ok())
        .is_some_and(|output| output.status.success())
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_round_trip_hits_fast_path() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("heavy-host.json");
        let first = resolve(&path, Duration::from_secs(60), true);
        assert!(!first.cache_hit);
        assert!(first.cache_present);
        let second = resolve(&path, Duration::from_secs(60), false);
        assert!(second.cache_hit);
        assert_eq!(first.config, second.config);
    }

    #[test]
    fn stale_cache_is_refreshed() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("heavy-host.json");
        let first = resolve(&path, Duration::ZERO, true);
        std::thread::sleep(Duration::from_secs(1));
        let second = resolve(&path, Duration::ZERO, false);
        assert!(!second.cache_hit);
        assert!(second.config.discovered_at_unix > first.config.discovered_at_unix);
    }
}
