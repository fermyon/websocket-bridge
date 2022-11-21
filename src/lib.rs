use {
    anyhow::{Context, Result},
    axum::{
        body::Body,
        extract::{
            ws::{Message, WebSocket, WebSocketUpgrade},
            BodyStream, Extension, Path,
        },
        response::IntoResponse,
        routing, Error, Router, TypedHeader,
    },
    dashmap::DashMap,
    futures::{stream::SplitSink, SinkExt, Stream, StreamExt, TryStreamExt},
    headers::ContentType,
    http::{HeaderMap, StatusCode},
    once_cell::sync::OnceCell,
    regex::RegexSet,
    reqwest::{Client, Response},
    std::sync::Arc,
    tokio::sync::Mutex as AsyncMutex,
    tower_http::trace::{DefaultMakeSpan, TraceLayer},
    url::Url,
    uuid::Uuid,
};

pub struct Config {
    pub host_base_url: Arc<OnceCell<Url>>,
    pub whitelist: RegexSet,
}

struct Urls {
    on_frame: Url,
    on_disconnect: Url,
}

type Sink = SplitSink<WebSocket, Message>;

struct State {
    config: Config,
    sinks: DashMap<Uuid, AsyncMutex<Sink>>,
    client: Client,
}

pub fn router(config: Config) -> Router<Body> {
    Router::new()
        .route("/connect", routing::get(on_connect))
        .route("/send/:id", routing::post(on_send))
        .layer(Extension(Arc::new(State {
            config,
            sinks: DashMap::new(),
            client: Client::new(),
        })))
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(DefaultMakeSpan::default().include_headers(true)),
        )
}

async fn concat(mut stream: BodyStream) -> Result<Vec<u8>, Error> {
    let mut vec = Vec::new();
    while let Some(bytes) = stream.try_next().await? {
        vec.extend(&bytes);
    }
    Ok(vec)
}

async fn on_send(
    Path(id): Path<Uuid>,
    body: BodyStream,
    TypedHeader(content_type): TypedHeader<ContentType>,
    Extension(state): Extension<Arc<State>>,
) -> impl IntoResponse {
    let body_error = |_| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "error reading body".to_owned(),
        )
    };

    state
        .sinks
        .get(&id)
        .ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                format!("unknown connection id: {id}"),
            )
        })?
        .lock()
        .await
        .send(if content_type == ContentType::text_utf8() {
            Message::Text(
                String::from_utf8(concat(body).await.map_err(body_error)?).map_err(|_| {
                    (
                        StatusCode::BAD_REQUEST,
                        "unable to parse body as UTF-8".to_owned(),
                    )
                })?,
            )
        } else {
            Message::Binary(concat(body).await.map_err(body_error)?)
        })
        .await
        .map_err(|e| {
            tracing::warn!("error sending to connection {id}: {e:?}");

            (
                StatusCode::NOT_FOUND,
                format!("connection {id} has been closed"),
            )
        })
}

