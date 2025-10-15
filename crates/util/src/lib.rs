pub mod config;

use std::{env, net::SocketAddr};

pub use config::{AppConfig, ConfigError, Environment};

pub const DEFAULT_BIND_ADDR: &str = "127.0.0.1:8080";

/// Loads environment variables from `.env` when available.
///
/// Missing files are ignored so the function is safe in production builds
/// where dotenv files are not deployed.
pub fn load_env_file() {
    let _ = dotenvy::dotenv();
}

/// Returns the address the HTTP server should bind to.
///
/// The value is resolved from the `APP_BIND_ADDR` environment variable and
/// falls back to [`DEFAULT_BIND_ADDR`] when the variable is not set.
pub fn server_bind_address() -> Result<SocketAddr, std::net::AddrParseError> {
    let value = env::var("APP_BIND_ADDR").unwrap_or_else(|_| DEFAULT_BIND_ADDR.to_string());
    value.parse()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        env,
        sync::{LazyLock, Mutex},
    };

    static ENV_GUARD: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

    #[test]
    fn returns_default_address_when_env_missing() {
        let _lock = ENV_GUARD.lock().expect("env guard poisoned");
        env::remove_var("APP_BIND_ADDR");
        let addr = server_bind_address().expect("default address is valid");
        assert_eq!(addr.to_string(), DEFAULT_BIND_ADDR);
    }

    #[test]
    fn parses_custom_address_from_env() {
        let _lock = ENV_GUARD.lock().expect("env guard poisoned");
        env::set_var("APP_BIND_ADDR", "0.0.0.0:9000");
        let addr = server_bind_address().expect("custom address should parse");
        assert_eq!(addr.to_string(), "0.0.0.0:9000");
        env::remove_var("APP_BIND_ADDR");
    }
}
