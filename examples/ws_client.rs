// src/main.rs (Example usage of the SDK)

use realtime_websocket_sdk::RealtimeClient;
use tokio::time::{sleep, Duration};
use tracing_subscriber;

// This example will connect to the official public echo server.
const WS_URL: &str = "wss://echo.websocket.events/";

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Setup tracing for better visibility into the client's internal operations
    tracing_subscriber::fmt::init();
    
    println!("Connecting to: {}", WS_URL);
    
    // Connect to the WebSocket server
    let mut client = RealtimeClient::connect(WS_URL).await?;
    
    println!("Connection established. Starting send and receive tasks.");

    // --- Task 1: Sender Loop (Developer's logic) ---
    // This task runs concurrently and is not blocked by the network I/O.
    let send_handle = tokio::spawn({
        let client_sender = client.sender.clone(); // Clone the mpsc::Sender for the send task
        async move {
            for i in 1..=5 {
                let message = format!("Hello, world! Message #{}", i);
                println!("[SENDER] Sending: '{}'", message);
                
                // This is non-blocking because it only pushes to an internal channel.
                if let Err(e) = client_sender.send(message).await {
                    eprintln!("[SENDER] Failed to send message: {}", e);
                    break;
                }
                sleep(Duration::from_millis(500)).await;
            }
            println!("[SENDER] Send task finished.");
        }
    });

    // --- Task 2: Receiver Loop (Developer's logic) ---
    // This task runs concurrently and uses the SDK's channel receiver.
    let mut rx = client.receiver();
    let receive_handle = tokio::spawn(async move {
        // Use a simple loop to wait for and process incoming messages
        while let Some(message) = rx.recv().await {
            println!("[RECEIVER] Received: '{}'", message);
            if message.contains("#5") {
                println!("[RECEIVER] Received last message. Exiting receive loop.");
                break;
            }
        }
        println!("[RECEIVER] Receive task finished.");
    });

    // Wait for both tasks to complete
    let (_, _) = tokio::join!(send_handle, receive_handle);

    // Close the connection
    println!("All messages processed. Closing connection gracefully...");
    client.close().await?;
    println!("Connection closed. Program finished.");
    
    Ok(())
}
