use crate::config::{AppConfig, ProxyProfile, ProxyProtocol};
use anyhow::Context;
#[cfg(windows)]
use std::os::windows::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};

/// Resolve the path of a proxy binary:
/// 1. If `override_path` is given and the file exists, use it.
/// 2. Try the directory that contains the running executable.
/// 3. Fall back to just `name` and let the OS $PATH resolve it.
fn find_binary(name: &str, override_path: Option<&PathBuf>) -> PathBuf {
    if let Some(path) = override_path {
        if path.exists() {
            return path.clone();
        }
    }
    // Directory that contains the running juicity-gui executable.
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let candidate = dir.join(name);
            if candidate.exists() {
                return candidate;
            }
        }
    }
    // Current working directory (useful when running from the project root during development).
    if let Ok(cwd) = std::env::current_dir() {
        let candidate = cwd.join(name);
        if candidate.exists() {
            return candidate;
        }
    }
    PathBuf::from(name)
}

#[derive(Debug)]
struct RunningCore {
    protocol: ProxyProtocol,
    /// Display name of the profile this core was started for.
    name: String,
    child: Child,
    /// Temporary config file written for this run; deleted on stop.
    temp_config: Option<PathBuf>,
}

impl Drop for RunningCore {
    // Safety net: never leave the proxy child (or its credentials) behind.
    fn drop(&mut self) {
        let _ = self.child.kill();
        if let Some(path) = &self.temp_config {
            let _ = std::fs::remove_file(path);
        }
    }
}

#[derive(Debug, Default)]
pub struct CoreManager {
    running: Option<RunningCore>,
}

impl CoreManager {
    pub fn new() -> Self {
        Self { running: None }
    }

    pub fn is_running(&self) -> bool {
        self.running.is_some()
    }

    /// Name of the profile the running core was started with.
    pub fn current_name(&self) -> Option<&str> {
        self.running.as_ref().map(|v| v.name.as_str())
    }

    pub fn current_protocol(&self) -> Option<ProxyProtocol> {
        self.running.as_ref().map(|v| v.protocol)
    }

    pub fn start_profile(
        &mut self,
        config: &AppConfig,
        profile: &ProxyProfile,
    ) -> anyhow::Result<()> {
        // Wait for the old process so its listen port is free for the new one.
        self.stop_and_wait();

        let (mut cmd, temp_config) = build_command(config, profile)?;
        tracing::info!(
            "starting {:?} core for profile {}",
            profile.protocol,
            profile.name
        );

        let child = match cmd.spawn() {
            Ok(child) => child,
            Err(err) => {
                if let Some(path) = temp_config.as_ref() {
                    let _ = std::fs::remove_file(path);
                }
                return Err(err).with_context(|| {
                    format!("failed to spawn {:?} core process", profile.protocol)
                });
            }
        };

        self.running = Some(RunningCore {
            protocol: profile.protocol,
            name: profile.display_name(),
            child,
            temp_config,
        });
        Ok(())
    }

    /// Kill the core and reap it on a background thread (keeps the UI responsive).
    pub fn stop(&mut self) -> anyhow::Result<()> {
        if let Some(mut running) = self.running.take() {
            tracing::info!("stopping {:?} core", running.protocol);
            let _ = running.child.kill();
            // Dropping `running` after the wait removes the temp config.
            std::thread::spawn(move || {
                let _ = running.child.wait();
            });
        }
        Ok(())
    }

    /// Kill the core and block until it has exited (used before a restart and
    /// on application exit).
    pub fn stop_and_wait(&mut self) {
        if let Some(mut running) = self.running.take() {
            tracing::info!("stopping {:?} core", running.protocol);
            let _ = running.child.kill();
            let _ = running.child.wait();
        }
    }

    pub fn poll(&mut self) -> anyhow::Result<Option<std::process::ExitStatus>> {
        if let Some(running) = &mut self.running {
            if let Some(status) = running
                .child
                .try_wait()
                .with_context(|| format!("failed to poll {:?} process", running.protocol))?
            {
                self.running = None;
                return Ok(Some(status));
            }
        }
        Ok(None)
    }
}

