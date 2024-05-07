use async_channel::{Receiver, Sender};
use axum::{
    extract::{
        ws::{Message, WebSocket},
        ConnectInfo, State, WebSocketUpgrade,
    },
    response::IntoResponse,
};
use futures_util::{
    stream::{SplitSink, SplitStream},
    SinkExt, StreamExt,
};
use std::net::SocketAddr;
use tower_http::trace::{DefaultMakeSpan, TraceLayer};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();

    let port = 8080;
    let (from_ws_tx, from_ws_rx) = async_channel::unbounded();
    let (to_ws_tx, to_ws_rx) = async_channel::unbounded();

    tokio::select! {
        web_app_result = serve_web_app(port, from_ws_tx, to_ws_rx) => { web_app_result? },
        handler_result = handle_messages(from_ws_rx, to_ws_tx) => { handler_result? },
    }

    Ok(())
}

#[derive(Clone)]
struct AppState {
    to_ws_handler: Receiver<Vec<u8>>,
    from_ws_handler: Sender<Vec<u8>>,
}

async fn serve_web_app(
    port: u16,
    from_ws_tx: Sender<Vec<u8>>,
    to_ws_rx: Receiver<Vec<u8>>,
) -> anyhow::Result<()> {
    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    let listener = tokio::net::TcpListener::bind(addr).await?;
    let app_state = AppState {
        to_ws_handler: to_ws_rx,
        from_ws_handler: from_ws_tx,
    };

    tracing::info!("Listening on {}", listener.local_addr()?);
    let app = axum::Router::new()
        .route("/health_check", axum::routing::get(handle_health_check))
        .route("/ws", axum::routing::get(handle_ws_connection))
        .with_state(app_state)
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(DefaultMakeSpan::default().include_headers(true)),
        );
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await?;

    Ok(())
}

async fn handle_health_check(ConnectInfo(addr): ConnectInfo<SocketAddr>) -> impl IntoResponse {
    tracing::info!("Received health check from address: {addr}");
    ()
}

async fn handle_ws_connection(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
) -> impl IntoResponse {
    tracing::info!("Client at {addr} requested a WebSocket connection");

    let AppState {
        to_ws_handler: to_ws,
        from_ws_handler: from_ws,
    } = state;

    ws.on_failed_upgrade(|err| tracing::error!("Failed to upgrade connection: {err}"))
        .on_upgrade(move |socket| socket_task(socket, from_ws, to_ws))
}

async fn socket_task(ws: WebSocket, from_ws_tx: Sender<Vec<u8>>, to_ws_rx: Receiver<Vec<u8>>) {
    let (sender, receiver) = ws.split();

    tokio::select! {
        _ = handle_incoming_ws_message(receiver, from_ws_tx) => {},
        _ = handle_outgoing_payloads(sender, to_ws_rx) => {},
    };
}

async fn handle_outgoing_payloads(
    mut sender: SplitSink<WebSocket, Message>,
    to_ws_rx: Receiver<Vec<u8>>,
) {
    while let Ok(msg) = to_ws_rx.recv().await {
        if let Err(err) = sender.send(Message::Binary(msg)).await {
            tracing::error!("Unable to send message to web client: {err}");
            break;
        }
    }
}

async fn handle_incoming_ws_message(
    mut receiver: SplitStream<WebSocket>,
    from_ws_tx: Sender<Vec<u8>>,
) {
    while let Some(msg) = receiver.next().await {
        match msg {
            Ok(Message::Binary(msg)) => {
                tracing::debug!("Received {} bytes from client", msg.len());
                if let Err(_) = from_ws_tx.send(msg).await {
                    tracing::warn!("Closing WebSocket connection from the server side");
                    break;
                }
            }
            Ok(Message::Text(_)) => {
                tracing::warn!("Received unexpected text from WebSocket client");
            }
            Ok(Message::Ping(_) | Message::Pong(_)) => {}
            Ok(Message::Close(_)) => {
                tracing::info!("WebSocket client closed the connection");
                break;
            }
            Err(err) => {
                tracing::error!("Communication with WebSocket client interrupted: {err}");
                break;
            }
        }
    }
}

async fn handle_messages(from_ws: Receiver<Vec<u8>>, to_ws: Sender<Vec<u8>>) -> anyhow::Result<()> {
    while let Ok(msg) = from_ws.recv().await {
        if let Ok(msg) = std::str::from_utf8(&msg[..]) {
            tracing::debug!("Received message: {msg}");
            let response = msg.to_uppercase();
            if let Err(_) = to_ws.send(response.into_bytes()).await {
                tracing::error!("Channel disconnected; exiting");
                break;
            }
        }
    }

    Ok(())
}
