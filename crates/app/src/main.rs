mod router;

use std::net::SocketAddr;

use tracing::info;
use twi_overlay_util::{load_env_file, server_bind_address};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    load_env_file();
    let addr: SocketAddr = server_bind_address()?;

    info!(stage = "app", %addr, "starting HTTP server");

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, router::app_router())
        .await
        .map_err(|err| err.into())
}
