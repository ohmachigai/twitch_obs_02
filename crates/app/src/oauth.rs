use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::{IntoResponse, Redirect, Response},
    Json,
};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use chrono::{DateTime, Duration, Utc};
use metrics::counter;
use rand::{rngs::OsRng, RngCore};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use tracing::{error, warn};
use twi_overlay_storage::{
    NewOauthLink, NewOauthLoginState, OauthFailure, OauthLink, OauthLoginState, OauthTokenUpdate,
    OauthValidationResult, StateIndexError,
};
use twi_overlay_twitch::{AuthorizeUrlParams, OAuthError, TokenResponse, ValidateTokenResponse};
use ulid::Ulid;
use url::form_urlencoded;
use uuid::Uuid;

#[cfg(test)]
use std::time::Duration as StdDuration;
#[cfg(test)]
use twi_overlay_twitch::HelixClient;

use crate::problem::ProblemResponse;
use crate::router::AppState;
use crate::tap::{StageEvent, StageKind, StageMetadata, StagePayload};

const OAUTH_SCOPES: &[&str] = &["channel:read:redemptions", "channel:manage:redemptions"];
const MANAGED_SCOPES: &[&str] = &["channel:read:redemptions", "channel:manage:redemptions"];
const REDIRECT_ALLOWLIST: &[&str] = &["/admin", "/overlay"];
const DEFAULT_SUCCESS_REDIRECT: &str = "/admin/oauth/success";
const ERROR_REDIRECT_PATH: &str = "/admin/oauth/error";
const REFRESH_LEEWAY_SECS: i64 = 300;
const CODE_VERIFIER_LEN: usize = 64;

