//! Who may use the web UI and its API: GitHub users on an allowlist.
//!
//! Browsers log in through GitHub OAuth and get a signed session cookie.
//! Scripts and agents send a GitHub token, e.g. `gh auth token`, as a bearer
//! token instead.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Context;
use axum::extract::{FromRequestParts, Query, State};
use axum::http::request::Parts;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use hmac::{KeyInit, Mac};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::Server;
use super::config::{GithubConfig, WebConfig, read_secret};
use super::metrics::GithubMetrics;

const SESSION_COOKIE: &str = "amphi_session";
const STATE_COOKIE: &str = "amphi_oauth_state";
const SESSION_LIFETIME: Duration = Duration::from_secs(30 * 24 * 3600);
/// How long a verified bearer token is trusted before GitHub is asked again.
const TOKEN_CACHE: Duration = Duration::from_secs(300);

/// Logins of recently verified bearer tokens, by the token's SHA-256.
type TokenCache = HashMap<[u8; 32], (Option<String>, Instant)>;

/// Session cookies: a login and an expiry, signed with a server key.
struct Sessions {
    key: Vec<u8>,
}

impl Sessions {
    fn sign(&self, payload: &str) -> String {
        let mut mac = hmac::Hmac::<Sha256>::new_from_slice(&self.key)
            .expect("HMAC accepts keys of any length");
        mac.update(payload.as_bytes());
        URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes())
    }

    fn create(&self, login: &str) -> String {
        let expires = jiff::Timestamp::now().as_second() + SESSION_LIFETIME.as_secs() as i64;
        let payload = format!("{login}:{expires}");
        format!(
            "{}.{}",
            URL_SAFE_NO_PAD.encode(&payload),
            self.sign(&payload)
        )
    }

    fn login(&self, cookie: &str) -> Option<String> {
        let (payload, signature) = cookie.split_once('.')?;
        let payload = String::from_utf8(URL_SAFE_NO_PAD.decode(payload).ok()?).ok()?;
        let mut mac = hmac::Hmac::<Sha256>::new_from_slice(&self.key)
            .expect("HMAC accepts keys of any length");
        mac.update(payload.as_bytes());
        mac.verify_slice(&URL_SAFE_NO_PAD.decode(signature).ok()?)
            .ok()?;
        let (login, expires) = payload.split_once(':')?;
        (expires.parse::<i64>().ok()? > jiff::Timestamp::now().as_second())
            .then(|| login.to_owned())
    }
}

pub struct Auth {
    client_id: String,
    client_secret: String,
    sessions: Sessions,
    users: HashSet<String>,
    github_url: String,
    api_url: String,
    callback_url: String,
    secure_cookies: bool,
    http: reqwest::Client,
    metrics: GithubMetrics,
    tokens: Mutex<TokenCache>,
}

impl Auth {
    pub fn new(
        web: &WebConfig,
        github: &GithubConfig,
        public_url: &str,
        http: reqwest::Client,
        metrics: GithubMetrics,
    ) -> anyhow::Result<Self> {
        let public_url = public_url.trim_end_matches('/');
        let github_url = github.url.trim_end_matches('/');
        reqwest::Url::parse(github_url)
            .with_context(|| format!("invalid GitHub URL {github_url:?}"))?;
        Ok(Self {
            client_id: web.client_id.clone(),
            client_secret: read_secret(&web.client_secret_file)?,
            sessions: Sessions {
                key: read_secret(&web.session_key_file)?.into_bytes(),
            },
            users: web.users.iter().map(|user| user.to_lowercase()).collect(),
            github_url: github_url.to_owned(),
            api_url: github.api_url.trim_end_matches('/').to_owned(),
            callback_url: format!("{public_url}/api/auth/callback"),
            secure_cookies: public_url.starts_with("https://"),
            http,
            metrics,
            tokens: Mutex::default(),
        })
    }

    /// Sends a request to GitHub and records it like the API client does.
    async fn send(
        &self,
        operation: &'static str,
        request: reqwest::RequestBuilder,
    ) -> reqwest::Result<reqwest::Response> {
        let started = Instant::now();
        let response = request.send().await;
        let result = match &response {
            Ok(response) if response.status().is_success() => "ok".to_owned(),
            Ok(response) => response.status().as_u16().to_string(),
            Err(_) => "transport".to_owned(),
        };
        self.metrics.request(operation, result, started.elapsed());
        response
    }

    fn allowed(&self, login: &str) -> bool {
        self.users.contains(&login.to_lowercase())
    }

    async fn token_login(&self, token: &str) -> anyhow::Result<Option<String>> {
        let digest: [u8; 32] = Sha256::digest(token.as_bytes()).into();
        if let Some((login, verified)) = self.tokens.lock().expect("not poisoned").get(&digest)
            && verified.elapsed() < TOKEN_CACHE
        {
            return Ok(login.clone());
        }
        let login = self.github_login(token).await?;
        let mut tokens = self.tokens.lock().expect("not poisoned");
        tokens.retain(|_, (_, verified)| verified.elapsed() < TOKEN_CACHE);
        tokens.insert(digest, (login.clone(), Instant::now()));
        Ok(login)
    }

