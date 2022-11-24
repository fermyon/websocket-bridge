use {
    anyhow::Result,
    axum_server::tls_rustls::RustlsConfig,
    clap::Parser,
    once_cell::sync::OnceCell,
    regex::RegexSet,
    std::{net::SocketAddr, path::PathBuf, sync::Arc},
    url::Url,
    websocket_bridge::{self, Config},
};

#[derive(Parser)]
#[command(author, version, about)]
struct Options {
    /// Address to accept incoming requests on
    #[arg(long)]
    address: SocketAddr,

    /// Base URL to send to backend servers for calling back to this server
    #[arg(long)]
    base_url: Url,

    /// allowlist of allowed backend server URLs
    #[arg(long)]
    allowlist: Vec<String>,

    /// TLS certificate to use
    #[arg(long, requires = "key")]
    cert: Option<PathBuf>,

    /// TLS key to use
    #[arg(long, requires = "cert")]
    key: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> Result<()> {
    pretty_env_logger::init_timed();

    let options = Options::parse();

    let app = websocket_bridge::router(Config {
        host_base_url: Arc::new(OnceCell::from(options.base_url)),
        allowlist: RegexSet::new(options.allowlist)?,
    })
    .into_make_service();

    if let (Some(cert), Some(key)) = (&options.cert, &options.key) {
        axum_server::bind_rustls(
            options.address,
            RustlsConfig::from_pem_file(cert, key).await?,
        )
        .serve(app)
        .await
    } else {
        axum_server::bind(options.address).serve(app).await
    }?;

    Ok(())
}
