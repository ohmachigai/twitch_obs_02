use std::{env, fmt, net::SocketAddr};

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
}

impl AppConfig {
    /// Constructs the configuration by reading and validating environment variables.
    pub fn from_env() -> Result<Self, ConfigError> {
        let env_value = env::var("APP_ENV").unwrap_or_else(|_| "development".to_string());
        let environment = Environment::from_str(&env_value)?;
        let bind_addr = server_bind_address().map_err(ConfigError::BindAddress)?;

        Ok(Self {
            bind_addr,
            environment,
        })
    }
}

/// Errors that can occur during configuration loading.
#[derive(Debug)]
pub enum ConfigError {
    InvalidEnvironment(String),
    BindAddress(std::net::AddrParseError),
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidEnvironment(value) => write!(
                f,
                "APP_ENV must be one of 'development', 'production', or 'test' (got {value})"
            ),
            Self::BindAddress(err) => write!(f, "invalid APP_BIND_ADDR value: {err}"),
        }
    }
}

impl std::error::Error for ConfigError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::DEFAULT_BIND_ADDR;
    use std::sync::{LazyLock, Mutex};

    static ENV_GUARD: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

    #[test]
    fn loads_defaults_in_development() {
        let _guard = ENV_GUARD.lock().expect("env guard poisoned");
        env::remove_var("APP_ENV");
        env::remove_var("APP_BIND_ADDR");

        let config = AppConfig::from_env().expect("config should load with defaults");
        assert_eq!(config.environment, Environment::Development);
        assert_eq!(config.bind_addr.to_string(), DEFAULT_BIND_ADDR);
    }

    #[test]
    fn rejects_invalid_environment() {
        let _guard = ENV_GUARD.lock().expect("env guard poisoned");
        env::set_var("APP_ENV", "invalid");

        let err = AppConfig::from_env().expect_err("invalid env should error");
        assert!(matches!(err, ConfigError::InvalidEnvironment(value) if value == "invalid"));

        env::remove_var("APP_ENV");
    }

    #[test]
    fn parses_production_environment() {
        let _guard = ENV_GUARD.lock().expect("env guard poisoned");
        env::set_var("APP_ENV", "production");
        env::set_var("APP_BIND_ADDR", "0.0.0.0:9000");

        let config = AppConfig::from_env().expect("config should load");
        assert_eq!(config.environment, Environment::Production);
        assert_eq!(config.bind_addr.to_string(), "0.0.0.0:9000");

        env::remove_var("APP_ENV");
        env::remove_var("APP_BIND_ADDR");
    }
}
