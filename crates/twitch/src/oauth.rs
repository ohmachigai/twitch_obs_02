use chrono::{DateTime, Duration, Utc};
use reqwest::{Client, Response, StatusCode};
use serde::{de::DeserializeOwned, Deserialize};
use thiserror::Error;
use url::Url;

/// Client responsible for OAuth flow interactions with Twitch.
#[derive(Clone)]
pub struct TwitchOAuthClient {
    http: Client,
    base_url: Url,
    client_id: String,
    client_secret: String,
}

impl TwitchOAuthClient {
    /// Creates a new client with the provided HTTP instance and configuration.
    pub fn new(
        client_id: impl Into<String>,
        client_secret: impl Into<String>,
        base_url: Url,
        http: Client,
    ) -> Self {
        Self {
            http,
            base_url,
            client_id: client_id.into(),
            client_secret: client_secret.into(),
        }
    }

    /// Builds the authorization URL with PKCE parameters.
    pub fn authorize_url(&self, params: &AuthorizeUrlParams<'_>) -> Result<Url, OAuthError> {
        let mut url = self.base_url.join("authorize")?;
        {
            let mut query = url.query_pairs_mut();
            query.append_pair("client_id", &self.client_id);
            query.append_pair("redirect_uri", params.redirect_uri);
            query.append_pair("response_type", "code");
            query.append_pair("scope", &params.scopes.join(" "));
            query.append_pair("state", params.state);
            query.append_pair("code_challenge", params.code_challenge);
            query.append_pair("code_challenge_method", "S256");
            query.append_pair("force_verify", "true");
            if let Some(login_hint) = params.login_hint {
                query.append_pair("login_hint", login_hint);
            }
        }

        Ok(url)
    }

    /// Exchanges an authorization code for access and refresh tokens.
    pub async fn exchange_code(
        &self,
        code: &str,
        code_verifier: &str,
        redirect_uri: &str,
    ) -> Result<TokenResponse, OAuthError> {
        let url = self.base_url.join("token")?;
        let response = self
            .http
            .post(url)
            .form(&[
                ("client_id", self.client_id.as_str()),
                ("client_secret", self.client_secret.as_str()),
                ("code", code),
                ("grant_type", "authorization_code"),
                ("redirect_uri", redirect_uri),
                ("code_verifier", code_verifier),
            ])
            .send()
            .await?;

        parse_json(response).await
    }

    /// Refreshes the access token using the stored refresh token.
    pub async fn refresh_token(&self, refresh_token: &str) -> Result<TokenResponse, OAuthError> {
        let url = self.base_url.join("token")?;
        let response = self
            .http
            .post(url)
            .form(&[
                ("client_id", self.client_id.as_str()),
                ("client_secret", self.client_secret.as_str()),
                ("refresh_token", refresh_token),
                ("grant_type", "refresh_token"),
            ])
            .send()
            .await?;

        parse_json(response).await
    }

    /// Validates the provided access token and returns metadata.
    pub async fn validate_token(
        &self,
        access_token: &str,
    ) -> Result<ValidateTokenResponse, OAuthError> {
        let url = self.base_url.join("validate")?;
        let response = self
            .http
            .get(url)
            .header("Authorization", format!("OAuth {access_token}"))
            .send()
            .await?;

        parse_json(response).await
    }
}

/// Parameters required to generate an authorization URL.
pub struct AuthorizeUrlParams<'a> {
    pub state: &'a str,
    pub redirect_uri: &'a str,
    pub code_challenge: &'a str,
    pub scopes: &'a [&'a str],
    pub login_hint: Option<&'a str>,
}

/// Token exchange/refresh response returned by Twitch.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct TokenResponse {
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: Option<String>,
    pub expires_in: u64,
    #[serde(default)]
    pub scope: Vec<String>,
    pub token_type: String,
}

impl TokenResponse {
    /// Computes the expiration timestamp relative to the provided instant.
    pub fn expires_at(&self, now: DateTime<Utc>) -> DateTime<Utc> {
        now + Duration::seconds(self.expires_in as i64)
    }
}

/// Validation response describing the access token.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct ValidateTokenResponse {
    pub client_id: String,
    pub login: String,
    pub scopes: Vec<String>,
    pub user_id: String,
    pub expires_in: u64,
}

/// Errors that can occur during OAuth interactions.
#[derive(Debug, Error)]
pub enum OAuthError {
    #[error("failed to build url: {0}")]
    Url(#[from] url::ParseError),
    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("unexpected status {status}: {body}")]
    Status { status: StatusCode, body: String },
}

