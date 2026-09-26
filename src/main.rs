use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use tracing::level_filters::LevelFilter;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_appender::rolling::{self, Rotation};
use tracing_subscriber::Layer;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

use marksync_server::config::{API_VERSION, Config, LogConfig};
use marksync_server::http::{self, App};

const PURGE_INTERVAL: Duration = Duration::from_secs(60 * 60);

const USAGE: &str = "\
Usage: marksync-server [COMMAND] [--config <settings.json>]

Commands:
  serve        Run the API service (default)
  healthcheck  Exit 0 if the local service reports status online (1) or no new syncs (3)

Options:
  -c, --config <path>  Settings file (default: $MARKSYNC_CONFIG or config/settings.json)
  -h, --help           Print help
  -V, --version        Print version";

enum Command {
    Serve,
    Healthcheck,
}

fn main() -> ExitCode {
    let mut command = Command::Serve;
    let mut config_path = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "serve" => command = Command::Serve,
            "healthcheck" => command = Command::Healthcheck,
            "-c" | "--config" => match args.next() {
                Some(path) => config_path = Some(PathBuf::from(path)),
                None => return usage_error("--config requires a path"),
            },
            "-h" | "--help" => {
                println!("{USAGE}");
                return ExitCode::SUCCESS;
            }
            "-V" | "--version" => {
                println!("marksync-server {} (xBrowserSync API {API_VERSION})", env!("CARGO_PKG_VERSION"));
                return ExitCode::SUCCESS;
            }
            other => return usage_error(&format!("unexpected argument {other:?}")),
        }
    }

    let config = match Config::load(config_path.as_deref()) {
        Ok(config) => config,
        Err(err) => {
            eprintln!("error: {err:#}");
            return ExitCode::FAILURE;
        }
    };

    match command {
        Command::Healthcheck => healthcheck(&config),
        Command::Serve => {
            let runtime = tokio::runtime::Runtime::new().expect("failed to start tokio runtime");
            match runtime.block_on(serve(config)) {
                Ok(()) => ExitCode::SUCCESS,
                Err(err) => {
                    tracing::error!(error = %format!("{err:#}"), "service failed");
                    eprintln!("error: {err:#}");
                    ExitCode::FAILURE
                }
            }
        }
    }
}

fn usage_error(message: &str) -> ExitCode {
    eprintln!("error: {message}\n\n{USAGE}");
    ExitCode::from(2)
}

async fn serve(config: Config) -> Result<()> {
    let _log_guard = init_logging(&config.log)?;
    // Several dependencies enable different rustls backends, so pick one explicitly
    // (used for HTTPS serving and TLS connections to MongoDB).
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    let addr = resolve_addr(&config.server.host, config.server.port)?;
    let https = config.server.https.clone();
    let base_path = config.base_path();

    let app = marksync_server::build_app(config).await?;
    spawn_purge_task(app.clone());
    let service = http::router(app).into_make_service_with_connect_info::<SocketAddr>();

    if https.enabled {
        serve_tls(addr, &https.cert_path, &https.key_path, &base_path, service).await
    } else {
        let listener =
            tokio::net::TcpListener::bind(addr).await.with_context(|| format!("unable to listen on {addr}"))?;
        tracing::info!("Service started at http://{addr}{base_path}");
        axum::serve(listener, service).with_graceful_shutdown(shutdown_signal()).await?;
        tracing::info!("Service shutting down");
        Ok(())
    }
}

#[cfg(feature = "tls")]
async fn serve_tls(
    addr: SocketAddr,
    cert_path: &str,
    key_path: &str,
    base_path: &str,
    service: axum::extract::connect_info::IntoMakeServiceWithConnectInfo<axum::Router, SocketAddr>,
) -> Result<()> {
    let tls = axum_server::tls_rustls::RustlsConfig::from_pem_file(cert_path, key_path)
        .await
        .context("unable to load TLS certificate or key")?;
    let handle = axum_server::Handle::new();
    let shutdown = handle.clone();
    tokio::spawn(async move {
        shutdown_signal().await;
        shutdown.graceful_shutdown(Some(Duration::from_secs(10)));
    });
    tracing::info!("Service started at https://{addr}{base_path}");
    axum_server::bind_rustls(addr, tls).handle(handle).serve(service).await?;
    tracing::info!("Service shutting down");
    Ok(())
}

#[cfg(not(feature = "tls"))]
async fn serve_tls(
    _: SocketAddr,
    _: &str,
    _: &str,
    _: &str,
    _: axum::extract::connect_info::IntoMakeServiceWithConnectInfo<axum::Router, SocketAddr>,
) -> Result<()> {
    bail!("server.https.enabled is set but this build has no TLS support (enable the `tls` feature)")
}

