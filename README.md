# websocket-bridge

This is a simple reverse proxy server which accepts WebSocket connections and
forwards any incoming frames to backend HTTP server(s) as POST requests.  Those
POST requests will include a header with a "callback" URL which may be used to
send frames back to the client (again via POST requests).  The "callback" URL
may also be shared with other clients and servers, allowing them to send frames
directly to the original client for as long as the WebSocket exists.

```
[client] <---WebSocket--> [websocket-bridge] <---HTTP POST requests---> [backend HTTP server]
```

## Building and Running

### Prerequisites

- Rust
- (Optional, for testing) [websocat](https://crates.io/crates/websocat)

You can build and run in one step using `cargo`.  The `--base-url` argument
should match the publicly-visible hostname for your server, and will be used to
construct "callback" URLs.  The `--cert` and `--key` arguments are optional if
you'd like to use TLS.

The `--allowlist` option(s) (which may be specified multiple times) control
which backend HTTP servers may be proxied to.  Note that the `--allowlist`
option takes a regular expression, so be sure to escape any dots in the name
(e.g. `foo.example.com` -> `foo\.example\.com`).  If you don't specify a
allowlist, the server will reject all incoming traffic.

```
cargo run -- \
    --address 0.0.0.0:9443 \
    --base-url https://$YOUR_WEBSOCKET_BRIDGE_SERVER:9443 \
    --cert $PATH_TO_YOUR_WEBSOCKET_BRIDGE_TLS_CERT \
    --key $PATH_TO_YOUR_WEBSOCKET_BRIDGE_TLS_CERT \
    --allowlist 'https://$YOUR_BACKEND_SERVER_WITH_DOTS_ESCAPED/.*'
```

Then, assuming your backend server is up and accepting POST requests for
`/frame` and `/disconnect`, you can start sending frames using e.g.


```
websocat wss://$YOUR_WEBSOCKET_BRIDGE_SERVER:9443/connect?f=https://$YOUR_BACKEND_SERVER/frame&d=https://$YOUR_BACKEND_SERVER/disconnect
```

Any frames you send will be posted to `https://$YOUR_BACKEND_SERVER/frame`, and
`websocket-bridge` will send a final POST to
`https://$YOUR_BACKEND_SERVER/disconnect` when the WebSocket connection is
closed.  In both cases, the POST requests will contain a `x-ws-proxy-send`
header that specifies the "callback" URL -- e.g.
`https://$YOUR_WEBSOCKET_BRIDGE_SERVER:9443/[some random UUID]`, where `[some
random UUID]` is a UUID which uniquely identifies the connection.
