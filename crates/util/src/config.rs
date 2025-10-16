use std::{env, fmt, net::SocketAddr};

const DEV_SSE_TOKEN_HEX: &str = "6465762d7373652d7365637265742d6368616e67652d6d65";

use super::server_bind_address;

/// Application runtime environment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Environment {
    Development,
    Production,
    Test,
}

impl Environment {
    fn from_str(value: &str) -> Result<Self, ConfigError> {
        match value {
            "development" | "dev" => Ok(Self::Development),
            "production" | "prod" => Ok(Self::Production),
            "test" => Ok(Self::Test),
            other => Err(ConfigError::InvalidEnvironment(other.to_string())),
        }
    }

    /// Returns `true` when the current environment should behave as development.
    pub fn is_development(self) -> bool {
        matches!(self, Self::Development)
    }

    /// Returns `true` when running tests.
    pub fn is_test(self) -> bool {
        matches!(self, Self::Test)
    }

    /// Returns the canonical name used for logging/metrics labels.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Development => "development",
            Self::Production => "production",
            Self::Test => "test",
        }
    }
}

/// Runtime configuration resolved from environment variables.
#[derive(Debug, Clone)]
pub struct AppConfig {
    pub bind_addr: SocketAddr,
    pub environment: Environment,
    pub database_url: String,
    pub webhook_secret: String,
    pub sse_token_signing_key: Vec<u8>,
    pub sse_heartbeat_secs: u64,
    pub sse_ring_max: usize,
    pub sse_ring_ttl_secs: u64,
    pub twitch_client_id: String,
    pub twitch_client_secret: String,
    pub oauth_redirect_uri: String,
    pub twitch_oauth_base_url: String,
    pub twitch_api_base_url: String,
    pub oauth_state_ttl_secs: u64,
    pub helix_backfill_interval_secs: u64,
    pub helix_backfill_page_size: u32,
}

