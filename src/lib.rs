#![deny(warnings)]
use {
    anyhow::{Context, Result},
    axum::{
        extract::{
            ws::{Message, WebSocket, WebSocketUpgrade},
            BodyStream, Path, Query, State,
        },
        response::IntoResponse,
        routing, Error, Router, TypedHeader,
    },
    dashmap::DashMap,
    futures::{future, stream::SplitSink, SinkExt, Stream, StreamExt, TryStreamExt},
    headers::ContentType,
    http::{header::HeaderName, HeaderMap, HeaderValue, Method, StatusCode},
    once_cell::sync::OnceCell,
    regex::RegexSet,
    reqwest::{Client, RequestBuilder, Response},
    serde::Deserialize,
    std::sync::Arc,
    tokio::sync::Mutex as AsyncMutex,
    tower_http::{
        cors::CorsLayer,
        trace::{DefaultMakeSpan, TraceLayer},
    },
    url::Url,
    uuid::Uuid,
};

pub struct Config {
    pub host_base_url: Arc<OnceCell<Url>>,
    pub allowlist: RegexSet,
    pub group_by_host: bool,
}

struct Urls {
    on_frame: Url,
    on_disconnect: Url,
}

struct Sink {
    tx: AsyncMutex<SplitSink<WebSocket, Message>>,
    group: Option<String>,
}

struct MyState {
    config: Config,
    sinks: DashMap<Uuid, Sink>,
    client: Client,
}

#[derive(Deserialize)]
struct ConnectQuery {
    #[serde(rename = "f")]
    on_frame: Option<String>,

    #[serde(rename = "d")]
    on_disconnect: Option<String>,
}

pub fn router(config: Config) -> Router {
    Router::new()
        .route("/connect", routing::get(on_connect))
        .route("/send/:id", routing::post(on_send))
        .route("/healthz", routing::get(|| future::ready(StatusCode::OK)))
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(DefaultMakeSpan::default().include_headers(true)),
        )
        .layer(
            CorsLayer::new()
                .allow_origin("*".parse::<HeaderValue>().unwrap())
                .allow_methods([Method::GET, Method::POST]),
        )
        .with_state(Arc::new(MyState {
            config,
            sinks: DashMap::new(),
            client: Client::new(),
        }))
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
    TypedHeader(content_type): TypedHeader<ContentType>,
    headers: HeaderMap,
    State(state): State<Arc<MyState>>,
    body: BodyStream,
) -> impl IntoResponse {
    let body_error = |_| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "error reading body".to_owned(),
        )
    };

    let sink = state.sinks.get(&id).ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            format!("unknown connection id: {id}"),
        )
    })?;

    if let Some(group) = sink.group.as_deref() {
        if group != get_header(&headers, "ws-bridge-group")? {
            return Err((
                StatusCode::BAD_REQUEST,
                r#"incorrect "ws-bridge-group" header"#.to_owned(),
            ));
        }
    }

    sink.tx
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
        })?;

    Ok::<_, (StatusCode, String)>(StatusCode::OK)
}

fn get_optional_header<'a>(
    headers: &'a HeaderMap,
    name: &str,
) -> Result<Option<&'a str>, (StatusCode, String)> {
    headers
        .get(name)
        .map(|v| {
            v.to_str().map_err(|_| {
                (
                    StatusCode::BAD_REQUEST,
                    format!(r#"unable to parse "{name}" header as UTF-8"#),
                )
            })
        })
        .transpose()
}

fn get_header<'a>(headers: &'a HeaderMap, name: &str) -> Result<&'a str, (StatusCode, String)> {
    get_optional_header(headers, name)?.ok_or_else(|| {
        (
            StatusCode::BAD_REQUEST,
            format!(r#"missing required header: "{name}""#),
        )
    })
}

