use clap::{Parser, Subcommand};
use juicity_common::cert;
use juicity_common::config::Config;
use juicity_common::link;
use juicity_common::BuildInfo;
use tracing_subscriber::EnvFilter;

#[global_allocator]
static GLOBAL_ALLOCATOR: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[derive(Parser, Debug)]
#[command(
    name = "juicity-server",
    about = "A QUIC-based proxy server",
    disable_version_flag = true,
)]
struct Cli {
    /// Show version information
    #[arg(short = 'v', long = "version", help = "Print version information")]
    version: bool,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Run the proxy server
    Run {
        /// Config file path
        #[arg(short = 'c', long = "config")]
        config: String,

        /// Log level
        #[arg(long = "log-level", default_value = "info")]
        log_level: String,
    },

    /// Export share link, QR code, or JSON config
    Export {
        /// Config file path
        #[arg(short = 'c', long = "config")]
        config: String,

        /// Print share link to stdout
        #[arg(long = "link")]
        link: bool,

        /// Print QR code to terminal
        #[arg(long = "qrcode")]
        qrcode: bool,

        /// Save QR code as PNG file
        #[arg(long = "qrcode-png")]
        qrcode_png: Option<String>,

        /// Export server config as JSON
        #[arg(long = "json-server")]
        json_server: bool,

        /// Export client config derived from this server config as JSON
        #[arg(long = "json-client")]
        json_client: bool,

        /// SOCKS inbound listen port written into the exported client config
        #[arg(long = "socks-port", default_value = "1080")]
        socks_port: u16,

        /// Network interface name to use for the share link host (e.g. eth0).
        /// If not specified, an interactive selection will be shown.
        #[arg(long = "interface")]
        interface: Option<String>,

        /// Domain to use as SNI in the share link.
        /// If not specified and the certificate is a wildcard or multi-domain,
        /// an interactive prompt will be shown.
        #[arg(long = "domain")]
        domain: Option<String>,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Install the default rustls CryptoProvider (aws-lc-rs)
    rustls::crypto::aws_lc_rs::default_provider()
        .install_default()
        .expect("Failed to install default rustls CryptoProvider");

    let cli = Cli::parse();

    // Handle -v/--version before any subcommand logic
    if cli.version {
        println!("{}", BuildInfo::version_string());
        return Ok(());
    }

    let Some(command) = cli.command else {
        // No subcommand and no --version flag; show help
        let mut cmd = <Cli as clap::CommandFactory>::command();
        cmd.print_help()?;
        println!();
        return Ok(());
    };

    match command {
        Commands::Run { config, log_level } => {
            tracing_subscriber::fmt()
                .with_env_filter(
                    EnvFilter::try_from_default_env()
                        .unwrap_or_else(|_| EnvFilter::new(&log_level)),
                )
                .init();

            let config = Config::from_file(&config)?;
            config.validate_for_server()?;

            tracing::info!("Juicity server starting...");

            let srv = juicity_server::server::JuicityServer::new(&config).await?;
            srv.serve(&config.listen).await?;
        }

        Commands::Export {
            config,
            link: do_link,
            qrcode,
            qrcode_png,
            json_server,
            json_client,
            socks_port,
            interface,
            domain,
        } => {
            let config = Config::from_file(&config)?;

            if do_link || qrcode || qrcode_png.is_some() {
                // Step 1: Resolve host (from --interface or interactive selection)
                let host_override = resolve_export_host(interface.as_deref())?;

                // Step 2: Resolve SNI (from --domain, certificate parsing, or interactive prompt)
                let sni_override =
                    resolve_export_sni(&config, domain.as_deref())?;

                // Step 3: Generate share link
                let share_link = link::generate_share_link(
                    &config,
                    host_override.as_deref(),
                    sni_override.as_deref(),
                )
                .map_err(|e| anyhow::anyhow!("Failed to generate share link: {}", e))?;

                // Step 4: Output
                if do_link {
                    println!("{}", share_link);
                }
                if qrcode {
                    link::print_qrcode(&share_link)?;
                }
                if let Some(path) = qrcode_png {
                    link::save_qrcode_png(&share_link, &path)?;
                }
            }

            if json_server {
                println!("{}", config.to_server_json()?);
            }

            if json_client {
                println!("{}", config.to_client_json_from_server(socks_port)?);
            }
        }
    }

    Ok(())
}

// ── Host Resolution ──

/// Resolve the host to use in the share link.
///
/// Priority:
/// 1. If `--interface` is specified, read that interface's IP.
/// 2. Otherwise, interactively list available interfaces and let the user pick.
fn resolve_export_host(interface: Option<&str>) -> anyhow::Result<Option<String>> {
    match interface {
        Some(iface) => Ok(Some(get_interface_ip(iface)?)),
        None => Ok(Some(interactive_select_interface()?)),
    }
}

/// Get the IP address of a specific network interface by name.
///
/// Prefers IPv4; falls back to IPv6.
fn get_interface_ip(interface_name: &str) -> anyhow::Result<String> {
    use pnet::datalink;

    let interfaces = datalink::interfaces();
    let iface = interfaces
        .into_iter()
        .find(|iface| iface.name == interface_name)
        .ok_or_else(|| anyhow::anyhow!("Interface '{}' not found", interface_name))?;

    // Prefer IPv4
    for ip in &iface.ips {
        if ip.is_ipv4() {
            return Ok(ip.ip().to_string());
        }
    }
    // Fallback to IPv6
    for ip in &iface.ips {
        if ip.is_ipv6() {
            return Ok(ip.ip().to_string());
        }
    }
    anyhow::bail!("Interface '{}' has no IP address", interface_name)
}

/// Interactively select a network interface by listing all available ones.
fn interactive_select_interface() -> anyhow::Result<String> {
    use dialoguer::Select;
    use pnet::datalink;

    let interfaces = datalink::interfaces();
    let mut candidates: Vec<(String, String)> = Vec::new(); // (name, ip)

    for iface in &interfaces {
        if iface.is_loopback() {
            continue;
        }
        for ip in &iface.ips {
            if ip.is_ipv4() {
                candidates.push((iface.name.clone(), ip.ip().to_string()));
                break;
            }
        }
    }

    if candidates.is_empty() {
        anyhow::bail!("No network interfaces with IP addresses found");
    }

    let items: Vec<String> = candidates
        .iter()
        .map(|(name, ip)| format!("{} ({})", name, ip))
        .collect();

    let selection = Select::new()
        .with_prompt("Select network interface for share link host")
        .items(&items)
        .default(0)
        .interact()?;

    Ok(candidates[selection].1.clone())
}

// ── SNI Resolution ──

/// Resolve the SNI to use in the share link.
///
/// Priority:
/// 1. If `--domain` is specified, use it directly.
/// 2. Parse the TLS certificate and determine the best domain:
///    - Single non-wildcard domain → auto-use
///    - Wildcard-only → interactive prompt for specific domain
///    - Multiple domains → interactive selection
/// 3. If certificate parsing fails, fall back to `config.sni`.
fn resolve_export_sni(config: &Config, domain: Option<&str>) -> anyhow::Result<Option<String>> {
    // User explicitly specified a domain → use it directly
    if let Some(d) = domain {
        return Ok(Some(d.to_string()));
    }

    // No certificate path → fall back to config.sni
    if config.certificate.is_empty() {
        return Ok(if config.sni.is_empty() {
            None
        } else {
            Some(config.sni.clone())
        });
    }

    // Try to parse the certificate
    match cert::parse_cert_domains(&config.certificate) {
        Ok(domains) => {
            // Single non-wildcard domain → auto-use
            if let Some(preferred) = cert::pick_preferred_domain(&domains, None) {
                return Ok(Some(preferred));
            }

            // Wildcard-only → prompt user to enter specific domain
            if domains.is_wildcard && domains.sans.len() <= 1 {
                let wildcard_pattern = domains.sans.first().or(domains.cn.as_ref()).unwrap();
                eprintln!("Certificate is a wildcard ({})", wildcard_pattern);
                return Ok(Some(interactive_input_domain(wildcard_pattern)?));
            }

            // Multiple domains (including mixed wildcard + specific) → let user choose
            if !domains.sans.is_empty() {
                return Ok(Some(interactive_select_domain(&domains.sans)?));
            }

            // No SANs and CN is wildcard → shouldn't happen, but fallback
            if let Some(cn) = &domains.cn {
                return Ok(Some(cn.clone()));
            }

            Ok(None)
        }
        Err(_) => {
            // Certificate parsing failed → fall back to config.sni
            Ok(if config.sni.is_empty() {
                None
            } else {
                Some(config.sni.clone())
            })
        }
    }
}

/// Interactively prompt the user to enter a specific domain for a wildcard certificate.
fn interactive_input_domain(wildcard_pattern: &str) -> anyhow::Result<String> {
    use dialoguer::Input;

    let domain: String = Input::new()
        .with_prompt(format!(
            "Enter the specific domain for SNI (wildcard: {})",
            wildcard_pattern
        ))
        .interact_text()?;

    if domain.is_empty() {
        anyhow::bail!("Domain cannot be empty");
    }
    Ok(domain)
}

/// Interactively let the user select one domain from multiple certificate SANs.
fn interactive_select_domain(domains: &[String]) -> anyhow::Result<String> {
    use dialoguer::Select;

    let selection = Select::new()
        .with_prompt("Certificate contains multiple domains, select one for SNI")
        .items(domains)
        .default(0)
        .interact()?;

    Ok(domains[selection].clone())
}
