// src/lib.rs

//! A developer-friendly, non-blocking real-time WebSocket SDK for Rust.
//!
//! This library provides a simple way to connect to a WebSocket server and
//! send/receive messages asynchronously without blocking the calling thread.

use tokio::net::TcpStream;
use tokio_tungstenite::{
    connect_async,
    tungstenite::{
        protocol::{frame::coding::CloseCode, CloseFrame, Message},
        error::Error as TungsteniteError,
    },
    WebSocketStream,
};
use futures_util::{
    stream::{SplitSink, SplitStream},
    SinkExt, StreamExt,
};
use tokio::sync::mpsc;
use url::Url;
use thiserror::Error;
use tracing::{info, error, instrument};

/// A custom Result type for the SDK.
pub type Result<T> = std::result::Result<T, RealtimeError>;

/// Defines the custom error types for the SDK.
#[derive(Error, Debug)]
pub enum RealtimeError {
    #[error("WebSocket connection failed: {0}")]
    Connection(#[from] TungsteniteError),

    #[error("Message serialization/deserialization error: {0}")]
    Codec(String),

    #[error("Failed to parse URL: {0}")]
    UrlParse(#[from] url::ParseError),

    #[error("Failed to send message over channel: {0}")]
    SendChannel(String),

    #[error("Client is disconnected")]
    Disconnected,

    #[error("Protocol error: {0}")]
    Protocol(String),

    #[error("An internal error occurred: {0}")]
    Internal(String),
}

/// The main client for sending and receiving real-time data.
///
/// This struct holds the sending half of the WebSocket stream and the
/// receiver for incoming messages.
pub struct RealtimeClient {
    // Used for sending messages asynchronously.
    sender: mpsc::Sender<Message>,
    // Used by the developer to receive messages.
    receiver: mpsc::Receiver<String>,
}

// Internal type aliases for clarity
type WsStream = WebSocketStream<tokio_tungstenite::MaybeTlsStream<TcpStream>>;
type WsSink = SplitSink<WsStream, Message>;
type WsStreamReader = SplitStream<WsStream>;

impl RealtimeClient {
    /// Establishes an asynchronous connection to the WebSocket server.
    ///
    /// This is the primary entry point for a developer. It handles the full
    /// connection handshake, stream splitting, and spawns the background task
    /// for message reception.
    ///
    /// # Arguments
    /// * `url`: The WebSocket URL (e.g., "ws://127.0.0.1:8080" or "wss://...").
    ///
    /// # Returns
    /// A tuple containing the `RealtimeClient` instance and a `mpsc::Receiver<String>`
    /// that the developer can use to consume incoming messages.
    #[instrument(skip(self), err)]
    pub async fn connect(url: &str) -> Result<Self> {
        let url = Url::parse(url)?;
        
        info!("Attempting to connect to {}", url);
        let (ws_stream, _) = connect_async(url)
            .await
            .map_err(RealtimeError::Connection)?;

        info!("WebSocket handshake successful.");

        // Split the stream into a sender (Sink) and a receiver (Stream)
        let (write, read) = ws_stream.split();

        // 1. Channel for *incoming* messages (from server to developer)
        // 100 is a reasonable buffer size for high-throughput applications.
        let (tx_to_dev, rx_from_server) = mpsc::channel(100);

        // 2. Channel for *outgoing* messages (from developer to server)
        // The sender half (tx_from_dev) is returned in the RealtimeClient struct.
        let (tx_from_dev, rx_to_server) = mpsc::channel::<Message>(100);

        // --- Spawn the background tasks for I/O ---

        // Task 1: Receiver Loop (Read from WebSocket -> Send to developer channel)
        tokio::spawn(Self::receive_loop(read, tx_to_dev));

        // Task 2: Sender Loop (Receive from developer channel -> Send to WebSocket)
        tokio::spawn(Self::send_loop(write, rx_to_server));

        Ok(RealtimeClient {
            sender: tx_from_dev,
            receiver: rx_from_server,
        })
    }
    
    /// Spawns the main task to handle all incoming messages from the WebSocket.
    ///
    /// This function runs in a separate spawned task. It processes all messages
    /// and errors from the server and forwards user-data messages (Text/Binary)
    /// to the developer's channel.
    #[instrument(skip_all)]
    async fn receive_loop(
        mut read: WsStreamReader,
        tx_to_dev: mpsc::Sender<String>,
    ) {
        info!("Starting WebSocket receiver loop.");
        while let Some(msg) = read.next().await {
            match msg {
                Ok(message) => match message {
                    Message::Text(text) => {
                        // Forward Text messages to the developer's receiver
                        if tx_to_dev.send(text).await.is_err() {
                            error!("Developer message receiver was dropped. Shutting down receive loop.");
                            break;
                        }
                    }
                    Message::Binary(_) => {
                        // For a string-based SDK, we'll ignore binary for simplicity,
                        // but a full SDK would handle this.
                        tracing::warn!("Received Binary message, ignoring for now.");
                    }
                    Message::Ping(data) => {
                        // Tungstenite should automatically send a Pong, but we can log.
                        info!("Received Ping with data: {:?}", data);
                    }
                    Message::Pong(_) => {
                        // Pong is a response to Ping, can be used for keep-alive check.
                        info!("Received Pong.");
                    }
                    Message::Close(close_frame) => {
                        let code = close_frame.as_ref().map(|f| f.code.into());
                        info!("Received Close frame. Code: {:?}", code);
                        // The loop should naturally exit here after the peer initiates close.
                        break; 
                    }
                    _ => {} // Ignore other potential message types if they arise
                },
                Err(e) => {
                    error!("WebSocket Read Error: {}. Disconnecting.", e);
                    // The error is non-recoverable, so we exit the loop.
                    break;
                }
            }
        }
        info!("WebSocket receiver loop finished.");
    }

    /// Spawns the main task to handle all outgoing messages to the WebSocket.
    ///
    /// This function runs in a separate spawned task. It receives messages from
    /// the developer's `send` call and writes them to the WebSocket sink.
    #[instrument(skip_all)]
    async fn send_loop(
        mut write: WsSink, 
        mut rx_to_server: mpsc::Receiver<Message>
    ) {
        info!("Starting WebSocket sender loop.");
        while let Some(message) = rx_to_server.recv().await {
            match write.send(message).await {
                Ok(_) => {
                    // Message successfully sent.
                }
                Err(e) => {
                    error!("WebSocket Write Error: {}. Shutting down send loop.", e);
                    // The connection is likely dead, so we exit the loop.
                    break;
                }
            }
        }
        info!("WebSocket sender loop finished.");
    }


    /// Sends a message to the connected server.
    ///
    /// This is a non-blocking operation on the application thread, as it only
    /// sends the message to an internal channel. The actual network I/O
    /// happens in a separate background task.
    ///
    /// # Arguments
    /// * `message`: The string data to send to the server.
    #[instrument(skip(self), fields(msg_len = message.len()))]
    pub async fn send(&self, message: String) -> Result<()> {
        let msg = Message::Text(message);
        self.sender
            .send(msg)
            .await
            .map_err(|e| RealtimeError::SendChannel(e.to_string()))
    }
    
    /// Returns a mutable reference to the internal message receiver.
    ///
    /// This allows the developer to use `await client.receiver().recv().await`
    /// to poll for new messages in a loop.
    pub fn receiver(&mut self) -> &mut mpsc::Receiver<String> {
        &mut self.receiver
    }

    /// Sends a close frame and waits for the connection to close gracefully.
    #[instrument(skip(self))]
    pub async fn close(&self) -> Result<()> {
        let close_message = Message::Close(Some(CloseFrame {
            code: CloseCode::Normal,
            reason: "Client shut down.".into(),
        }));

        // Send the close message over the same internal sender channel.
        self.sender
            .send(close_message)
            .await
            .map_err(|e| RealtimeError::SendChannel(e.to_string()))
    }
}
