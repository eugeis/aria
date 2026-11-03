//! aria-ask — the ASK surface of the Aria voice assistant.
//!
//! Two jobs:
//!   serve  the Alexa Skills Kit endpoint (/alexa), the raw audio streaming
//!          (/stream/{token}, /art/{id}), per-device player state, and the
//!          brain bridge (the Aria Mosaic app over loopback REST).
//!   scan   walk the media collections, index tags (lofty), and publish the
//!          deterministic metadata JSONL (media.jsonl.gz) the brain ingests
//!          as its `media` knowledge collection — then ask the brain to
//!          reindex.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context as _;
use axum::Router;
use axum::routing::get;
use tracing_subscriber::EnvFilter;

mod agent;
mod config;
mod router;
mod scan;
mod state;
mod stream;

use agent::{BrainAgent, MockAgent};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (command, config_path) = split_args(&args)?;

    let cfg =
        config::load(config_path.as_deref()).map_err(|e| anyhow::anyhow!(e.to_exit_message()))?;

    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(cfg.logging.level.clone()));
    tracing_subscriber::fmt().with_env_filter(filter).init();

    match command {
        Command::Scan { reindex } => run_scan(&cfg, reindex).await,
        Command::Serve => run_serve(cfg).await,
    }
}

#[derive(Default)]
enum Command {
    #[default]
    Serve,
    Scan {
        reindex: bool,
    },
}

fn split_args(args: &[String]) -> anyhow::Result<(Command, Option<PathBuf>)> {
    let mut command = Command::Serve;
    let mut config_path = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "serve" => command = Command::Serve,
            "scan" => {
                command = Command::Scan { reindex: true };
                if i + 1 < args.len() && args[i + 1] == "--no-reindex" {
                    command = Command::Scan { reindex: false };
                    i += 1;
                }
            }
            "-c" | "--config" => {
                i += 1;
                config_path = args.get(i).map(PathBuf::from);
            }
            "-h" | "--help" => {
                println!(
                    "aria-ask — the ASK surface of the Aria voice assistant\n\n\
                     usage: aria-ask <COMMAND> [OPTIONS]\n\n\
                     commands:\n  serve              run the /alexa + /stream server (default)\n  \
                     scan [--no-reindex]  rescan the collections, publish media.jsonl.gz, ask the brain to reindex\n\n\
                     options:\n  -c, --config <PATH>   config file (default: ~/.aria/config.toml)\n  -h, --help            show help\n\n\
                     env:\n  ARIA_CONFIG         config file path\n  ARIA_DATA_DIR       state directory (default ~/.aria)\n  \
                     ARIA_BRAIN_TOKEN    brain service-account bearer\n  ARIA_BRAIN_URL      brain base url (default http://127.0.0.1:3000)\n  \
                     ARIA_PUBLIC_URL     public https base url for stream links\n  RUST_LOG            log filter"
                );
                std::process::exit(0);
            }
            other => {
                eprintln!("unknown argument: {other} (try --help)");
                std::process::exit(2);
            }
        }
        i += 1;
    }
    Ok((command, config_path))
}

/// One full scan + publish (+ optional brain reindex).
async fn run_scan(cfg: &config::Config, reindex: bool) -> anyhow::Result<()> {
    // Validate the collection kinds up front (fail fast, good message).
    cfg.collection_items().map_err(anyhow::Error::msg)?;
    let task_cfg = cfg.clone();
    let (library, records) = tokio::task::spawn_blocking(move || scan::scan_library(&task_cfg))
        .await
        .context("scan task panicked")?
        .map_err(anyhow::Error::msg)?;
    let changed = scan::publish(cfg, &records)?;
    println!(
        "scanned {} item(s) ({} records); media.jsonl.gz {}",
        library.len(),
        records.len(),
        if changed { "updated" } else { "unchanged" }
    );
    if changed && reindex {
        match scan::reindex(cfg).await {
            Ok(()) => println!("brain reindex requested"),
            Err(e) => {
                eprintln!(
                    "brain reindex failed: {e} (the brain picks up the scan on its next boot)"
                );
            }
        }
    }
    Ok(())
}