    /// The login of the token's user, or `None` if GitHub rejects the token.
    async fn github_login(&self, token: &str) -> anyhow::Result<Option<String>> {
        #[derive(Deserialize)]
        struct GithubUser {
            login: String,
        }
        let request = self
            .http
            .get(format!("{}/user", self.api_url))
            .bearer_auth(token)
            .header(header::USER_AGENT, "amphi")
            .header(header::ACCEPT, "application/vnd.github+json");
        let response = self
            .send("user", request)
            .await
            .context("asking GitHub for the user")?;
        if response.status() == StatusCode::UNAUTHORIZED {
            return Ok(None);
        }
        let user: GithubUser = response.error_for_status()?.json().await?;
        Ok(Some(user.login))
    }

    async fn exchange_code(&self, code: &str) -> anyhow::Result<Option<String>> {
        #[derive(Deserialize)]
        struct TokenResponse {
            access_token: Option<String>,
            error_description: Option<String>,
        }
        let request = self
            .http
            .post(format!("{}/login/oauth/access_token", self.github_url))
            .header(header::ACCEPT, "application/json")
            .form(&[
                ("client_id", self.client_id.as_str()),
                ("client_secret", self.client_secret.as_str()),
                ("code", code),
                ("redirect_uri", self.callback_url.as_str()),
            ]);
        let response: TokenResponse = self
            .send("oauth_token", request)
            .await?
            .error_for_status()?
            .json()
            .await?;
        let token = response.access_token.with_context(|| {
            format!(
                "GitHub refused the login: {}",
                response.error_description.unwrap_or_default()
            )
        })?;
        self.github_login(&token).await
    }

    fn cookie(&self, name: &str, value: &str, max_age: Duration) -> HeaderValue {
        let secure = if self.secure_cookies { "; Secure" } else { "" };
        HeaderValue::from_str(&format!(
            "{name}={value}; Path=/; Max-Age={}; HttpOnly; SameSite=Lax{secure}",
            max_age.as_secs()
        ))
        .expect("cookie values are ASCII")
    }
}

fn cookie<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
        .find(|(key, _)| *key == name)
        .map(|(_, value)| value)
}

pub enum AuthError {
    Disabled,
    Unauthenticated,
    Forbidden(String),
    LoginExpired,
    Github(anyhow::Error),
}

impl IntoResponse for AuthError {
    fn into_response(self) -> Response {
        match self {
            Self::Disabled => (StatusCode::NOT_FOUND, "the web UI is disabled".to_owned()),
            Self::Unauthenticated => (
                StatusCode::UNAUTHORIZED,
                "log in at /api/auth/login, or send a GitHub token as a bearer token".to_owned(),
            ),
            Self::Forbidden(login) => (StatusCode::FORBIDDEN, format!("{login} may not use Amphi")),
            Self::LoginExpired => (
                StatusCode::BAD_REQUEST,
                "the login expired, try again".to_owned(),
            ),
            Self::Github(error) => {
                tracing::warn!("asking GitHub who the user is: {error:#}");
                (StatusCode::BAD_GATEWAY, format!("{error:#}"))
            }
        }
        .into_response()
    }
}

/// A user allowed to use the web UI and its API.
pub struct User(pub String);

impl FromRequestParts<Arc<Server>> for User {
    type Rejection = AuthError;

    async fn from_request_parts(
        parts: &mut Parts,
        server: &Arc<Server>,
    ) -> Result<Self, AuthError> {
        let auth = auth(server)?;
        let bearer = parts
            .headers
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "));
        let login = match bearer {
            Some(token) => auth.token_login(token).await.map_err(AuthError::Github)?,
            None => {
                cookie(&parts.headers, SESSION_COOKIE).and_then(|value| auth.sessions.login(value))
            }
        };
        match login {
            Some(login) if auth.allowed(&login) => Ok(Self(login)),
            Some(login) => Err(AuthError::Forbidden(login)),
            None => Err(AuthError::Unauthenticated),
        }
    }
}

pub fn router() -> Router<Arc<Server>> {
    Router::new()
        .route("/api/auth/login", get(login))
        .route("/api/auth/callback", get(callback))
        .route("/api/auth/logout", post(logout))
        .route("/api/auth/me", get(me))
}

fn auth(server: &Server) -> Result<&Auth, AuthError> {
    server.auth.as_ref().ok_or(AuthError::Disabled)
}

#[derive(Deserialize)]
struct LoginQuery {
    next: Option<String>,
}