fn get_header_url(headers: &HeaderMap, name: &str) -> Result<Url, (StatusCode, String)> {
    let missing_error = || {
        (
            StatusCode::BAD_REQUEST,
            format!(r#"missing required header: "{name}""#),
        )
    };

    let parse_error = || {
        (
            StatusCode::BAD_REQUEST,
            format!(r#"unable to parse "{name}" header as a URL"#),
        )
    };

    Url::parse(
        headers
            .get(name)
            .ok_or_else(missing_error)?
            .to_str()
            .map_err(|_| parse_error())?,
    )
    .map_err(|_| parse_error())
}

fn get_urls(whitelist: &RegexSet, headers: &HeaderMap) -> Result<Urls, (StatusCode, String)> {
    let get_header_url = |name| {
        let url = get_header_url(headers, name)?;

        if whitelist.is_match(url.as_ref()) {
            Ok(url)
        } else {
            Err((
                StatusCode::FORBIDDEN,
                format!(r#"access denied for URL specified by "{name}" header"#),
            ))
        }
    };

    Ok(Urls {
        on_frame: get_header_url("x-ws-proxy-on-frame")?,
        on_disconnect: get_header_url("x-ws-proxy-on-disconnect")?,
    })
}

async fn on_connect(
    ws: WebSocketUpgrade,
    headers: HeaderMap,
    Extension(state): Extension<Arc<State>>,
) -> impl IntoResponse {
    match get_urls(&state.config.whitelist, &headers) {
        Ok(urls) => ws.on_upgrade(move |ws| async move { serve(&state, &urls, ws).await }),
        Err(rejection) => rejection.into_response(),
    }
}

async fn serve(state: &State, urls: &Urls, ws: WebSocket) {
    let id = Uuid::new_v4();

    let (tx, rx) = ws.split();

    state.sinks.insert(id, AsyncMutex::new(tx));

    let send_url = format!("{}/send/{id}", state.config.host_base_url.get().unwrap());

    if let Err(e) = receive(state, &urls.on_frame, rx, &send_url).await {
        tracing::warn!("error serving connection {id}: {e:?}");
    }

    if let Err(e) = state
        .sinks
        .remove(&id)
        .unwrap()
        .1
        .lock()
        .await
        .close()
        .await
    {
        tracing::warn!("error closing connection {id}: {e:?}");
    }

    if let Err(e) = state
        .client
        .post(urls.on_disconnect.clone())
        .header("x-ws-proxy-send", send_url)
        .send()
        .await
        .and_then(Response::error_for_status)
    {
        tracing::warn!(
            "error posting to `on-disconnect` URL {} for {id}: {e:?}",
            urls.on_disconnect
        );
    }
}

async fn receive(
    state: &State,
    on_frame: &Url,
    mut rx: impl Stream<Item = Result<Message, Error>> + Unpin,
    send_url: &str,
) -> Result<()> {
    let post = || {
        state
            .client
            .post(on_frame.clone())
            .header("x-ws-proxy-send", send_url)
    };

    let context = || format!("posting to {on_frame}");

    while let Some(message) = rx.next().await {
        match message.context("receiving from client")? {
            Message::Text(text) => drop(
                post()
                    .header("content-type", "text/plain;charset=UTF-8")
                    .body(text)
                    .send()
                    .await
                    .and_then(Response::error_for_status)
                    .with_context(context)?,
            ),

            Message::Binary(bytes) => drop(
                post()
                    .header("content-type", "application/octet-stream")
                    .body(bytes)
                    .send()
                    .await
                    .and_then(Response::error_for_status)
                    .with_context(context)?,
            ),

            Message::Ping(_) | Message::Pong(_) => (),

            Message::Close(_) => break,
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        anyhow::{anyhow, bail},
        async_trait::async_trait,
        axum::{
            extract::{FromRequest, RequestParts},
            Server,
        },
        futures::channel::mpsc::{self, Sender},
        std::net::{Ipv4Addr, SocketAddr},
        tungstenite::{handshake::client, protocol::Message as TMessage},
    };

    #[derive(Debug)]
    enum Item {
        OnFrame {
            body: Vec<u8>,
            content_type: ContentType,
            send_url: Url,
        },
        OnDisconnect {
            send_url: Url,
        },
    }

    struct SendUrl(Url);

    #[async_trait]
    impl<B: Send> FromRequest<B> for SendUrl {
        type Rejection = (StatusCode, String);

        async fn from_request(request: &mut RequestParts<B>) -> Result<Self, Self::Rejection> {
            get_header_url(request.headers(), "x-ws-proxy-send").map(Self)
        }
    }

    fn backend_router(sender: Arc<AsyncMutex<Sender<Item>>>) -> Router<Body> {
        Router::new()
            .route("/frame", routing::post(on_frame))
            .route("/disconnect", routing::post(on_disconnect))
            .layer(Extension(sender))
            .layer(
                TraceLayer::new_for_http()
                    .make_span_with(DefaultMakeSpan::default().include_headers(true)),
            )
    }

    async fn on_frame(
        body: BodyStream,
        TypedHeader(content_type): TypedHeader<ContentType>,
        SendUrl(send_url): SendUrl,
        Extension(sender): Extension<Arc<AsyncMutex<Sender<Item>>>>,
    ) -> impl IntoResponse {
        let error = || StatusCode::INTERNAL_SERVER_ERROR;

        sender
            .lock()
            .await
            .send(Item::OnFrame {
                body: concat(body).await.map_err(|_| error())?,
                content_type,
                send_url,
            })
            .await
            .map_err(|_| error())
    }

    async fn on_disconnect(
        SendUrl(send_url): SendUrl,
        Extension(sender): Extension<Arc<AsyncMutex<Sender<Item>>>>,
    ) -> impl IntoResponse {
        sender
            .lock()
            .await
            .send(Item::OnDisconnect { send_url })
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
    }

    #[tokio::test]
    async fn integration() -> Result<()> {
        pretty_env_logger::init_timed();

        let bind_address = SocketAddr::from((Ipv4Addr::LOCALHOST, 0));

        let (tx, mut rx) = mpsc::channel(2);

        let backend = Server::try_bind(&bind_address)?
            .serve(backend_router(Arc::new(AsyncMutex::new(tx))).into_make_service());

        let backend_addr = backend.local_addr();

        tokio::spawn(backend);

        let host_base_url = Arc::new(OnceCell::new());

        let proxy = Server::try_bind(&bind_address)?.serve(
            router(Config {
                host_base_url: host_base_url.clone(),
                whitelist: RegexSet::new([&format!(
                    "http://{}/.*",
                    backend_addr.to_string().replace('.', "\\.")
                )])?,
            })
            .into_make_service(),
        );

        let proxy_addr = proxy.local_addr();

        host_base_url
            .set(format!("http://{proxy_addr}").parse()?)
            .map_err(|e| anyhow!("{e}"))?;

        tokio::spawn(proxy);

        let (mut socket, _response) = tokio_tungstenite::connect_async(
            client::Request::builder()
                .method("GET")
                .header("host", proxy_addr.ip().to_string())
                .header("connection", "Upgrade")
                .header("upgrade", "websocket")
                .header("sec-websocket-version", "13")
                .header("sec-websocket-key", client::generate_key())
                .header(
                    "x-ws-proxy-on-frame",
                    format!("http://{backend_addr}/frame"),
                )
                .header(
                    "x-ws-proxy-on-disconnect",
                    format!("http://{backend_addr}/disconnect"),
                )
                .uri(&format!("ws://{proxy_addr}/connect"))
                .body(())?,
        )
        .await?;

        socket.send(TMessage::text("hello")).await.unwrap();

        let my_send_url;

        match rx.next().await.context("unexpected end of stream")? {
            Item::OnFrame {
                body,
                content_type,
                send_url,
            } => {
                assert_eq!(content_type, ContentType::text_utf8());
                assert_eq!(&body, b"hello");
                assert!(send_url
                    .to_string()
                    .starts_with(&format!("http://{proxy_addr}/send")));

                Client::new()
                    .post(send_url.clone())
                    .header("content-type", "text/plain;charset=UTF-8")
                    .body("hola")
                    .send()
                    .await?
                    .error_for_status()?;

                my_send_url = send_url;
            }

            other => bail!("expected an `OnFrame` but got {other:?}"),
        }

        match socket.next().await.context("unexpected end of stream")?? {
            TMessage::Text(msg) => assert_eq!(msg, "hola"),
            other => bail!("expected a text message but got {other:?}"),
        }

        socket.close(None).await?;

        drop(socket);

        match rx.next().await.context("unexpected end of stream")? {
            Item::OnDisconnect { send_url } => {
                assert_eq!(my_send_url, send_url);
            }

            other => bail!("expected an `OnFrame` but got {other:?}"),
        }

        Ok(())
    }
}