#[derive(Debug, Deserialize)]
pub struct LoginQuery {
    pub broadcaster: String,
    #[serde(default)]
    pub redirect_to: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct CallbackQuery {
    pub state: Option<String>,
    pub code: Option<String>,
    pub error: Option<String>,
    #[serde(default)]
    pub error_description: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ValidateRequest {
    pub broadcaster: String,
    #[serde(default)]
    pub force: bool,
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum ValidateStatus {
    Ok,
    Refresh,
    Reauth,
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ValidateResponse {
    status: ValidateStatus,
    managed_rewards: Vec<String>,
    next_check_at: Option<DateTime<Utc>>,
}

pub async fn login(
    State(state): State<AppState>,
    Query(params): Query<LoginQuery>,
) -> Result<Response, ProblemResponse> {
    ensure_broadcaster(&state, &params.broadcaster).await?;

    if let Some(ref redirect) = params.redirect_to {
        if !is_redirect_allowed(redirect) {
            return Err(ProblemResponse::new(
                StatusCode::BAD_REQUEST,
                "invalid_redirect",
                "redirect_to is not permitted",
            ));
        }
    }

    let now = state.now();
    let login_repo = state.storage().oauth_login_states();
    let has_active = login_repo
        .has_active(&params.broadcaster, now)
        .await
        .map_err(|err| {
            error!(
                stage = "oauth",
                broadcaster = %params.broadcaster,
                error = %err,
                "failed to query active oauth login state"
            );
            internal_error("failed to evaluate existing OAuth state")
        })?;

    if has_active {
        return Err(ProblemResponse::new(
            StatusCode::CONFLICT,
            "oauth_state_active",
            "an OAuth login is already in progress",
        ));
    }

    let state_value = Ulid::new().to_string();
    let code_verifier = generate_code_verifier();
    let code_challenge = compute_code_challenge(&code_verifier);
    let login_hint = existing_login_hint(&state, &params.broadcaster).await?;

    let authorize_url = state
        .oauth_client()
        .authorize_url(&AuthorizeUrlParams {
            state: &state_value,
            redirect_uri: state.oauth_redirect_uri(),
            code_challenge: &code_challenge,
            scopes: OAUTH_SCOPES,
            login_hint: login_hint.as_deref(),
        })
        .map_err(|err| {
            error!(stage = "oauth", error = %err, "failed to build authorize url");
            internal_error("failed to build Twitch authorize URL")
        })?;

    let expires_at = now
        + Duration::from_std(state.oauth_state_ttl()).map_err(|err| {
            error!(stage = "oauth", error = %err, "invalid oauth state ttl");
            internal_error("invalid OAuth state TTL")
        })?;

    login_repo
        .insert(&NewOauthLoginState {
            state: state_value.clone(),
            broadcaster_id: &params.broadcaster,
            code_verifier,
            redirect_to: params.redirect_to.clone(),
            created_at: now,
            expires_at,
        })
        .await
        .map_err(|err| {
            error!(stage = "oauth", error = %err, "failed to persist oauth login state");
            internal_error("failed to persist OAuth login state")
        })?;

    publish_oauth_event(
        &state,
        now,
        &params.broadcaster,
        "oauth.login.start",
        json!({ "redirect": params.redirect_to }),
    );

    Ok(redirect_found(authorize_url.as_str()))
}

pub async fn callback(
    State(state): State<AppState>,
    Query(query): Query<CallbackQuery>,
) -> Result<Response, ProblemResponse> {
    let Some(state_param) = query.state.as_deref() else {
        return Ok(error_redirect(
            "missing_state",
            None,
            query.error_description.as_deref(),
        ));
    };

    let maybe_login_state = consume_login_state(&state, state_param).await?;
    let Some(login_state) = maybe_login_state else {
        return Ok(error_redirect("state_not_found", None, None));
    };

    let now = state.now();
    if login_state.expires_at <= now {
        warn!(
            stage = "oauth",
            broadcaster = %login_state.broadcaster_id,
            "received expired oauth state"
        );
        return Ok(error_redirect("state_expired", Some(&login_state), None));
    }

    if let Some(error_code) = query.error.as_deref() {
        warn!(
            stage = "oauth",
            broadcaster = %login_state.broadcaster_id,
            error = error_code,
            description = query.error_description.as_deref(),
            "oauth callback returned error"
        );
        publish_oauth_event(
            &state,
            now,
            &login_state.broadcaster_id,
            "oauth.callback.error",
            json!({
                "error": error_code,
                "description": query.error_description,
            }),
        );
        return Ok(error_redirect(
            error_code,
            Some(&login_state),
            query.error_description.as_deref(),
        ));
    }

    let code = match query.code.as_deref() {
        Some(code) if !code.is_empty() => code,
        _ => {
            warn!(
                stage = "oauth",
                broadcaster = %login_state.broadcaster_id,
                "oauth callback missing code"
            );
            return Ok(error_redirect("missing_code", Some(&login_state), None));
        }
    };

    let token_response = match state
        .oauth_client()
        .exchange_code(code, &login_state.code_verifier, state.oauth_redirect_uri())
        .await
    {
        Ok(token) => token,
        Err(err) => {
            let reason = format_error_code(&err);
            warn!(
                stage = "oauth",
                broadcaster = %login_state.broadcaster_id,
                error = %err,
                "failed to exchange authorization code"
            );
            publish_oauth_event(
                &state,
                now,
                &login_state.broadcaster_id,
                "oauth.callback.exchange_failed",
                json!({ "reason": reason }),
            );
            return Ok(error_redirect("exchange_failed", Some(&login_state), None));
        }
    };

    let validation = match state
        .oauth_client()
        .validate_token(&token_response.access_token)
        .await
    {
        Ok(meta) => meta,
        Err(err) => {
            let reason = format_error_code(&err);
            warn!(
                stage = "oauth",
                broadcaster = %login_state.broadcaster_id,
                error = %err,
                "failed to validate oauth token"
            );
            publish_oauth_event(
                &state,
                now,
                &login_state.broadcaster_id,
                "oauth.callback.validate_failed",
                json!({ "reason": reason }),
            );
            return Ok(error_redirect("validate_failed", Some(&login_state), None));
        }
    };

    if let Some(missing) = missing_required_scope(&validation.scopes) {
        warn!(
            stage = "oauth",
            broadcaster = %login_state.broadcaster_id,
            missing = %missing,
            "token missing required scope"
        );
        publish_oauth_event(
            &state,
            now,
            &login_state.broadcaster_id,
            "oauth.callback.missing_scope",
            json!({ "missing": missing }),
        );
        return Ok(error_redirect("missing_scope", Some(&login_state), None));
    }

    let Some(refresh_token) = token_response.refresh_token.clone() else {
        warn!(
            stage = "oauth",
            broadcaster = %login_state.broadcaster_id,
            "twitch response omitted refresh token"
        );
        return Ok(error_redirect(
            "missing_refresh_token",
            Some(&login_state),
            None,
        ));
    };

    persist_link(
        &state,
        &login_state,
        &token_response,
        &refresh_token,
        &validation,
        now,
    )
    .await?;

    publish_oauth_event(
        &state,
        now,
        &login_state.broadcaster_id,
        "oauth.callback.success",
        json!({
            "twitch_user_id": validation.user_id,
            "scopes": validation.scopes,
        }),
    );

    let redirect_target = login_state
        .redirect_to
        .unwrap_or_else(|| DEFAULT_SUCCESS_REDIRECT.to_string());
    Ok(redirect_found(&redirect_target))
}

pub async fn validate(
    State(state): State<AppState>,
    Json(body): Json<ValidateRequest>,
) -> Result<Json<ValidateResponse>, ProblemResponse> {
    ensure_broadcaster(&state, &body.broadcaster).await?;

    let Some(link) = state
        .storage()
        .oauth_links()
        .fetch_by_broadcaster(&body.broadcaster)
        .await
        .map_err(|err| {
            error!(stage = "oauth", error = %err, "failed to fetch oauth link");
            internal_error("failed to load OAuth link")
        })?
    else {
        return Err(ProblemResponse::new(
            StatusCode::NOT_FOUND,
            "oauth_link_not_found",
            "OAuth link has not been established",
        ));
    };

    if link.requires_reauth {
        publish_oauth_event(
            &state,
            state.now(),
            &body.broadcaster,
            "oauth.validate.reauth_required",
            json!({ "reason": "flagged" }),
        );
        return Ok(Json(ValidateResponse {
            status: ValidateStatus::Reauth,
            managed_rewards: Vec::new(),
            next_check_at: Some(link.expires_at),
        }));
    }

    let refresh_deadline = state.now() + Duration::seconds(REFRESH_LEEWAY_SECS);
    let should_refresh = body.force || link.expires_at <= refresh_deadline;

    if should_refresh {
        handle_refresh(&state, &body.broadcaster, link).await
    } else {
        handle_validation(&state, &body.broadcaster, link).await
    }
}

async fn handle_refresh(
    state: &AppState,
    broadcaster: &str,
    link: OauthLink,
) -> Result<Json<ValidateResponse>, ProblemResponse> {
    let token_response = match state
        .oauth_client()
        .refresh_token(&link.refresh_token)
        .await
    {
        Ok(token) => token,
        Err(err) => {
            let reason = format_error_code(&err);
            let requires_reauth = should_require_reauth(&err);
            if record_failure(state, broadcaster, &link, &reason, requires_reauth)
                .await
                .is_err()
            {
                error!(stage = "oauth", "failed to record refresh failure");
            }
            counter!("oauth_refresh_total", "result" => "failed").increment(1);
            counter!("oauth_validate_failures_total").increment(1);
            if requires_reauth {
                publish_oauth_event(
                    state,
                    state.now(),
                    broadcaster,
                    "oauth.refresh.reauth_required",
                    json!({ "reason": reason }),
                );
                return Ok(Json(ValidateResponse {
                    status: ValidateStatus::Reauth,
                    managed_rewards: Vec::new(),
                    next_check_at: Some(link.expires_at),
                }));
            }
            return Err(ProblemResponse::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "oauth_refresh_failed",
                "failed to refresh OAuth token",
            ));
        }
    };

    let validation = match state
        .oauth_client()
        .validate_token(&token_response.access_token)
        .await
    {
        Ok(meta) => meta,
        Err(err) => {
            let reason = format_error_code(&err);
            let requires_reauth = should_require_reauth(&err);
            if record_failure(state, broadcaster, &link, &reason, requires_reauth)
                .await
                .is_err()
            {
                error!(stage = "oauth", "failed to record validation failure");
            }
            counter!("oauth_refresh_total", "result" => "failed").increment(1);
            counter!("oauth_validate_failures_total").increment(1);
            if requires_reauth {
                publish_oauth_event(
                    state,
                    state.now(),
                    broadcaster,
                    "oauth.refresh.reauth_required",
                    json!({ "reason": reason }),
                );
                return Ok(Json(ValidateResponse {
                    status: ValidateStatus::Reauth,
                    managed_rewards: Vec::new(),
                    next_check_at: Some(link.expires_at),
                }));
            }
            return Err(ProblemResponse::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "oauth_validate_failed",
                "failed to validate refreshed token",
            ));
        }
    };

    if let Some(missing) = missing_required_scope(&validation.scopes) {
        if record_failure(state, broadcaster, &link, missing, true)
            .await
            .is_err()
        {
            error!(stage = "oauth", "failed to record missing scope");
        }
        counter!("oauth_refresh_total", "result" => "failed").increment(1);
        counter!("oauth_validate_failures_total").increment(1);
        publish_oauth_event(
            state,
            state.now(),
            broadcaster,
            "oauth.refresh.missing_scope",
            json!({ "missing": missing }),
        );
        return Ok(Json(ValidateResponse {
            status: ValidateStatus::Reauth,
            managed_rewards: Vec::new(),
            next_check_at: Some(link.expires_at),
        }));
    }

    let refreshed_at = state.now();
    let expires_at = token_response.expires_at(refreshed_at);
    let refresh_token = token_response
        .refresh_token
        .clone()
        .unwrap_or_else(|| link.refresh_token.clone());

    let storage = state.storage().clone();
    let command_repo = storage.command_log();
    let mut tx = command_repo.begin().await.map_err(|err| {
        error!(stage = "oauth", error = %err, "failed to begin transaction");
        internal_error("failed to begin transaction")
    })?;

    let updated = storage
        .oauth_links()
        .update_tokens(
            &mut tx,
            &OauthTokenUpdate {
                broadcaster_id: broadcaster,
                twitch_user_id: link.twitch_user_id.clone(),
                access_token: token_response.access_token.clone(),
                refresh_token,
                expires_at,
                scopes: validation.scopes.clone(),
                managed_scopes: managed_scopes(&validation.scopes),
                refreshed_at,
                validated_at: refreshed_at,
                updated_at: refreshed_at,
            },
        )
        .await
        .map_err(|err| {
            error!(stage = "oauth", error = %err, "failed to update oauth tokens");
            internal_error("failed to persist refreshed tokens")
        })?;

    tx.commit().await.map_err(|err| {
        error!(stage = "oauth", error = %err, "failed to commit token update");
        internal_error("failed to commit refreshed tokens")
    })?;

    counter!("oauth_refresh_total", "result" => "success").increment(1);
    publish_oauth_event(
        state,
        refreshed_at,
        broadcaster,
        "oauth.refresh.success",
        json!({ "expires_at": updated.expires_at }),
    );

    notify_backfill(state, broadcaster).await;

    Ok(Json(ValidateResponse {
        status: ValidateStatus::Refresh,
        managed_rewards: Vec::new(),
        next_check_at: Some(updated.expires_at),
    }))
}

async fn handle_validation(
    state: &AppState,
    broadcaster: &str,
    link: OauthLink,
) -> Result<Json<ValidateResponse>, ProblemResponse> {
    let validation = match state
        .oauth_client()
        .validate_token(&link.access_token)
        .await
    {
        Ok(meta) => meta,
        Err(err) => {
            let reason = format_error_code(&err);
            let requires_reauth = should_require_reauth(&err);
            if record_failure(state, broadcaster, &link, &reason, requires_reauth)
                .await
                .is_err()
            {
                error!(stage = "oauth", %reason, requires_reauth, "failed to record validation failure");
            }
            counter!("oauth_validate_failures_total").increment(1);
            if requires_reauth {
                publish_oauth_event(
                    state,
                    state.now(),
                    broadcaster,
                    "oauth.validate.reauth_required",
                    json!({ "reason": reason }),
                );
                return Ok(Json(ValidateResponse {
                    status: ValidateStatus::Reauth,
                    managed_rewards: Vec::new(),
                    next_check_at: Some(link.expires_at),
                }));
            }
            return Err(ProblemResponse::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "oauth_validate_failed",
                "failed to validate OAuth token",
            ));
        }
    };

    if let Some(missing) = missing_required_scope(&validation.scopes) {
        if record_failure(state, broadcaster, &link, missing, true)
            .await
            .is_err()
        {
            error!(stage = "oauth", missing, "failed to record missing scope");
        }
        counter!("oauth_validate_failures_total").increment(1);
        publish_oauth_event(
            state,
            state.now(),
            broadcaster,
            "oauth.validate.missing_scope",
            json!({ "missing": missing }),
        );
        return Ok(Json(ValidateResponse {
            status: ValidateStatus::Reauth,
            managed_rewards: Vec::new(),
            next_check_at: Some(link.expires_at),
        }));
    }

    let validated_at = state.now();
    let storage = state.storage().clone();
    let command_repo = storage.command_log();
    let mut tx = command_repo.begin().await.map_err(|err| {
        error!(stage = "oauth", error = %err, "failed to begin validation transaction");
        internal_error("failed to begin validation transaction")
    })?;

    storage
        .oauth_links()
        .mark_validation_result(
            &mut tx,
            &OauthValidationResult {
                broadcaster_id: broadcaster,
                twitch_user_id: link.twitch_user_id.clone(),
                validated_at,
                requires_reauth: false,
                failure: None,
            },
        )
        .await
        .map_err(|err| {
            error!(stage = "oauth", error = %err, "failed to mark validation result");
            internal_error("failed to record validation result")
        })?;

    tx.commit().await.map_err(|err| {
        error!(stage = "oauth", error = %err, "failed to commit validation result");
        internal_error("failed to commit validation result")
    })?;

    counter!("oauth_refresh_total", "result" => "skipped").increment(1);
    publish_oauth_event(
        state,
        validated_at,
        broadcaster,
        "oauth.validate.success",
        json!({ "expires_at": link.expires_at }),
    );

    notify_backfill(state, broadcaster).await;

    Ok(Json(ValidateResponse {
        status: ValidateStatus::Ok,
        managed_rewards: link.managed_scopes,
        next_check_at: Some(link.expires_at),
    }))
}

async fn notify_backfill(state: &AppState, broadcaster: &str) {
    if let Err(err) = state.backfill().trigger(broadcaster.to_string()).await {
        warn!(
            stage = "oauth",
            broadcaster = %broadcaster,
            error = %err,
            "failed to trigger helix backfill"
        );
    }
}

pub(crate) async fn ensure_broadcaster(
    state: &AppState,
    broadcaster: &str,
) -> Result<(), ProblemResponse> {
    match state
        .storage()
        .state_index()
        .fetch_current_version(broadcaster)
        .await
    {
        Ok(_) => Ok(()),
        Err(StateIndexError::MissingBroadcaster) => Err(ProblemResponse::new(
            StatusCode::BAD_REQUEST,
            "unknown_broadcaster",
            "broadcaster is not provisioned",
        )),
        Err(err) => {
            error!(stage = "oauth", error = %err, "failed to query state index");
            Err(internal_error("failed to query broadcaster state"))
        }
    }
}

async fn existing_login_hint(
    state: &AppState,
    broadcaster: &str,
) -> Result<Option<String>, ProblemResponse> {
    let link = state
        .storage()
        .oauth_links()
        .fetch_by_broadcaster(broadcaster)
        .await
        .map_err(|err| {
            error!(stage = "oauth", error = %err, "failed to fetch existing oauth link");
            internal_error("failed to load OAuth link")
        })?;

    Ok(link.map(|link| link.twitch_user_id))
}

async fn consume_login_state(
    state: &AppState,
    state_value: &str,
) -> Result<Option<OauthLoginState>, ProblemResponse> {
    state
        .storage()
        .oauth_login_states()
        .consume(state_value)
        .await
        .map_err(|err| {
            error!(stage = "oauth", error = %err, "failed to consume oauth state");
            internal_error("failed to load OAuth state")
        })
}

async fn persist_link(
    state: &AppState,
    login_state: &OauthLoginState,
    token_response: &TokenResponse,
    refresh_token: &str,
    validation: &ValidateTokenResponse,
    now: DateTime<Utc>,
) -> Result<(), ProblemResponse> {
    let storage = state.storage().clone();
    let command_repo = storage.command_log();
    let mut tx = command_repo.begin().await.map_err(|err| {
        error!(stage = "oauth", error = %err, "failed to begin transaction");
        internal_error("failed to begin transaction")
    })?;

    let expires_at = token_response.expires_at(now);
    storage
        .oauth_links()
        .upsert_link(
            &mut tx,
            &NewOauthLink {
                id: Uuid::new_v4().to_string(),
                broadcaster_id: &login_state.broadcaster_id,
                twitch_user_id: validation.user_id.clone(),
                scopes: validation.scopes.clone(),
                managed_scopes: managed_scopes(&validation.scopes),
                access_token: token_response.access_token.clone(),
                refresh_token: refresh_token.to_string(),
                expires_at,
                created_at: now,
                updated_at: now,
            },
        )
        .await
        .map_err(|err| {
            error!(stage = "oauth", error = %err, "failed to upsert oauth link");
            internal_error("failed to persist OAuth link")
        })?;

    tx.commit().await.map_err(|err| {
        error!(stage = "oauth", error = %err, "failed to commit oauth link");
        internal_error("failed to persist OAuth link")
    })
}

async fn record_failure(
    state: &AppState,
    broadcaster: &str,
    link: &OauthLink,
    reason: &str,
    requires_reauth: bool,
) -> Result<(), ProblemResponse> {
    let storage = state.storage().clone();
    let command_repo = storage.command_log();
    let mut tx = command_repo.begin().await.map_err(|err| {
        error!(stage = "oauth", error = %err, "failed to begin failure transaction");
        internal_error("failed to begin transaction")
    })?;

    let occurred_at = state.now();
    storage
        .oauth_links()
        .mark_failure(
            &mut tx,
            &OauthFailure {
                broadcaster_id: broadcaster,
                twitch_user_id: link.twitch_user_id.clone(),
                occurred_at,
                reason,
                requires_reauth,
            },
        )
        .await
        .map_err(|err| {
            error!(stage = "oauth", error = %err, "failed to mark oauth failure");
            internal_error("failed to record OAuth failure")
        })?;

    tx.commit().await.map_err(|err| {
        error!(stage = "oauth", error = %err, "failed to commit oauth failure");
        internal_error("failed to record OAuth failure")
    })
}

fn redirect_found(location: &str) -> Response {
    let mut response = Redirect::temporary(location).into_response();
    *response.status_mut() = StatusCode::FOUND;
    response
}

fn error_redirect(reason: &str, state: Option<&OauthLoginState>, detail: Option<&str>) -> Response {
    let mut serializer = form_urlencoded::Serializer::new(String::new());
    serializer.append_pair("reason", reason);
    if let Some(login_state) = state {
        serializer.append_pair("broadcaster", &login_state.broadcaster_id);
    }
    if let Some(value) = detail {
        serializer.append_pair("detail", value);
    }
    let location = format!("{ERROR_REDIRECT_PATH}?{}", serializer.finish());
    redirect_found(&location)
}

fn is_redirect_allowed(value: &str) -> bool {
    if !value.starts_with('/') || value.starts_with("//") || value.contains("://") {
        return false;
    }
    if value.split('/').any(|segment| segment == "..") {
        return false;
    }
    let path = value.split('?').next().unwrap_or(value);
    REDIRECT_ALLOWLIST
        .iter()
        .any(|prefix| path == *prefix || path.starts_with(&format!("{prefix}/")))
}

fn generate_code_verifier() -> String {
    let mut bytes = [0u8; CODE_VERIFIER_LEN];
    OsRng.fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

fn compute_code_challenge(verifier: &str) -> String {
    let digest = Sha256::digest(verifier.as_bytes());
    URL_SAFE_NO_PAD.encode(digest)
}

fn missing_required_scope(scopes: &[String]) -> Option<&'static str> {
    OAUTH_SCOPES
        .iter()
        .find(|scope| !scopes.iter().any(|item| item == **scope))
        .copied()
}

fn managed_scopes(scopes: &[String]) -> Vec<String> {
    scopes
        .iter()
        .filter(|scope| MANAGED_SCOPES.contains(&scope.as_str()))
        .cloned()
        .collect()
}

fn should_require_reauth(err: &OAuthError) -> bool {
    matches!(
        err,
        OAuthError::Status { status, .. }
            if *status == StatusCode::UNAUTHORIZED || *status == StatusCode::BAD_REQUEST
    )
}

fn format_error_code(err: &OAuthError) -> String {
    match err {
        OAuthError::Status { status, .. } => status.as_u16().to_string(),
        other => other.to_string(),
    }
}

fn publish_oauth_event(
    state: &AppState,
    timestamp: DateTime<Utc>,
    broadcaster: &str,
    message: &str,
    payload: serde_json::Value,
) {
    state.tap().publish(StageEvent {
        ts: timestamp,
        stage: StageKind::Oauth,
        trace_id: None,
        op_id: None,
        version: None,
        broadcaster_id: Some(broadcaster.to_string()),
        meta: StageMetadata {
            message: Some(message.to_string()),
            ..StageMetadata::default()
        },
        r#in: StagePayload::default(),
        out: StagePayload {
            redacted: true,
            payload,
            truncated: None,
        },
    });
}

fn internal_error(message: impl Into<String>) -> ProblemResponse {
    ProblemResponse::new(StatusCode::INTERNAL_SERVER_ERROR, "internal_error", message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::Body,
        http::{header, Request, StatusCode},
        routing::{get, post},
        Router,
    };
    use chrono::{DateTime, Duration, Utc};
    use http_body_util::BodyExt;
    use reqwest::Client;
    use serde_json::json;
    use sqlx::query;
    use std::sync::Arc;
    use tower::ServiceExt;
    use twi_overlay_storage::Database;
    use twi_overlay_twitch::TwitchOAuthClient;
    use url::Url;

    use crate::tap::TapHub;
    use crate::telemetry;

    const BROADCASTER_ID: &str = "b-1";

    #[tokio::test]
    async fn login_creates_state_and_redirects() {
        let context = TestContext::new().await;
        let app = context.router();

        let response = app
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/oauth/login?broadcaster=b-1&redirect_to=/admin")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::FOUND);
        let location = response
            .headers()
            .get(header::LOCATION)
            .expect("location header")
            .to_str()
            .unwrap();
        assert!(location.starts_with("https://id.twitch.tv/oauth2/authorize"));

        let params: Vec<(String, String)> =
            url::form_urlencoded::parse(location.split('?').nth(1).unwrap().as_bytes())
                .into_owned()
                .collect();
        assert!(params.iter().any(|(k, _)| k == "state"));
        assert!(params.iter().any(|(k, _)| k == "code_challenge"));

        let state_value = params
            .into_iter()
            .find(|(k, _)| k == "state")
            .map(|(_, v)| v)
            .expect("state param");
        let stored = context
            .database
            .oauth_login_states()
            .consume(&state_value)
            .await
            .unwrap()
            .expect("state present");
        assert_eq!(stored.broadcaster_id, BROADCASTER_ID);
        assert_eq!(stored.redirect_to.as_deref(), Some("/admin"));
    }

    #[tokio::test]
    async fn login_rejects_invalid_redirect() {
        let context = TestContext::new().await;
        let app = context.router();

        let response = app
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/oauth/login?broadcaster=b-1&redirect_to=http://evil.test")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn callback_persists_tokens() {
        let context = TestContext::with_mock().await;
        let state_value = context
            .insert_login_state(Some("/admin/oauth/success"))
            .await;
        context.mock_exchange_success();

        let app = context.router();
        let response = app
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri(format!("/oauth/callback?state={state_value}&code=test"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::FOUND);
        assert_eq!(
            response.headers().get(header::LOCATION).unwrap(),
            "/admin/oauth/success"
        );

        let link = context
            .database
            .oauth_links()
            .fetch_by_broadcaster(BROADCASTER_ID)
            .await
            .unwrap()
            .expect("link present");
        assert_eq!(link.twitch_user_id, "user-123");
        assert_eq!(link.managed_scopes.len(), 2);
    }

    #[tokio::test]
    async fn validate_refresh_path_updates_tokens() {
        let context = TestContext::with_mock().await;
        context.insert_oauth_link(Duration::seconds(30)).await;
        context.mock_refresh_success();

        let app = context.router();
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/oauth2/validate")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from("{\"broadcaster\":\"b-1\"}"))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body_bytes = response.into_body().collect().await.unwrap().to_bytes();
        let payload: ValidateResponse = serde_json::from_slice(&body_bytes).unwrap();
        assert_eq!(payload.status, ValidateStatus::Refresh);

        let link = context
            .database
            .oauth_links()
            .fetch_by_broadcaster(BROADCASTER_ID)
            .await
            .unwrap()
            .expect("link present");
        assert!(link.last_refreshed_at.is_some());
    }

    struct TestContext {
        database: Database,
        state: AppState,
        now: DateTime<Utc>,
        mock_server: Option<httpmock::MockServer>,
    }

    impl TestContext {
        async fn new() -> Self {
            Self::init(None).await
        }

        async fn with_mock() -> Self {
            let server = httpmock::MockServer::start();
            Self::init(Some(server)).await
        }

        async fn init(mock_server: Option<httpmock::MockServer>) -> Self {
            let metrics = telemetry::init_metrics().expect("metrics");
            let tap = TapHub::new();
            let database = Database::connect("sqlite::memory:?cache=shared")
                .await
                .expect("connect");
            database.run_migrations().await.expect("migrations");

            query(
                "INSERT INTO broadcasters (id, twitch_broadcaster_id, display_name, timezone, settings_json, created_at, updated_at) \
                 VALUES (?, ?, ?, ?, '{}', ?, ?)",
            )
            .bind(BROADCASTER_ID)
            .bind("twitch-".to_string() + BROADCASTER_ID)
            .bind("Broadcaster")
            .bind("UTC")
            .bind("2024-01-01T00:00:00Z")
            .bind("2024-01-01T00:00:00Z")
            .execute(database.pool())
            .await
            .expect("insert broadcaster");

            query(
                "INSERT INTO state_index (broadcaster_id, current_version, updated_at) VALUES (?, 0, '2024-01-01T00:00:00Z')",
            )
            .bind(BROADCASTER_ID)
            .execute(database.pool())
            .await
            .expect("insert state index");

            let now = DateTime::parse_from_rfc3339("2024-01-01T00:00:00Z")
                .unwrap()
                .with_timezone(&Utc);
            let clock = Arc::new(move || now);

            let oauth_base = mock_server
                .as_ref()
                .map(|server| format!("{}/", server.base_url()))
                .unwrap_or_else(|| "https://id.twitch.tv/oauth2/".to_string());
            let http = Client::builder().build().expect("client");
            let oauth_client = TwitchOAuthClient::new(
                "client",
                "secret",
                Url::parse(&oauth_base).expect("url"),
                http.clone(),
            );
            let helix_client = HelixClient::new(
                "client",
                Url::parse("https://api.twitch.tv/helix/").expect("url"),
                http,
            );

            let (state, _worker) = AppState::new(
                metrics,
                tap,
                database.clone(),
                Arc::from(b"secret".to_vec().into_boxed_slice()),
                b"token-secret".to_vec(),
                64,
                StdDuration::from_secs(60),
                25,
                helix_client,
                oauth_client,
                "http://localhost/oauth/callback".to_string(),
                StdDuration::from_secs(600),
                StdDuration::from_secs(300),
                50,
            );
            let state = state.with_clock(clock);

            Self {
                database,
                state,
                now,
                mock_server,
            }
        }

        fn router(&self) -> Router {
            Router::new()
                .route("/oauth/login", get(super::login))
                .route("/oauth/callback", get(super::callback))
                .route("/oauth2/validate", post(super::validate))
                .with_state(self.state.clone())
        }

        async fn insert_login_state(&self, redirect: Option<&str>) -> String {
            let state_value = "state-1".to_string();
            self.database
                .oauth_login_states()
                .insert(&NewOauthLoginState {
                    state: state_value.clone(),
                    broadcaster_id: BROADCASTER_ID,
                    code_verifier: "verifier".into(),
                    redirect_to: redirect.map(|value| value.to_string()),
                    created_at: self.now,
                    expires_at: self.now + Duration::minutes(10),
                })
                .await
                .expect("insert state");
            state_value
        }

        async fn insert_oauth_link(&self, expires_in: Duration) {
            let command_repo = self.database.command_log();
            let mut tx = command_repo.begin().await.expect("begin tx");
            self.database
                .oauth_links()
                .upsert_link(
                    &mut tx,
                    &NewOauthLink {
                        id: "link-1".into(),
                        broadcaster_id: BROADCASTER_ID,
                        twitch_user_id: "user-1".into(),
                        scopes: vec![
                            "channel:manage:redemptions".into(),
                            "channel:read:redemptions".into(),
                        ],
                        managed_scopes: vec![
                            "channel:manage:redemptions".into(),
                            "channel:read:redemptions".into(),
                        ],
                        access_token: "access".into(),
                        refresh_token: "refresh".into(),
                        expires_at: self.now + expires_in,
                        created_at: self.now,
                        updated_at: self.now,
                    },
                )
                .await
                .expect("insert link");
            tx.commit().await.expect("commit");
        }

        fn mock_exchange_success(&self) {
            let server = self.mock_server.as_ref().expect("mock server");
            server.mock(|when, then| {
                when.method("POST").path("/token");
                then.status(200)
                    .header("content-type", "application/json")
                    .body(
                        json!({
                            "access_token": "new-access",
                            "refresh_token": "new-refresh",
                            "expires_in": 3600,
                            "scope": OAUTH_SCOPES,
                            "token_type": "bearer"
                        })
                        .to_string(),
                    );
            });
            server.mock(|when, then| {
                when.method("GET").path("/validate");
                then.status(200)
                    .header("content-type", "application/json")
                    .body(
                        json!({
                            "client_id": "client",
                            "login": "broadcaster",
                            "scopes": OAUTH_SCOPES,
                            "user_id": "user-123",
                            "expires_in": 3600
                        })
                        .to_string(),
                    );
            });
        }

        fn mock_refresh_success(&self) {
            let server = self.mock_server.as_ref().expect("mock server");
            server.mock(|when, then| {
                when.method("POST").path("/token");
                then.status(200)
                    .header("content-type", "application/json")
                    .body(
                        json!({
                            "access_token": "refresh-access",
                            "refresh_token": "refresh-refresh",
                            "expires_in": 3600,
                            "scope": OAUTH_SCOPES,
                            "token_type": "bearer"
                        })
                        .to_string(),
                    );
            });
            server.mock(|when, then| {
                when.method("GET").path("/validate");
                then.status(200)
                    .header("content-type", "application/json")
                    .body(
                        json!({
                            "client_id": "client",
                            "login": "broadcaster",
                            "scopes": OAUTH_SCOPES,
                            "user_id": "user-123",
                            "expires_in": 3600
                        })
                        .to_string(),
                    );
            });
        }
    }
}
