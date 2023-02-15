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

    /// List of allowed backend server URL regular expressions
    ///
    /// Example: 'https://.*\.foo\.example\.com/.*'
    #[arg(long)]
    allowlist: Vec<String>,

    /// Like `allowlist`, but just the host suffixes as plain strings (not regular expressions)
    ///
    /// This list will be combined with `allowlist`, if present.
    ///
    /// Example: '.foo.example.com'
    #[arg(long)]
    host_suffix_allowlist: Vec<String>,

    /// TLS certificate to use
    #[arg(long, requires = "key")]
    cert: Option<PathBuf>,

    /// TLS key to use
    #[arg(long, requires = "cert")]
    key: Option<PathBuf>,

    /// If specified, send the backend hostname as a `ws-bridge-group` header along with the `ws-bridge-send`
    /// header and require that all /send/* requests echo it back.
    ///
    /// This is useful for grouping and rate limiting requests by hostname in a load-balancing proxy.
    #[arg(long)]
    group_by_host: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    pretty_env_logger::init_timed();

    let options = Options::parse();

    let allowlist = options
        .allowlist
        .into_iter()
        .chain(
            options
                .host_suffix_allowlist
                .iter()
                .map(|s| format!("https://.*{}/.*", regex::escape(s))),
        )
        .collect::<Vec<_>>();

    let app = websocket_bridge::router(Config {
        host_base_url: Arc::new(OnceCell::from(options.base_url)),
        allowlist: RegexSet::new(allowlist)?,
        group_by_host: options.group_by_host,
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