/// Only paths on this site, so the login cannot redirect elsewhere. The
/// path is resolved like a browser would, which e.g. reads `/\host` as
/// `//host`.
fn local_path(next: Option<String>) -> String {
    let site = reqwest::Url::parse("http://amphi.invalid/").expect("valid URL");
    let Some(url) = next
        .filter(|next| next.starts_with('/'))
        .and_then(|next| site.join(&next).ok())
        .filter(|url| url.origin() == site.origin())
    else {
        return "/".to_owned();
    };
    match url.query() {
        Some(query) => format!("{}?{query}", url.path()),
        None => url.path().to_owned(),
    }
}

async fn login(
    State(server): State<Arc<Server>>,
    Query(query): Query<LoginQuery>,
) -> Result<Response, AuthError> {
    let auth = auth(&server)?;
    let state = uuid::Uuid::new_v4().simple().to_string();
    let next = local_path(query.next);
    let url = reqwest::Url::parse_with_params(
        &format!("{}/login/oauth/authorize", auth.github_url),
        [
            ("client_id", auth.client_id.as_str()),
            ("redirect_uri", auth.callback_url.as_str()),
            ("state", state.as_str()),
        ],
    )
    .expect("the GitHub URL was valid at startup");
    let cookie_value = format!("{state}.{}", URL_SAFE_NO_PAD.encode(next));
    Ok((
        [(
            header::SET_COOKIE,
            auth.cookie(STATE_COOKIE, &cookie_value, Duration::from_secs(600)),
        )],
        Redirect::to(url.as_str()),
    )
        .into_response())
}

#[derive(Deserialize)]
struct CallbackQuery {
    code: String,
    state: String,
}

async fn callback(
    State(server): State<Arc<Server>>,
    headers: HeaderMap,
    Query(query): Query<CallbackQuery>,
) -> Result<Response, AuthError> {
    let auth = auth(&server)?;
    let expected = cookie(&headers, STATE_COOKIE).and_then(|value| value.split_once('.'));
    let Some((_, next)) = expected.filter(|(state, _)| *state == query.state) else {
        return Err(AuthError::LoginExpired);
    };
    let next = local_path(
        URL_SAFE_NO_PAD
            .decode(next)
            .ok()
            .and_then(|next| String::from_utf8(next).ok()),
    );
    let login = auth
        .exchange_code(&query.code)
        .await
        .map_err(AuthError::Github)?
        .ok_or(AuthError::Unauthenticated)?;
    if !auth.allowed(&login) {
        return Err(AuthError::Forbidden(login));
    }
    tracing::info!(login, "logged in");
    let mut response = Redirect::to(&next).into_response();
    let headers = response.headers_mut();
    headers.append(
        header::SET_COOKIE,
        auth.cookie(
            SESSION_COOKIE,
            &auth.sessions.create(&login),
            SESSION_LIFETIME,
        ),
    );
    headers.append(
        header::SET_COOKIE,
        auth.cookie(STATE_COOKIE, "", Duration::ZERO),
    );
    Ok(response)
}

async fn logout(State(server): State<Arc<Server>>) -> Result<Response, AuthError> {
    let auth = auth(&server)?;
    Ok((
        [(
            header::SET_COOKIE,
            auth.cookie(SESSION_COOKIE, "", Duration::ZERO),
        )],
        StatusCode::NO_CONTENT,
    )
        .into_response())
}

#[derive(Serialize)]
struct Me {
    login: String,
}

async fn me(User(login): User) -> Json<Me> {
    Json(Me { login })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sessions() -> Sessions {
        Sessions {
            key: b"key".to_vec(),
        }
    }

    #[test]
    fn sessions_round_trip_and_reject_tampering() {
        let sessions = sessions();
        let session = sessions.create("alice");
        assert_eq!(sessions.login(&session).as_deref(), Some("alice"));

        let (_, signature) = session.split_once('.').unwrap();
        let forged = format!(
            "{}.{signature}",
            URL_SAFE_NO_PAD.encode("mallory:99999999999")
        );
        assert_eq!(sessions.login(&forged), None);
    }

    #[test]
    fn expired_sessions_are_rejected() {
        let sessions = sessions();
        let payload = "alice:1";
        let session = format!(
            "{}.{}",
            URL_SAFE_NO_PAD.encode(payload),
            sessions.sign(payload)
        );
        assert_eq!(sessions.login(&session), None);
    }

    #[test]
    fn logins_redirect_only_within_the_site() {
        assert_eq!(
            local_path(Some("/owner/repo/commit/abc".into())),
            "/owner/repo/commit/abc"
        );
        assert_eq!(local_path(Some("//evil.example".into())), "/");
        assert_eq!(local_path(Some("/\\evil.example".into())), "/");
        assert_eq!(
            local_path(Some("/runs?active=true".into())),
            "/runs?active=true"
        );
        assert_eq!(local_path(Some("https://evil.example".into())), "/");
        assert_eq!(local_path(None), "/");
    }
}
