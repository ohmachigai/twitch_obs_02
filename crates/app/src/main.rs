mod backfill;
mod command;
mod maintenance;
mod oauth;
mod problem;
mod router;
mod sse;
mod state;
mod tap;
mod telemetry;
mod webhook;

use std::{net::SocketAddr, sync::Arc, time::Duration};

use reqwest::Client;
use tracing::info;
use twi_overlay_storage::Database;
use twi_overlay_twitch::{HelixClient, TwitchOAuthClient};
use twi_overlay_util::{load_env_file, AppConfig};
use url::Url;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    load_env_file();
    let config = AppConfig::from_env()?;

    telemetry::init_tracing(&config)?;
    let metrics = telemetry::init_metrics()?;

    let tap_hub = tap::TapHub::new();
    if config.environment.is_development() {
        tap_hub.spawn_mock_publisher();
    }

    let database = Database::connect(&config.database_url).await?;
    database.run_migrations().await?;

    let _maintenance_handle =
        maintenance::MaintenanceWorker::new(database.clone(), tap_hub.clone()).spawn();

    let webhook_secret: Arc<[u8]> = Arc::from(
        config
            .webhook_secret
            .clone()
            .into_bytes()
            .into_boxed_slice(),
    );

    let oauth_http = Client::builder().build()?;
    let helix_http = Client::builder().build()?;
    let oauth_base_url = Url::parse(&ensure_trailing_slash(&config.twitch_oauth_base_url))?;
    let oauth_client = TwitchOAuthClient::new(
        config.twitch_client_id.clone(),
        config.twitch_client_secret.clone(),
        oauth_base_url,
        oauth_http,
    );
    let helix_base_url = Url::parse(&ensure_trailing_slash(&config.twitch_api_base_url))?;
    let helix_client =
        HelixClient::new(config.twitch_client_id.clone(), helix_base_url, helix_http);

    let (state, backfill_worker) = router::AppState::new(
        metrics,
        tap_hub.clone(),
        database,
        webhook_secret,
        config.sse_token_signing_key.clone(),
        config.sse_ring_max,
        Duration::from_secs(config.sse_ring_ttl_secs),
        config.sse_heartbeat_secs,
        helix_client,
        oauth_client,
        config.oauth_redirect_uri.clone(),
        Duration::from_secs(config.oauth_state_ttl_secs),
        Duration::from_secs(config.helix_backfill_interval_secs),
        config.helix_backfill_page_size,
    );

    let _backfill_handle = backfill_worker.spawn();

    let addr: SocketAddr = config.bind_addr;
    info!(stage = "app", %addr, env = %config.environment.as_str(), "starting HTTP server");

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, router::app_router(state))
        .await
        .map_err(|err| err.into())
}

fn ensure_trailing_slash(value: &str) -> String {
    if value.ends_with('/') {
        value.to_string()
    } else {
        format!("{value}/")
    }
}
