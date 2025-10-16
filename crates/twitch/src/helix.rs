use chrono::{DateTime, Utc};
use reqwest::{Client, Method, Response, StatusCode};
use serde::{de::DeserializeOwned, Deserialize};
use thiserror::Error;
use url::Url;

/// Client for interacting with Twitch Helix APIs relevant to channel point redemptions.
#[derive(Clone)]
pub struct HelixClient {
    http: Client,
    base_url: Url,
    client_id: String,
}

impl HelixClient {
    /// Creates a new Helix client with the provided configuration.
    pub fn new(client_id: impl Into<String>, base_url: Url, http: Client) -> Self {
        Self {
            http,
            base_url,
            client_id: client_id.into(),
        }
    }

    /// Issues a PATCH call to update the status of a redemption.
    pub async fn update_redemption(
        &self,
        access_token: &str,
        request: &UpdateRedemptionRequest<'_>,
    ) -> Result<(), HelixError> {
        let mut url = self
            .base_url
            .join("channel_points/custom_rewards/redemptions")?;
        {
            let mut query = url.query_pairs_mut();
            query.append_pair("broadcaster_id", request.broadcaster_id);
            query.append_pair("reward_id", request.reward_id);
            query.append_pair("id", request.redemption_id);
        }

        let body = serde_json::json!({ "status": request.status.as_str() });
        let response = self
            .authorized_request(Method::PATCH, url, access_token)
            .json(&body)
            .send()
            .await?;

        ensure_success(response).await
    }

    /// Fetches redemptions for the provided broadcaster.
    pub async fn list_redemptions(
        &self,
        access_token: &str,
        params: &ListRedemptionsParams<'_>,
    ) -> Result<HelixRedemptionPage, HelixError> {
        let mut url = self
            .base_url
            .join("channel_points/custom_rewards/redemptions")?;
        {
            let mut query = url.query_pairs_mut();
            query.append_pair("broadcaster_id", params.broadcaster_id);
            query.append_pair("status", params.status.as_str());
            if let Some(reward_id) = params.reward_id {
                query.append_pair("reward_id", reward_id);
            }
            if let Some(after) = params.after {
                query.append_pair("after", after);
            }
            if let Some(first) = params.first {
                query.append_pair("first", &first.to_string());
            }
        }

        let response = self
            .authorized_request(Method::GET, url, access_token)
            .send()
            .await?;

        parse_json::<HelixRedemptionListResponse>(response)
            .await
            .map(HelixRedemptionPage::from)
    }

    fn authorized_request(
        &self,
        method: Method,
        url: Url,
        access_token: &str,
    ) -> reqwest::RequestBuilder {
        self.http
            .request(method, url)
            .header("Client-Id", &self.client_id)
            .header("Authorization", format!("Bearer {access_token}"))
    }
}

/// Parameters for updating a redemption.
pub struct UpdateRedemptionRequest<'a> {
    pub broadcaster_id: &'a str,
    pub reward_id: &'a str,
    pub redemption_id: &'a str,
    pub status: HelixRedemptionStatus,
}

/// Parameters when listing redemptions.
pub struct ListRedemptionsParams<'a> {
    pub broadcaster_id: &'a str,
    pub reward_id: Option<&'a str>,
    pub status: HelixRedemptionStatus,
    pub after: Option<&'a str>,
    pub first: Option<u32>,
}

/// Possible redemption statuses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum HelixRedemptionStatus {
    Unfulfilled,
    Fulfilled,
    Canceled,
}

impl HelixRedemptionStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unfulfilled => "UNFULFILLED",
            Self::Fulfilled => "FULFILLED",
            Self::Canceled => "CANCELED",
        }
    }
}

/// Page of redemption results.
#[derive(Debug, Clone, PartialEq)]
pub struct HelixRedemptionPage {
    pub data: Vec<HelixRedemption>,
    pub cursor: Option<String>,
}