async fn run_serve(cfg: config::Config) -> anyhow::Result<()> {
    if cfg.server.public_base_url.trim().is_empty() && !cfg.brain.mock {
        tracing::warn!(
            "server.public_base_url is empty; Echo devices cannot reach /stream links. \
             Set it to your public https URL (e.g. https://aria.eisler-systems.de)."
        );
    }

    // Initial scan: index the library AND publish the metadata the brain
    // ingests (a fresh brain boot reindexes from the file; a running brain
    // is told via /api/ops/reindex when the file changed).
    let task_cfg = cfg.clone();
    let (library, records) = tokio::task::spawn_blocking(move || scan::scan_library(&task_cfg))
        .await
        .context("initial scan task panicked")?
        .map_err(anyhow::Error::msg)?;
    let changed = scan::publish(&cfg, &records).context("publish media.jsonl.gz")?;
    if changed && !cfg.brain.mock {
        match scan::reindex(&cfg).await {
            Ok(()) => tracing::info!("brain reindex requested after initial scan"),
            Err(e) => tracing::warn!(%e, "brain reindex after initial scan failed"),
        }
    }

    let agent: Arc<dyn agent::AgentBackend> = if cfg.brain.mock {
        tracing::info!("brain.mock = true: using mock brain (no REST calls)");
        Arc::new(MockAgent::new())
    } else {
        Arc::new(BrainAgent::new(&cfg))
    };

    let state = Arc::new(state::AppState::new(cfg.clone(), library, agent));

    // Periodic rescan: refresh the in-memory library, republish on change,
    // reindex the brain.
    if cfg.collections.rescan_secs > 0 {
        let st = state.clone();
        let secs = cfg.collections.rescan_secs;
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(tokio::time::Duration::from_secs(secs));
            ticker.tick().await; // first tick is immediate; skip it
            loop {
                ticker.tick().await;
                let cfg = st.cfg.clone();
                let (lib, records) =
                    match tokio::task::spawn_blocking(move || scan::scan_library(&cfg)).await {
                        Ok(Ok(x)) => x,
                        Ok(Err(e)) => {
                            tracing::warn!(%e, "rescan failed");
                            continue;
                        }
                        Err(_) => continue,
                    };
                match scan::publish(&st.cfg, &records) {
                    Ok(true) => {
                        if !st.cfg.brain.mock {
                            match scan::reindex(&st.cfg).await {
                                Ok(()) => tracing::info!(
                                    records = records.len(),
                                    "collections changed: published + reindexed"
                                ),
                                Err(e) => tracing::warn!(%e, "reindex after rescan failed"),
                            }
                        }
                    }
                    Ok(false) => {}
                    Err(e) => tracing::warn!(%e, "publish after rescan failed"),
                }
                let n = lib.len();
                *st.library.write().unwrap() = lib;
                tracing::info!(items = n, "collections rescanned");
            }
        });
    }

    // Housekeeping: expire stream tokens, persist state.
    {
        let st = state.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(tokio::time::Duration::from_secs(30));
            loop {
                ticker.tick().await;
                let removed = st.registry.expire_old();
                if removed > 0 {
                    tracing::debug!(removed, "expired stream tokens");
                }
                st.save_state();
            }
        });
    }

    let app = Router::new()
        .route("/alexa", axum::routing::post(stream::handle_alexa))
        .route("/stream/{token}", get(stream::handle_stream))
        .route("/art/{id}", get(stream::handle_art))
        .route("/health", get(stream::handle_health))
        .layer(tower_http::trace::TraceLayer::new_for_http())
        .with_state(state);

    let addr: SocketAddr = cfg
        .server
        .bind
        .parse()
        .with_context(|| format!("bad server.bind: {}", cfg.server.bind))?;

    let has_tls = cfg.server.tls_cert.is_some() && cfg.server.tls_key.is_some();
    println!("aria-ask starting");
    println!(
        "  listen       : {addr}{}",
        if has_tls { " (TLS)" } else { "" }
    );
    if !cfg.server.public_base_url.is_empty() {
        println!(
            "  skill endpoint : {}/alexa",
            cfg.server.public_base_url.trim_end_matches('/')
        );
        println!(
            "  stream endpoint: {}/stream/<token>",
            cfg.server.public_base_url.trim_end_matches('/')
        );
    }

    match (cfg.server.tls_cert.clone(), cfg.server.tls_key.clone()) {
        (Some(cert), Some(key)) => serve_tls(app, addr, cert, key).await,
        _ => serve_http(app, addr).await,
    }
}

async fn serve_http(app: Router, addr: SocketAddr) -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!(%addr, "http listener ready");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("server failed")?;
    Ok(())
}

async fn serve_tls(
    app: Router,
    addr: SocketAddr,
    cert_path: PathBuf,
    key_path: PathBuf,
) -> anyhow::Result<()> {
    let cert_data = tokio::fs::read(&cert_path).await?;
    let key_data = tokio::fs::read(&key_path).await?;
    let certs = rustls_pemfile::certs(&mut cert_data.as_slice())
        .collect::<Result<Vec<_>, _>>()
        .context("cannot parse certificate PEM")?;
    let key = rustls_pemfile::private_key(&mut key_data.as_slice())
        .context("cannot parse private key PEM")?
        .ok_or_else(|| anyhow::anyhow!("no private key found in {key_path:?}"))?;
    let server_cfg = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .context("cannot build rustls config")?;
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(server_cfg));
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!(%addr, "https listener ready");

    loop {
        tokio::select! {
            _ = shutdown_signal() => break,
            accepted = listener.accept() => {
                let (tcp, remote) = accepted?;
                let acceptor = acceptor.clone();
                let router = app.clone();
                tokio::spawn(async move {
                    let tls = match acceptor.accept(tcp).await {
                        Ok(t) => t,
                        Err(e) => {
                            tracing::debug!(%remote, %e, "tls handshake failed");
                            return;
                        }
                    };
                    // Bridge tower::Service (axum Router) to hyper's Service.
                    let svc = hyper::service::service_fn(move |req: http::Request<hyper::body::Incoming>| {
                        let mut router = router.clone();
                        async move {
                            match tower::ServiceExt::oneshot(&mut router, req).await {
                                // axum Router errors are Infallible.
                                Ok(res) => Ok::<_, std::convert::Infallible>(res),
                                Err(e) => {
                                    let _: std::convert::Infallible = e;
                                    unreachable!()
                                }
                            }
                        }
                    });
                    let io = hyper_util::rt::TokioIo::new(tls);
                    if let Err(e) = hyper_util::server::conn::auto::Builder::new(
                        hyper_util::rt::TokioExecutor::new(),
                    )
                    .serve_connection(io, svc)
                    .await
                    {
                        tracing::debug!(%remote, %e, "tls connection error");
                    }
                });
            }
        }
    }
    Ok(())
}

async fn shutdown_signal() {
    if let Err(e) = tokio::signal::ctrl_c().await {
        tracing::error!(%e, "cannot install ctrl-c handler");
        return;
    }
    tracing::info!("shutting down");
}