/// Returns the command and an optional temp config path that must be cleaned up.
fn build_command(
    config: &AppConfig,
    profile: &ProxyProfile,
) -> anyhow::Result<(Command, Option<PathBuf>)> {
    match profile.protocol {
        ProxyProtocol::Juicity => build_juicity_command(config, profile),
        ProxyProtocol::Shadowsocks => build_shadowsocks_command(config, profile),
    }
}

fn write_temp_config(prefix: &str, json: &str) -> anyhow::Result<PathBuf> {
    let pid = std::process::id();
    let path = std::env::temp_dir().join(format!("{}{}-config.json", prefix, pid));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    // The file contains credentials: keep it private to the current user.
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let write = || -> std::io::Result<()> {
        use std::io::Write;
        options.open(&path)?.write_all(json.as_bytes())
    };
    write().with_context(|| format!("failed to write temp config {}", path.display()))?;
    Ok(path)
}

fn build_juicity_command(
    config: &AppConfig,
    profile: &ProxyProfile,
) -> anyhow::Result<(Command, Option<PathBuf>)> {
    let binary = find_binary("juicity-client", config.juicity_client_path.as_ref());

    // Use legacy config_path if set; otherwise generate from individual fields.
    let (config_path, temp) = if let Some(path) = &profile.config_path {
        (path.clone(), None)
    } else {
        let sni = profile
            .sni
            .as_deref()
            .filter(|s| !s.is_empty())
            .unwrap_or(&profile.server);
        let json = serde_json::json!({
            "listen": config.socks_listen,
            "server": crate::util::format_host_port(&profile.server, profile.server_port),
            "uuid": profile.uuid,
            "password": profile.password,
            "sni": sni,
            "allow_insecure": profile.allow_insecure,
            "log_level": "info"
        });
        let path = write_temp_config("juicity-gui-", &json.to_string())?;
        (path.clone(), Some(path))
    };

    let mut cmd = Command::new(binary);
    cmd.arg("run")
        .arg("-c")
        .arg(&config_path)
        .arg("--log-level")
        .arg("info")
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // On Windows, prevent the child process from opening a console window.
    #[cfg(windows)]
    cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    Ok((cmd, temp))
}

fn build_shadowsocks_command(
    config: &AppConfig,
    profile: &ProxyProfile,
) -> anyhow::Result<(Command, Option<PathBuf>)> {
    let binary = find_binary("sslocal", config.ss_local_path.as_ref());

    // Use legacy config_path if set; otherwise generate from individual fields.
    let (config_path, temp) = if let Some(path) = &profile.config_path {
        (path.clone(), None)
    } else {
        let (local_addr, local_port) = crate::util::split_host_port(&config.socks_listen);
        let (http_addr, http_port) = crate::util::split_host_port(&config.http_listen);

        let mut json = serde_json::json!({
            "server": profile.server,
            "server_port": profile.server_port,
            "password": profile.password,
            "method": profile.method,
            "locals": [
                {
                    "local_address": local_addr,
                    "local_port": local_port,
                    "protocol": "socks"
                },
                {
                    "local_address": http_addr,
                    "local_port": http_port,
                    "protocol": "http"
                }
            ],
            "timeout": profile.timeout
        });
        if let Some(plugin) = &profile.plugin {
            json["plugin"] = serde_json::Value::String(plugin.clone());
        }
        if let Some(opts) = &profile.plugin_opts {
            json["plugin_opts"] = serde_json::Value::String(opts.clone());
        }
        if let Some(args) = &profile.plugin_args {
            json["plugin_args"] = serde_json::Value::String(args.clone());
        }
        let path = write_temp_config("juicity-gui-ss-", &json.to_string())?;
        (path.clone(), Some(path))
    };

    let mut cmd = Command::new(binary);
    cmd.arg("-c")
        .arg(&config_path)
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // On Windows, prevent the child process from opening a console window.
    #[cfg(windows)]
    cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    Ok((cmd, temp))
}