fn parse_error(name: &str) -> (StatusCode, String) {
    (
        StatusCode::BAD_REQUEST,
        format!(r#"unable to parse "{name}" header as a URL"#),
    )
}

fn get_header_url(headers: &HeaderMap, name: &str) -> Result<Url, (StatusCode, String)> {
    Url::parse(get_header(headers, name)?).map_err(|_| parse_error(name))
}

fn get_urls(
    allowlist: &RegexSet,
    query: &ConnectQuery,
    headers: &HeaderMap,
) -> Result<Urls, (StatusCode, String)> {
    let get_header_url = |param, name| {
        let url = if let Some(param) = param {
            Url::parse(param).map_err(|_| parse_error(name))
        } else {
            get_header_url(headers, name)
        }?;

        if allowlist.is_match(url.as_ref()) {
            Ok(url)
        } else {
            Err((
                StatusCode::FORBIDDEN,
                format!(r#"access denied for URL specified by "{name}" header"#),
            ))
        }
    };

    Ok(Urls {
        on_frame: get_header_url(query.on_frame.as_deref(), "ws-bridge-on-frame")?,
        on_disconnect: get_header_url(query.on_disconnect.as_deref(), "ws-bridge-on-disconnect")?,
    })
}

async fn on_connect(
    ws: WebSocketUpgrade,
    headers: HeaderMap,
    Query(query): Query<ConnectQuery>,
    State(state): State<Arc<MyState>>,
) -> impl IntoResponse {
    match get_urls(&state.config.allowlist, &query, &headers) {
        Ok(urls) => ws.on_upgrade(move |ws| async move { serve(&state, &urls, ws).await }),
        Err(rejection) => rejection.into_response(),
    }
}

async fn serve(state: &MyState, urls: &Urls, ws: WebSocket) {
    let id = Uuid::new_v4();

    let (tx, rx) = ws.split();

    let group = if state.config.group_by_host {
        urls.on_frame.host_str()
    } else {
        None
    };

    state.sinks.insert(
        id,
        Sink {
            tx: AsyncMutex::new(tx),
            group: group.map(str::to_owned),
        },
    );

    let send_url = format!("{}send/{id}", state.config.host_base_url.get().unwrap());

    if let Err(e) = receive(state, &urls.on_frame, rx, &send_url, group).await {
        tracing::warn!("error serving connection {id}: {e:?}");
    }

    if let Err(e) = state
        .sinks
        .remove(&id)
        .unwrap()
        .1
        .tx
        .into_inner()
        .close()
        .await
    {
        tracing::warn!("error closing connection {id}: {e:?}");
    }

    if let Err(e) = state
        .client
        .post(urls.on_disconnect.clone())
        .header("ws-bridge-send", send_url)
        .maybe_header("ws-bridge-group", group)
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
    state: &MyState,
    on_frame: &Url,
    mut rx: impl Stream<Item = Result<Message, Error>> + Unpin,
    send_url: &str,
    group: Option<&str>,
) -> Result<()> {
    let post = || {
        state
            .client
            .post(on_frame.clone())
            .header("ws-bridge-send", send_url)
            .maybe_header("ws-bridge-group", group)
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

trait MaybeHeader {
    fn maybe_header<K, V>(self, key: K, value: Option<V>) -> Self
    where
        HeaderName: TryFrom<K>,
        <HeaderName as TryFrom<K>>::Error: Into<http::Error>,
        HeaderValue: TryFrom<V>,
        <HeaderValue as TryFrom<V>>::Error: Into<http::Error>;
}

impl MaybeHeader for RequestBuilder {
    fn maybe_header<K, V>(self, key: K, value: Option<V>) -> Self
    where
        HeaderName: TryFrom<K>,
        <HeaderName as TryFrom<K>>::Error: Into<http::Error>,
        HeaderValue: TryFrom<V>,
        <HeaderValue as TryFrom<V>>::Error: Into<http::Error>,
    {
        if let Some(value) = value {
            self.header(key, value)
        } else {
            self
        }
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        anyhow::{anyhow, bail},
        async_trait::async_trait,
        axum::{extract::FromRequestParts, Server},
        futures::channel::mpsc::{self, Sender},
        http::request::Parts,
        std::net::{Ipv4Addr, SocketAddr},
        tungstenite::protocol::Message as TMessage,
    };

    #[derive(Debug)]
    enum Item {
        OnFrame {
            body: Vec<u8>,
            content_type: ContentType,
            send_info: SendInfo,
        },
        OnDisconnect {
            send_info: SendInfo,
        },
    }

    #[derive(Debug, PartialEq)]
    struct SendInfo {
        url: Url,
        group: Option<String>,
    }

    #[async_trait]
    impl<S: Sync> FromRequestParts<S> for SendInfo {
        type Rejection = (StatusCode, String);

        async fn from_request_parts(
            request: &mut Parts,
            _state: &S,
        ) -> Result<Self, Self::Rejection> {
            Ok(Self {
                url: get_header_url(&request.headers, "ws-bridge-send")?,
                group: get_optional_header(&request.headers, "ws-bridge-group")?.map(str::to_owned),
            })
        }
    }

    fn backend_router(sender: Arc<AsyncMutex<Sender<Item>>>) -> Router {
        Router::new()
            .route("/frame", routing::post(on_frame))
            .route("/disconnect", routing::post(on_disconnect))
            .layer(
                TraceLayer::new_for_http()
                    .make_span_with(DefaultMakeSpan::default().include_headers(true)),
            )
            .with_state(sender)
    }

    async fn on_frame(
        TypedHeader(content_type): TypedHeader<ContentType>,
        send_info: SendInfo,
        State(sender): State<Arc<AsyncMutex<Sender<Item>>>>,
        body: BodyStream,
    ) -> impl IntoResponse {
        let error = || StatusCode::INTERNAL_SERVER_ERROR;

        sender
            .lock()
            .await
            .send(Item::OnFrame {
                body: concat(body).await.map_err(|_| error())?,
                content_type,
                send_info,
            })
            .await
            .map_err(|_| error())
    }

    async fn on_disconnect(
        send_info: SendInfo,
        State(sender): State<Arc<AsyncMutex<Sender<Item>>>>,
    ) -> impl IntoResponse {
        sender
            .lock()
            .await
            .send(Item::OnDisconnect { send_info })
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
                allowlist: RegexSet::new([&format!(
                    "http://{}/.*",
                    regex::escape(&backend_addr.to_string())
                )])?,
                group_by_host: true,
            })
            .into_make_service(),
        );

        let proxy_addr = proxy.local_addr();

        host_base_url
            .set(format!("http://{proxy_addr}").parse()?)
            .map_err(|e| anyhow!("{e}"))?;

        tokio::spawn(proxy);

        let (mut socket, _response) = tokio_tungstenite::connect_async(
            format!("ws://{proxy_addr}/connect?f=http://{backend_addr}/frame&d=http://{backend_addr}/disconnect")
        )
        .await?;

        socket.send(TMessage::text("hello")).await.unwrap();

        let my_send_info;

        match rx.next().await.context("unexpected end of stream")? {
            Item::OnFrame {
                body,
                content_type,
                send_info,
            } => {
                assert_eq!(content_type, ContentType::text_utf8());
                assert_eq!(&body, b"hello");
                assert!(send_info
                    .url
                    .to_string()
                    .starts_with(&format!("http://{proxy_addr}/send")));
                assert_eq!(send_info.group, Some(proxy_addr.ip().to_string()));

                Client::new()
                    .post(send_info.url.clone())
                    .header("content-type", "text/plain;charset=UTF-8")
                    .maybe_header("ws-bridge-group", send_info.group.as_deref())
                    .body("hola")
                    .send()
                    .await?
                    .error_for_status()?;

                my_send_info = send_info;
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
            Item::OnDisconnect { send_info } => assert_eq!(my_send_info, send_info),
            other => bail!("expected an `OnDisconnect` but got {other:?}"),
        }

        Ok(())
    }
}
