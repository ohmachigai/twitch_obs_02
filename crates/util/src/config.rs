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

        Ok(Self {
            bind_addr,
            environment,
            database_url,
            webhook_secret,
            sse_token_signing_key,
            sse_heartbeat_secs,
            sse_ring_max,
            sse_ring_ttl_secs,
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

        let config = AppConfig::from_env().expect("config should load");
        assert_eq!(config.environment, Environment::Production);
        assert_eq!(config.bind_addr.to_string(), "0.0.0.0:9000");
        assert_eq!(config.database_url, "sqlite:///var/app.db");
        assert_eq!(config.webhook_secret, "prod-secret");
        assert_eq!(config.sse_token_signing_key, decode_hex("abcdef").unwrap());
        assert_eq!(config.sse_heartbeat_secs, 30);
        assert_eq!(config.sse_ring_max, 512);
        assert_eq!(config.sse_ring_ttl_secs, 180);

        env::remove_var("APP_ENV");
        env::remove_var("APP_BIND_ADDR");
        env::remove_var("DATABASE_URL");
        env::remove_var("WEBHOOK_SECRET");
        env::remove_var("SSE_TOKEN_SIGNING_KEY");
        env::remove_var("SSE_HEARTBEAT_SECS");
        env::remove_var("SSE_RING_MAX");
        env::remove_var("SSE_RING_TTL_SECS");
    }

    #[test]
    fn production_requires_webhook_secret() {
        let _guard = test_support::env_vars_lock();
        env::set_var("APP_ENV", "production");
        env::remove_var("WEBHOOK_SECRET");

        let err = AppConfig::from_env().expect_err("missing secret should error");
        assert!(
            matches!(err, ConfigError::MissingEnvVar(message) if message.contains("WEBHOOK_SECRET"))
        );

        env::remove_var("APP_ENV");
    }
}