async fn parse_json<T>(response: Response) -> Result<T, OAuthError>
where
    T: DeserializeOwned,
{
    let status = response.status();
    if !status.is_success() {
        let body = response
            .text()
            .await
            .unwrap_or_else(|_| String::from("<unavailable>"));
        return Err(OAuthError::Status { status, body });
    }

    Ok(response.json().await?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use httpmock::prelude::*;
    use serde_json::json;
    use std::borrow::Cow;

    fn client(base_url: &Url) -> TwitchOAuthClient {
        TwitchOAuthClient::new(
            "client",
            "secret",
            base_url.clone(),
            Client::builder().build().expect("client"),
        )
    }

    #[test]
    fn authorize_url_contains_expected_parameters() {
        let base = Url::parse("https://id.twitch.tv/oauth2/").expect("url");
        let client = client(&base);
        let url = client
            .authorize_url(&AuthorizeUrlParams {
                state: "state-123",
                redirect_uri: "https://example.com/callback",
                code_challenge: "challenge",
                scopes: &["channel:read:redemptions", "channel:manage:redemptions"],
                login_hint: Some("user123"),
            })
            .expect("url");

        let query: Vec<(Cow<'_, str>, Cow<'_, str>)> = url.query_pairs().collect();
        assert!(query.contains(&(Cow::Borrowed("client_id"), Cow::Borrowed("client"))));
        assert!(query.contains(&(Cow::Borrowed("state"), Cow::Borrowed("state-123"))));
        assert!(query.contains(&(Cow::Borrowed("code_challenge"), Cow::Borrowed("challenge"))));
        assert!(query
            .iter()
            .any(|(k, v)| k == "scope" && v.contains("channel:manage:redemptions")));
        assert!(query
            .iter()
            .any(|(k, v)| k == "login_hint" && v == "user123"));
    }

    #[tokio::test]
    async fn exchange_code_returns_tokens() {
        let server = MockServer::start_async().await;
        let base = Url::parse(&server.url("/oauth2/")).expect("url");
        let client = client(&base);

        let mock = server
            .mock_async(|when, then| {
                when.method(POST)
                    .path("/oauth2/token")
                    .body_contains("code=test-code")
                    .body_contains("code_verifier=verifier");
                then.status(200).json_body(json!({
                    "access_token": "access",
                    "refresh_token": "refresh",
                    "expires_in": 3600,
                    "scope": ["scope"],
                    "token_type": "bearer"
                }));
            })
            .await;

        let response = client
            .exchange_code("test-code", "verifier", "https://example.com")
            .await
            .expect("exchange");
        mock.assert_async().await;
        assert_eq!(response.access_token, "access");
        assert_eq!(response.refresh_token.as_deref(), Some("refresh"));
    }

    #[tokio::test]
    async fn refresh_token_roundtrips() {
        let server = MockServer::start_async().await;
        let base = Url::parse(&server.url("/oauth2/")).expect("url");
        let client = client(&base);

        let mock = server
            .mock_async(|when, then| {
                when.method(POST)
                    .path("/oauth2/token")
                    .body_contains("grant_type=refresh_token")
                    .body_contains("refresh_token=refresh");
                then.status(200).json_body(json!({
                    "access_token": "new-access",
                    "refresh_token": "new-refresh",
                    "expires_in": 4000,
                    "scope": [],
                    "token_type": "bearer"
                }));
            })
            .await;

        let response = client.refresh_token("refresh").await.expect("refresh");
        mock.assert_async().await;
        assert_eq!(response.access_token, "new-access");
        assert_eq!(response.refresh_token.as_deref(), Some("new-refresh"));
    }

    #[tokio::test]
    async fn validate_token_parses_response() {
        let server = MockServer::start_async().await;
        let base = Url::parse(&server.url("/oauth2/")).expect("url");
        let client = client(&base);

        let mock = server
            .mock_async(|when, then| {
                when.method(GET)
                    .path("/oauth2/validate")
                    .header("Authorization", "OAuth access");
                then.status(200).json_body(json!({
                    "client_id": "client",
                    "login": "user",
                    "scopes": ["scope"],
                    "user_id": "123",
                    "expires_in": 10
                }));
            })
            .await;

        let response = client.validate_token("access").await.expect("validate");
        mock.assert_async().await;
        assert_eq!(response.login, "user");
        assert_eq!(response.expires_in, 10);
    }

    #[tokio::test]
    async fn non_success_status_returns_error() {
        let server = MockServer::start_async().await;
        let base = Url::parse(&server.url("/oauth2/")).expect("url");
        let client = client(&base);

        server
            .mock_async(|when, then| {
                when.method(POST).path("/oauth2/token");
                then.status(400).body("bad request");
            })
            .await;

        let err = client
            .refresh_token("refresh")
            .await
            .expect_err("should error");
        match err {
            OAuthError::Status { status, body } => {
                assert_eq!(status, StatusCode::BAD_REQUEST);
                assert_eq!(body, "bad request");
            }
            other => panic!("unexpected error: {:?}", other),
        }
    }
}