fn resolve_addr(host: &str, port: u16) -> Result<SocketAddr> {
    (host, port)
        .to_socket_addrs()
        .with_context(|| format!("unable to resolve server.host {host:?}"))?
        .next()
        .with_context(|| format!("server.host {host:?} resolved to no addresses"))
}

fn spawn_purge_task(app: Arc<App>) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(PURGE_INTERVAL);
        loop {
            interval.tick().await;
            match app.service.purge().await {
                Ok((0, 0)) => {}
                Ok((syncs, logs)) => tracing::info!(syncs, logs, "purged expired data"),
                Err(err) => tracing::error!(error = %format!("{err:#}"), "unable to purge expired data"),
            }
        }
    });
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        use tokio::signal::unix::{SignalKind, signal};
        let mut term = signal(SignalKind::terminate()).expect("failed to install SIGTERM handler");
        let mut usr1 = signal(SignalKind::user_defined1()).expect("failed to install SIGUSR1 handler");
        let mut usr2 = signal(SignalKind::user_defined2()).expect("failed to install SIGUSR2 handler");
        tokio::select! {
            _ = term.recv() => {}
            _ = usr1.recv() => {}
            _ = usr2.recv() => {}
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
    }
    tracing::info!("Process terminated by signal");
}

fn level(name: &str) -> LevelFilter {
    match name.to_ascii_lowercase().as_str() {
        "trace" => LevelFilter::TRACE,
        "debug" => LevelFilter::DEBUG,
        "warn" => LevelFilter::WARN,
        "error" | "fatal" => LevelFilter::ERROR,
        _ => LevelFilter::INFO,
    }
}

fn init_logging(config: &LogConfig) -> Result<Option<WorkerGuard>> {
    let stdout = config.stdout.enabled.then(|| {
        tracing_subscriber::fmt::layer()
            .with_target(false)
            .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stdout()))
            .with_filter(level(&config.stdout.level))
    });

    let mut guard = None;
    let file = if config.file.enabled {
        let path = PathBuf::from(&config.file.path);
        let dir = path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(std::path::Path::new("."));
        let name = path.file_name().context("log.file.path has no file name")?.to_string_lossy().into_owned();
        std::fs::create_dir_all(dir).with_context(|| format!("unable to create log directory {}", dir.display()))?;
        let rotation = match config.file.rotation_period.chars().last() {
            Some('h') => Rotation::HOURLY,
            _ => Rotation::DAILY,
        };
        let appender = rolling::Builder::new()
            .rotation(rotation)
            .filename_prefix(name)
            .max_log_files(config.file.rotated_files_to_keep.max(1))
            .build(dir)
            .context("unable to open log file")?;
        let (writer, worker) = tracing_appender::non_blocking(appender);
        guard = Some(worker);
        Some(tracing_subscriber::fmt::layer().json().with_writer(writer).with_filter(level(&config.file.level)))
    } else {
        None
    };

    tracing_subscriber::registry().with(stdout).with(file).try_init().ok();
    Ok(guard)
}

/// Probes `/info` on the local service, for container health checks (no shell or curl
/// needed in the image).
fn healthcheck(config: &Config) -> ExitCode {
    match probe(config) {
        Ok(()) => {
            println!("HEALTHCHECK: online");
            ExitCode::SUCCESS
        }
        Err(err) => {
            println!("HEALTHCHECK: offline ({err:#})");
            ExitCode::FAILURE
        }
    }
}

fn probe(config: &Config) -> Result<()> {
    let host = match config.server.host.as_str() {
        "0.0.0.0" | "::" | "[::]" | "" => "127.0.0.1",
        host => host,
    };
    let addr = resolve_addr(host, config.server.port)?;
    let timeout = Duration::from_secs(2);
    let mut stream = TcpStream::connect_timeout(&addr, timeout)?;
    if config.server.https.enabled {
        // Listening is the best we can check without a TLS client.
        return Ok(());
    }
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    write!(
        stream,
        "GET {}info HTTP/1.1\r\nHost: {host}\r\nAccept: application/json\r\nConnection: close\r\n\r\n",
        config.base_path()
    )?;
    let mut response = String::new();
    stream.read_to_string(&mut response)?;
    let (head, body) = response.split_once("\r\n\r\n").context("malformed HTTP response")?;
    if !head.starts_with("HTTP/1.1 200") {
        bail!("unexpected response {}", head.lines().next().unwrap_or_default());
    }
    let info: serde_json::Value = serde_json::from_str(body.trim()).context("invalid /info response")?;
    match info.get("status").and_then(serde_json::Value::as_u64) {
        Some(1 | 3) => Ok(()),
        status => bail!("service status {status:?}"),
    }
}
