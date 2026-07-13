use std::env;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

const CACHE_SCHEMA: u32 = 2;

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

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BrowserChannel {
    Chrome,
    Chromium,
    Edge,
    Brave,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct DetectedBrowser {
    pub channel: BrowserChannel,
    pub path: PathBuf,
    pub version: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct LaunchCapabilities {
    pub headless_new: bool,
    pub headed: bool,
    pub display: Option<String>,
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
    pub browser: Option<DetectedBrowser>,
    pub launch_capabilities: LaunchCapabilities,
}

#[derive(Debug, Clone)]
pub struct ResolvedHostConfig {
    pub config: HostConfig,
    pub cache_hit: bool,
    pub cache_present: bool,
    pub cache_error: Option<String>,
}

struct HostProbe {
    display: Option<String>,
    xvfb_path: Option<PathBuf>,
    vglrun_path: Option<PathBuf>,
    dri_present: bool,
    nvidia_present: bool,
    platform_gpu: bool,
    browser: Option<DetectedBrowser>,
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
    // TTL is the invalidation policy for the fast path. A valid entry avoids
    // every host probe, including nvidia-smi and browser process execution.
    if !refresh {
        if let Some(config) = read_valid_cache(cache_path, ttl) {
            return ResolvedHostConfig {
                config,
                cache_hit: true,
                cache_present: true,
                cache_error: None,
            };
        }
    }

    let probe = probe_host();
    let fingerprint = host_fingerprint(&probe);
    let config = discover(probe, fingerprint);
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

fn probe_host() -> HostProbe {
    HostProbe {
        display: native_display(),
        xvfb_path: find_executable("Xvfb"),
        vglrun_path: find_executable("vglrun"),
        dri_present: Path::new("/dev/dri").exists(),
        nvidia_present: command_succeeds("nvidia-smi", &["-L"]),
        platform_gpu: matches!(env::consts::OS, "macos" | "windows"),
        browser: detect_browser(),
    }
}

fn discover(probe: HostProbe, fingerprint: String) -> HostConfig {
    let gpu_present = (probe.dri_present || probe.nvidia_present || probe.platform_gpu)
        && env::var_os("LIBGL_ALWAYS_SOFTWARE").is_none();
    let launch_capabilities = LaunchCapabilities {
        headless_new: probe
            .browser
            .as_ref()
            .is_some_and(browser_supports_headless_new),
        headed: probe.display.is_some() || probe.platform_gpu || probe.xvfb_path.is_some(),
        display: probe
            .display
            .clone()
            .or_else(|| probe.xvfb_path.as_ref().map(|_| ":99".into())),
    };

    let (render_mode, render_tier, resolved_display, command_prefix, chrome_args) =
        if gpu_present && (probe.display.is_some() || probe.platform_gpu) {
            (
                RenderMode::Gpu,
                RenderTier::NativeDisplayGpu,
                probe.display.clone(),
                Vec::new(),
                gpu_chrome_args(),
            )
        } else if gpu_present && probe.xvfb_path.is_some() {
            let command_prefix = probe
                .vglrun_path
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
                probe.xvfb_path.as_ref().map(|_| ":99".into()),
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
        xvfb_path: probe.xvfb_path,
        vglrun_path: probe.vglrun_path,
        browser: probe.browser,
        launch_capabilities,
    }
}

fn native_display() -> Option<String> {
    env::var("DISPLAY")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| {
            env::var("WAYLAND_DISPLAY")
                .ok()
                .filter(|value| !value.trim().is_empty())
        })
}

fn detect_browser() -> Option<DetectedBrowser> {
    browser_candidates().into_iter().find_map(|candidate| {
        candidate.path.is_file().then(|| DetectedBrowser {
            channel: candidate.channel,
            version: browser_version(&candidate.path),
            path: candidate.path,
        })
    })
}

struct BrowserCandidate {
    channel: BrowserChannel,
    path: PathBuf,
}

fn browser_candidates() -> Vec<BrowserCandidate> {
    let mut candidates = Vec::new();
    let names: &[(BrowserChannel, &str)] = match env::consts::OS {
        "windows" => &[
            (BrowserChannel::Chrome, "chrome.exe"),
            (BrowserChannel::Chromium, "chromium.exe"),
            (BrowserChannel::Edge, "msedge.exe"),
            (BrowserChannel::Brave, "brave.exe"),
        ],
        "macos" => &[
            (BrowserChannel::Chrome, "Google Chrome"),
            (BrowserChannel::Chromium, "Chromium"),
            (BrowserChannel::Edge, "Microsoft Edge"),
            (BrowserChannel::Brave, "Brave Browser"),
        ],
        _ => &[
            (BrowserChannel::Chrome, "google-chrome-stable"),
            (BrowserChannel::Chrome, "google-chrome"),
            (BrowserChannel::Chromium, "chromium"),
            (BrowserChannel::Chromium, "chromium-browser"),
            (BrowserChannel::Edge, "microsoft-edge-stable"),
            (BrowserChannel::Edge, "microsoft-edge"),
            (BrowserChannel::Brave, "brave-browser"),
        ],
    };

    for &(channel, name) in names {
        if let Some(path) = find_executable(name) {
            push_candidate(&mut candidates, channel, path);
        }
    }

    match env::consts::OS {
        "macos" => {
            for (channel, path) in [
                (
                    BrowserChannel::Chrome,
                    "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
                ),
                (
                    BrowserChannel::Chromium,
                    "/Applications/Chromium.app/Contents/MacOS/Chromium",
                ),
                (
                    BrowserChannel::Edge,
                    "/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge",
                ),
                (
                    BrowserChannel::Brave,
                    "/Applications/Brave Browser.app/Contents/MacOS/Brave Browser",
                ),
            ] {
                push_candidate(&mut candidates, channel, PathBuf::from(path));
            }
        }
        "windows" => add_windows_candidates(&mut candidates),
        _ => {}
    }

    candidates
}

fn add_windows_candidates(candidates: &mut Vec<BrowserCandidate>) {
    for root in ["PROGRAMFILES", "PROGRAMFILES(X86)", "LOCALAPPDATA"] {
        let Some(root) = env::var_os(root) else {
            continue;
        };
        let root = PathBuf::from(root);
        for (channel, suffix) in [
            (
                BrowserChannel::Chrome,
                "Google/Chrome/Application/chrome.exe",
            ),
            (BrowserChannel::Chromium, "Chromium/Application/chrome.exe"),
            (
                BrowserChannel::Edge,
                "Microsoft/Edge/Application/msedge.exe",
            ),
            (
                BrowserChannel::Brave,
                "BraveSoftware/Brave-Browser/Application/brave.exe",
            ),
        ] {
            push_candidate(candidates, channel, root.join(suffix));
        }
    }
}

fn push_candidate(candidates: &mut Vec<BrowserCandidate>, channel: BrowserChannel, path: PathBuf) {
    if !candidates.iter().any(|candidate| candidate.path == path) {
        candidates.push(BrowserCandidate { channel, path });
    }
}

fn browser_version(path: &Path) -> Option<String> {
    let output = Command::new(path).arg("--version").output().ok()?;
    if !output.status.success() {
        return None;
    }
    let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!value.is_empty()).then_some(value)
}

fn browser_supports_headless_new(browser: &DetectedBrowser) -> bool {
    browser
        .version
        .as_deref()
        .and_then(first_version_number)
        .is_none_or(|major| major >= 109)
}

fn first_version_number(value: &str) -> Option<u32> {
    value
        .split(|character: char| !character.is_ascii_digit())
        .find(|part| !part.is_empty())?
        .parse()
        .ok()
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

fn read_valid_cache(path: &Path, ttl: Duration) -> Option<HostConfig> {
    let bytes = fs::read(path).ok()?;
    let config: HostConfig = serde_json::from_slice(&bytes).ok()?;
    if config.schema_version != CACHE_SCHEMA {
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

fn host_fingerprint(probe: &HostProbe) -> String {
    let browser = probe
        .browser
        .as_ref()
        .map(|browser| {
            format!(
                "{:?}:{}:{}",
                browser.channel,
                browser.path.display(),
                browser.version.as_deref().unwrap_or("-")
            )
        })
        .unwrap_or_else(|| "-".into());
    format!(
        "v{CACHE_SCHEMA}|{}|{}|display={}|dri={}|nvidia={}|xvfb={}|vglrun={}|browser={browser}",
        env::consts::OS,
        env::consts::ARCH,
        probe.display.as_deref().unwrap_or("-"),
        probe.dri_present,
        probe.nvidia_present,
        path_marker(probe.xvfb_path.as_ref()),
        path_marker(probe.vglrun_path.as_ref()),
    )
}

fn path_marker(path: Option<&PathBuf>) -> String {
    path.map(|value| value.display().to_string())
        .unwrap_or_else(|| "-".into())
}

fn find_executable(name: impl AsRef<OsStr>) -> Option<PathBuf> {
    let name = name.as_ref();
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

    #[test]
    fn parses_browser_major_version() {
        assert_eq!(first_version_number("Google Chrome 126.0.1"), Some(126));
        assert_eq!(first_version_number("Chromium 109"), Some(109));
        assert_eq!(first_version_number("unknown"), None);
    }

    #[test]
    fn old_browser_does_not_claim_new_headless() {
        let browser = DetectedBrowser {
            channel: BrowserChannel::Chromium,
            path: PathBuf::from("chromium"),
            version: Some("Chromium 108.0".into()),
        };
        assert!(!browser_supports_headless_new(&browser));
    }

    #[test]
    fn unknown_version_optimistically_tries_new_headless() {
        let browser = DetectedBrowser {
            channel: BrowserChannel::Chrome,
            path: PathBuf::from("chrome"),
            version: None,
        };
        assert!(browser_supports_headless_new(&browser));
    }

    #[test]
    fn xvfb_makes_headed_ladder_available() {
        let config = discover(
            HostProbe {
                display: None,
                xvfb_path: Some(PathBuf::from("/usr/bin/Xvfb")),
                vglrun_path: None,
                dri_present: true,
                nvidia_present: false,
                platform_gpu: false,
                browser: Some(DetectedBrowser {
                    channel: BrowserChannel::Chrome,
                    path: PathBuf::from("chrome"),
                    version: Some("Google Chrome 126".into()),
                }),
            },
            "test".into(),
        );
        assert!(config.launch_capabilities.headed);
        assert_eq!(config.launch_capabilities.display.as_deref(), Some(":99"));
    }
}
