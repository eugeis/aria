use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use anyhow::Context as _;
use axum::Router;
use axum::routing::get;
use eugeis_audio::Library;
use tracing_subscriber::EnvFilter;

mod agent;
mod config;
mod router;
mod state;
mod stream;

use agent::{AgentBackend, MockAgent, ZeroClawAgent};
use eugeis_zeroclaw::AgentEvent;
use state::PendingApproval;

fn main() -> anyhow::Result<()> {
    let config_path = parse_args();
    let cfg =
        config::load(config_path.as_deref()).map_err(|e| anyhow::anyhow!(e.to_exit_message()))?;

    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(cfg.logging.level.clone()));
    tracing_subscriber::fmt().with_env_filter(filter).init();

    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(run(cfg))
}

fn parse_args() -> Option<PathBuf> {
    let mut args = std::env::args().skip(1);
    let mut config_path = None;
    while let Some(a) = args.next() {
        match a.as_str() {
            "-c" | "--config" => config_path = args.next().map(PathBuf::from),
            "-h" | "--help" => {
                println!(
                    "eugeis — Alexa voice front-end for ZeroClaw\n\nusage: eugeis [OPTIONS]\n\noptions:\n  -c, --config <PATH>   config file (default: ~/.eugeis/config.toml)\n  -h, --help            show help\n\nenv:\n  EUGEIS_CONFIG         config file path\n  EUGEIS_ZC_TOKEN       ZeroClaw gateway bearer token\n  EUGEIS_GATEWAY        gateway base url (ws://host:port)\n  EUGEIS_PUBLIC_URL     public https base url for stream links\n  RUST_LOG              log filter"
                );
                std::process::exit(0);
            }
            other => {
                eprintln!("unknown argument: {other} (try --help)");
                std::process::exit(2);
            }
        }
    }
    config_path
}

async fn run(cfg: config::Config) -> anyhow::Result<()> {
    if cfg.server.public_base_url.trim().is_empty() && !cfg.zeroclaw.mock {
        tracing::warn!(
            "server.public_base_url is empty; Echo devices cannot reach /stream links. \
             Set it to your public https URL (e.g. https://eugeis.example.com)."
        );
    }

    // Library scan (blocking; can be large).
    let paths = cfg.library.paths.clone();
    let library = tokio::task::spawn_blocking(move || Library::scan(&paths))
        .await
        .context("library scan task panicked")?
        .context("library scan failed")?;

    let agent: Arc<dyn AgentBackend> = if cfg.zeroclaw.mock {
        tracing::info!("zeroclaw.mock = true: using mock agent (no gateway connection)");
        Arc::new(MockAgent::new())
    } else {
        Arc::new(ZeroClawAgent::new(&cfg))
    };

    let state = Arc::new(state::AppState::new(cfg.clone(), library, agent.clone()));

    // Fold gateway events into app state.
    {
        let st = state.clone();
        let backend = st.agent.clone();
        agent::spawn_event_collector(backend.as_ref(), move |ev| match ev {
            AgentEvent::ApprovalRequested { device, info } => {
                tracing::info!(device = %device, tool = %info.tool, "approval requested");
                st.set_pending_approval(
                    &device,
                    Some(PendingApproval {
                        info,
                        asked_at: Instant::now(),
                    }),
                );
            }
            AgentEvent::LateReply { device, text } => {
                tracing::info!(device = %device, "cached late agent reply");
                st.set_cached_reply(&device, text);
            }
            AgentEvent::SessionError { device, message } => {
                tracing::warn!(device = %device, %message, "agent session error");
            }
        });
    }

    // Periodic library rescan.
    if cfg.library.rescan_secs > 0 {
        let st = state.clone();
        let paths = cfg.library.paths.clone();
        let secs = cfg.library.rescan_secs;
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(tokio::time::Duration::from_secs(secs));
            ticker.tick().await; // first tick is immediate; skip it
            loop {
                ticker.tick().await;
                let paths = paths.clone();
                let lib = match tokio::task::spawn_blocking(move || Library::scan(&paths)).await {
                    Ok(Ok(lib)) => lib,
                    Ok(Err(e)) => {
                        tracing::warn!(%e, "library rescan failed");
                        continue;
                    }
                    Err(_) => continue,
                };
                let n = lib.len();
                *st.library.write().unwrap() = lib;
                tracing::info!(tracks = n, "library rescanned");
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
    println!("eugeis starting");
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