impl AppConfig {
    /// Constructs the configuration by reading and validating environment variables.
    pub fn from_env() -> Result<Self, ConfigError> {
        let env_value = env::var("APP_ENV").unwrap_or_else(|_| "development".to_string());
        let environment = Environment::from_str(&env_value)?;
        let bind_addr = server_bind_address().map_err(ConfigError::BindAddress)?;
        let database_url =
            env::var("DATABASE_URL").unwrap_or_else(|_| "sqlite://./dev.db".to_string());

        let webhook_secret = match env::var("WEBHOOK_SECRET") {
            Ok(value) if !value.is_empty() => value,
            Ok(_) => {
                return Err(ConfigError::MissingEnvVar(
                    "WEBHOOK_SECRET must not be empty".to_string(),
                ))
            }
            Err(_) if environment.is_development() || environment.is_test() => {
                "dev-secret-change-me".to_string()
            }
            Err(_) => {
                return Err(ConfigError::MissingEnvVar(
                    "WEBHOOK_SECRET is required in production".to_string(),
                ))
            }
        };

        let sse_token_signing_key = match env::var("SSE_TOKEN_SIGNING_KEY") {
            Ok(value) if !value.is_empty() => decode_hex(&value)?,
            Ok(_) => {
                return Err(ConfigError::MissingEnvVar(
                    "SSE_TOKEN_SIGNING_KEY must not be empty".to_string(),
                ))
            }
            Err(_) if environment.is_development() || environment.is_test() => {
                decode_hex(DEV_SSE_TOKEN_HEX)?
            }
            Err(_) => {
                return Err(ConfigError::MissingEnvVar(
                    "SSE_TOKEN_SIGNING_KEY is required in production".to_string(),
                ))
            }
        };

        let sse_heartbeat_secs = match env::var("SSE_HEARTBEAT_SECS") {
            Ok(value) => value
                .parse::<u64>()
                .map_err(|_| ConfigError::InvalidNumber("SSE_HEARTBEAT_SECS".to_string(), value))?,
            Err(_) => 25,
        };

        let sse_ring_max = match env::var("SSE_RING_MAX") {
            Ok(value) => value
                .parse::<usize>()
                .map_err(|_| ConfigError::InvalidNumber("SSE_RING_MAX".to_string(), value))?,
            Err(_) => 1000,
        };

        let sse_ring_ttl_secs = match env::var("SSE_RING_TTL_SECS") {
            Ok(value) => value
                .parse::<u64>()
                .map_err(|_| ConfigError::InvalidNumber("SSE_RING_TTL_SECS".to_string(), value))?,
            Err(_) => 120,
        };

        let twitch_client_id =
            read_required_secret("TWITCH_CLIENT_ID", environment, "local-client-id")?;
        let twitch_client_secret =
            read_required_secret("TWITCH_CLIENT_SECRET", environment, "local-client-secret")?;
        let oauth_redirect_uri = match env::var("OAUTH_REDIRECT_URI") {
            Ok(value) if !value.is_empty() => value,
            Ok(_) => {
                return Err(ConfigError::MissingEnvVar(
                    "OAUTH_REDIRECT_URI must not be empty".to_string(),
                ))
            }
            Err(_) if environment.is_development() || environment.is_test() => {
                "http://127.0.0.1:8080/oauth/callback".to_string()
            }
            Err(_) => {
                return Err(ConfigError::MissingEnvVar(
                    "OAUTH_REDIRECT_URI is required in production".to_string(),
                ))
            }
        };

        let twitch_oauth_base_url = env::var("TWITCH_OAUTH_BASE_URL")
            .unwrap_or_else(|_| "https://id.twitch.tv/oauth2".to_string());
        let twitch_api_base_url = env::var("TWITCH_API_BASE_URL")
            .unwrap_or_else(|_| "https://api.twitch.tv/helix".to_string());

        let oauth_state_ttl_secs = match env::var("OAUTH_STATE_TTL_SECS") {
            Ok(value) => value.parse::<u64>().map_err(|_| {
                ConfigError::InvalidNumber("OAUTH_STATE_TTL_SECS".to_string(), value)
            })?,
            Err(_) => 600,
        };

        let helix_backfill_interval_secs = match env::var("HELIX_BACKFILL_INTERVAL_SECS") {
            Ok(value) => value.parse::<u64>().map_err(|_| {
                ConfigError::InvalidNumber("HELIX_BACKFILL_INTERVAL_SECS".to_string(), value)
            })?,
            Err(_) => 300,
        };

        let helix_backfill_page_size = match env::var("HELIX_BACKFILL_PAGE_SIZE") {
            Ok(value) => value.parse::<u32>().map_err(|_| {
                ConfigError::InvalidNumber("HELIX_BACKFILL_PAGE_SIZE".to_string(), value)
            })?,
            Err(_) => 50,
        };

        Ok(Self {
            bind_addr,
            environment,
            database_url,
            webhook_secret,
            sse_token_signing_key,
            sse_heartbeat_secs,
            sse_ring_max,
            sse_ring_ttl_secs,
            twitch_client_id,
            twitch_client_secret,
            oauth_redirect_uri,
            twitch_oauth_base_url,
            twitch_api_base_url,
            oauth_state_ttl_secs,
            helix_backfill_interval_secs,
            helix_backfill_page_size,
        })
    }
}

/// Errors that can occur during configuration loading.
#[derive(Debug)]
pub enum ConfigError {
    InvalidEnvironment(String),
    BindAddress(std::net::AddrParseError),
    MissingEnvVar(String),
    InvalidHex(String),
    InvalidNumber(String, String),
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidEnvironment(value) => write!(
                f,
                "APP_ENV must be one of 'development', 'production', or 'test' (got {value})"
            ),
            Self::BindAddress(err) => write!(f, "invalid APP_BIND_ADDR value: {err}"),
            Self::MissingEnvVar(message) => write!(f, "{message}"),
            Self::InvalidHex(var) => write!(f, "{var} must be a valid hex string"),
            Self::InvalidNumber(var, value) => {
                write!(f, "{var} must be a valid number (got {value})")
            }
        }
    }
}