impl From<HelixRedemptionListResponse> for HelixRedemptionPage {
    fn from(value: HelixRedemptionListResponse) -> Self {
        Self {
            data: value.data,
            cursor: value.pagination.and_then(|p| p.cursor),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
struct HelixRedemptionListResponse {
    data: Vec<HelixRedemption>,
    pagination: Option<Pagination>,
}

#[derive(Debug, Clone, Deserialize)]
struct Pagination {
    cursor: Option<String>,
}

/// Representation of a single redemption entry.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct HelixRedemption {
    pub id: String,
    pub broadcaster_id: String,
    pub broadcaster_login: String,
    pub broadcaster_name: String,
    pub user_id: String,
    pub user_login: String,
    pub user_name: String,
    pub user_input: String,
    pub status: HelixRedemptionStatus,
    pub reward: HelixReward,
    pub redeemed_at: DateTime<Utc>,
}

/// Reward metadata embedded within a redemption.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct HelixReward {
    pub id: String,
    pub title: String,
    pub prompt: Option<String>,
    pub cost: i64,
}

/// Errors produced by the Helix client.
#[derive(Debug, Error)]
pub enum HelixError {
    #[error("failed to build url: {0}")]
    Url(#[from] url::ParseError),
    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("unexpected status {status}: {body}")]
    Status { status: StatusCode, body: String },
}

async fn ensure_success(response: Response) -> Result<(), HelixError> {
    let status = response.status();
    if !status.is_success() {
        let body = response
            .text()
            .await
            .unwrap_or_else(|_| String::from("<unavailable>"));
        return Err(HelixError::Status { status, body });
    }
    Ok(())
}

async fn parse_json<T>(response: Response) -> Result<T, HelixError>
where
    T: DeserializeOwned,
{
    let status = response.status();
    if !status.is_success() {
        let body = response
            .text()
            .await
            .unwrap_or_else(|_| String::from("<unavailable>"));
        return Err(HelixError::Status { status, body });
    }

    Ok(response.json().await?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use httpmock::prelude::*;
    use httpmock::Method;
    use serde_json::json;

    fn client(base_url: &Url) -> HelixClient {
        HelixClient::new(
            "client-id",
            base_url.clone(),
            Client::builder().build().expect("client"),
        )
    }

    #[tokio::test]
    async fn list_redemptions_parses_response() {
        let server = MockServer::start_async().await;
        let base = Url::parse(&server.url("/helix/")).expect("url");
        let client = client(&base);

        let mock = server
            .mock_async(|when, then| {
                when.method(GET)
                    .path("/helix/channel_points/custom_rewards/redemptions")
                    .query_param("broadcaster_id", "b-1")
                    .query_param("status", "UNFULFILLED")
                    .query_param("first", "50");
                then.status(200).json_body(json!({
                    "data": [
                        {
                            "id": "red-1",
                            "broadcaster_id": "b-1",
                            "broadcaster_login": "streamer",
                            "broadcaster_name": "Streamer",
                            "user_id": "u-1",
                            "user_login": "viewer",
                            "user_name": "Viewer",
                            "user_input": "hello",
                            "status": "UNFULFILLED",
                            "reward": {
                                "id": "reward-1",
                                "title": "Wave",
                                "prompt": "Say hello",
                                "cost": 1000
                            },
                            "redeemed_at": "2024-01-01T00:00:00Z"
                        }
                    ],
                    "pagination": { "cursor": "cursor" }
                }));
            })
            .await;

        let result = client
            .list_redemptions(
                "token",
                &ListRedemptionsParams {
                    broadcaster_id: "b-1",
                    reward_id: None,
                    status: HelixRedemptionStatus::Unfulfilled,
                    after: None,
                    first: Some(50),
                },
            )
            .await
            .expect("list redemptions");
        mock.assert_async().await;

        assert_eq!(result.data.len(), 1);
        assert_eq!(result.cursor.as_deref(), Some("cursor"));
        assert_eq!(result.data[0].reward.title, "Wave");
    }

    #[tokio::test]
    async fn update_redemption_sends_patch() {
        let server = MockServer::start_async().await;
        let base = Url::parse(&server.url("/helix/")).expect("url");
        let client = client(&base);

        let mock = server
            .mock_async(|when, then| {
                when.method(Method::PATCH)
                    .path("/helix/channel_points/custom_rewards/redemptions")
                    .query_param("broadcaster_id", "b-1")
                    .query_param("reward_id", "reward-1")
                    .query_param("id", "red-1")
                    .header("Authorization", "Bearer token")
                    .header("Client-Id", "client-id")
                    .json_body(json!({ "status": "FULFILLED" }));
                then.status(204);
            })
            .await;

        client
            .update_redemption(
                "token",
                &UpdateRedemptionRequest {
                    broadcaster_id: "b-1",
                    reward_id: "reward-1",
                    redemption_id: "red-1",
                    status: HelixRedemptionStatus::Fulfilled,
                },
            )
            .await
            .expect("update redemption");
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn error_status_returns_message() {
        let server = MockServer::start_async().await;
        let base = Url::parse(&server.url("/helix/")).expect("url");
        let client = client(&base);

        server
            .mock_async(|when, then| {
                when.method(GET)
                    .path("/helix/channel_points/custom_rewards/redemptions");
                then.status(401).body("unauthorized");
            })
            .await;

        let err = client
            .list_redemptions(
                "token",
                &ListRedemptionsParams {
                    broadcaster_id: "b-1",
                    reward_id: None,
                    status: HelixRedemptionStatus::Unfulfilled,
                    after: None,
                    first: None,
                },
            )
            .await
            .expect_err("should error");
        match err {
            HelixError::Status { status, body } => {
                assert_eq!(status, StatusCode::UNAUTHORIZED);
                assert_eq!(body, "unauthorized");
            }
            other => panic!("unexpected error: {:?}", other),
        }
    }
}