impl std::error::Error for ConfigError {}

fn decode_hex(value: &str) -> Result<Vec<u8>, ConfigError> {
    hex::decode(value).map_err(|_| ConfigError::InvalidHex(value.to_string()))
}

fn read_required_secret(
    var: &str,
    environment: Environment,
    dev_default: &str,
) -> Result<String, ConfigError> {
    match env::var(var) {
        Ok(value) if !value.is_empty() => Ok(value),
        Ok(_) => Err(ConfigError::MissingEnvVar(format!(
            "{var} must not be empty"
        ))),
        Err(_) if environment.is_development() || environment.is_test() => {
            Ok(dev_default.to_string())
        }
        Err(_) => Err(ConfigError::MissingEnvVar(format!(
            "{var} is required in production"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{test_support, DEFAULT_BIND_ADDR};
    use std::env;

    #[test]
    fn loads_defaults_in_development() {
        let _guard = test_support::env_vars_lock();
        env::remove_var("APP_ENV");
        env::remove_var("APP_BIND_ADDR");
        env::remove_var("DATABASE_URL");
        env::remove_var("WEBHOOK_SECRET");

        let config = AppConfig::from_env().expect("config should load with defaults");
        assert_eq!(config.environment, Environment::Development);
        assert_eq!(config.bind_addr.to_string(), DEFAULT_BIND_ADDR);
        assert_eq!(config.database_url, "sqlite://./dev.db");
        assert_eq!(config.webhook_secret, "dev-secret-change-me");
        assert_eq!(
            config.sse_token_signing_key,
            decode_hex(DEV_SSE_TOKEN_HEX).unwrap()
        );
        assert_eq!(config.sse_heartbeat_secs, 25);
        assert_eq!(config.sse_ring_max, 1000);
        assert_eq!(config.sse_ring_ttl_secs, 120);
        assert_eq!(config.twitch_client_id, "local-client-id");
        assert_eq!(config.twitch_client_secret, "local-client-secret");
        assert_eq!(
            config.oauth_redirect_uri,
            "http://127.0.0.1:8080/oauth/callback"
        );
        assert_eq!(config.twitch_oauth_base_url, "https://id.twitch.tv/oauth2");
        assert_eq!(config.twitch_api_base_url, "https://api.twitch.tv/helix");
        assert_eq!(config.oauth_state_ttl_secs, 600);
        assert_eq!(config.helix_backfill_interval_secs, 300);
        assert_eq!(config.helix_backfill_page_size, 50);
    }

    #[test]
    fn rejects_invalid_environment() {
        let _guard = test_support::env_vars_lock();
        env::set_var("APP_ENV", "invalid");

        let err = AppConfig::from_env().expect_err("invalid env should error");
        assert!(matches!(err, ConfigError::InvalidEnvironment(value) if value == "invalid"));

        env::remove_var("APP_ENV");
    }

    #[test]
    fn parses_production_environment() {
        let _guard = test_support::env_vars_lock();
        env::set_var("APP_ENV", "production");
        env::set_var("APP_BIND_ADDR", "0.0.0.0:9000");
        env::set_var("DATABASE_URL", "sqlite:///var/app.db");
        env::set_var("WEBHOOK_SECRET", "prod-secret");
        env::set_var("SSE_TOKEN_SIGNING_KEY", "abcdef");
        env::set_var("SSE_HEARTBEAT_SECS", "30");
        env::set_var("SSE_RING_MAX", "512");
        env::set_var("SSE_RING_TTL_SECS", "180");
        env::set_var("TWITCH_CLIENT_ID", "prod-client");
        env::set_var("TWITCH_CLIENT_SECRET", "prod-secret");
        env::set_var("OAUTH_REDIRECT_URI", "https://example.com/oauth/callback");
        env::set_var("OAUTH_STATE_TTL_SECS", "900");
        env::set_var("HELIX_BACKFILL_INTERVAL_SECS", "120");
        env::set_var("HELIX_BACKFILL_PAGE_SIZE", "75");

        let config = AppConfig::from_env().expect("config should load");
        assert_eq!(config.environment, Environment::Production);
        assert_eq!(config.bind_addr.to_string(), "0.0.0.0:9000");
        assert_eq!(config.database_url, "sqlite:///var/app.db");
        assert_eq!(config.webhook_secret, "prod-secret");
        assert_eq!(config.sse_token_signing_key, decode_hex("abcdef").unwrap());
        assert_eq!(config.sse_heartbeat_secs, 30);
        assert_eq!(config.sse_ring_max, 512);
        assert_eq!(config.sse_ring_ttl_secs, 180);
        assert_eq!(config.twitch_client_id, "prod-client");
        assert_eq!(config.twitch_client_secret, "prod-secret");
        assert_eq!(
            config.oauth_redirect_uri,
            "https://example.com/oauth/callback"
        );
        assert_eq!(config.oauth_state_ttl_secs, 900);
        assert_eq!(config.helix_backfill_interval_secs, 120);
        assert_eq!(config.helix_backfill_page_size, 75);

        env::remove_var("APP_ENV");
        env::remove_var("APP_BIND_ADDR");
        env::remove_var("DATABASE_URL");
        env::remove_var("WEBHOOK_SECRET");
        env::remove_var("SSE_TOKEN_SIGNING_KEY");
        env::remove_var("SSE_HEARTBEAT_SECS");
        env::remove_var("SSE_RING_MAX");
        env::remove_var("SSE_RING_TTL_SECS");
        env::remove_var("TWITCH_CLIENT_ID");
        env::remove_var("TWITCH_CLIENT_SECRET");
        env::remove_var("OAUTH_REDIRECT_URI");
        env::remove_var("OAUTH_STATE_TTL_SECS");
        env::remove_var("HELIX_BACKFILL_INTERVAL_SECS");
        env::remove_var("HELIX_BACKFILL_PAGE_SIZE");
    }

    #[test]
    fn production_requires_webhook_secret() {
        let _guard = test_support::env_vars_lock();
        env::set_var("APP_ENV", "production");
        env::remove_var("WEBHOOK_SECRET");
        env::set_var("TWITCH_CLIENT_ID", "prod-client");
        env::set_var("TWITCH_CLIENT_SECRET", "prod-secret");
        env::set_var("OAUTH_REDIRECT_URI", "https://example.com/oauth/callback");
        env::set_var("SSE_TOKEN_SIGNING_KEY", "abcdef");

        let err = AppConfig::from_env().expect_err("missing secret should error");
        assert!(
            matches!(err, ConfigError::MissingEnvVar(message) if message.contains("WEBHOOK_SECRET"))
        );

        env::remove_var("APP_ENV");
        env::remove_var("TWITCH_CLIENT_ID");
        env::remove_var("TWITCH_CLIENT_SECRET");
        env::remove_var("OAUTH_REDIRECT_URI");
        env::remove_var("SSE_TOKEN_SIGNING_KEY");
    }

    #[test]
    fn production_requires_twitch_client_secret() {
        let _guard = test_support::env_vars_lock();
        env::set_var("APP_ENV", "production");
        env::set_var("WEBHOOK_SECRET", "prod-secret");
        env::set_var("TWITCH_CLIENT_ID", "prod-client");
        env::remove_var("TWITCH_CLIENT_SECRET");
        env::set_var("OAUTH_REDIRECT_URI", "https://example.com/oauth/callback");
        env::set_var("SSE_TOKEN_SIGNING_KEY", "abcdef");

        let err = AppConfig::from_env().expect_err("missing secret should error");
        assert!(matches!(
            err,
            ConfigError::MissingEnvVar(message) if message.contains("TWITCH_CLIENT_SECRET")
        ));

        env::remove_var("APP_ENV");
        env::remove_var("WEBHOOK_SECRET");
        env::remove_var("TWITCH_CLIENT_ID");
        env::remove_var("OAUTH_REDIRECT_URI");
        env::remove_var("SSE_TOKEN_SIGNING_KEY");
    }
}
