//! GitHub device-flow login, the one service that needs no token. The server
//! asks GitHub for a device code, hands it to the client, and exchanges it for
//! a token of `github:<login>` once the user has authorized it; it keeps no
//! state between the calls. Everything GitHub sends is checked here, at the
//! boundary.

use std::time::{Duration, SystemTime};

use scion_proto::v1::login_service_server::LoginService;
use scion_proto::v1::{
    FinishLoginRequest, FinishLoginResponse, StartLoginRequest, StartLoginResponse,
};
use serde::Deserialize;
use tonic::{Request, Response, Status};

use crate::token::{TokenKey, check_principal};

/// How long a token from `scn login` stays valid. Tokens cannot be revoked;
/// revoking a grant takes effect on the next request.
pub const LOGIN_TTL: Duration = Duration::from_secs(30 * 24 * 3600);

/// Deadline of each request to GitHub, well inside the RPC's own.
const GITHUB_DEADLINE: Duration = Duration::from_secs(10);

/// The GitHub OAuth app the server logs users in with.
#[derive(Debug, Clone)]
pub struct GitHub {
    client_id: String,
    /// `https://github.com`: device codes and access tokens.
    web: String,
    /// `https://api.github.com`: the user an access token belongs to.
    api: String,
}

impl GitHub {
    /// The OAuth app `client_id` (device flow enabled), on GitHub at `web`
    /// and its REST API at `api`.
    pub fn new(client_id: &str, web: &str, api: &str) -> Self {
        Self {
            client_id: client_id.to_owned(),
            web: web.trim_end_matches('/').to_owned(),
            api: api.trim_end_matches('/').to_owned(),
        }
    }
}

#[derive(Deserialize)]
struct CodeReply {
    device_code: String,
    user_code: String,
    verification_uri: String,
    expires_in: u32,
    interval: u32,
}

/// The access-token reply: a token, or an `error` such as
/// `authorization_pending`.
#[derive(Deserialize)]
struct TokenReply {
    access_token: Option<String>,
    error: Option<String>,
    error_description: Option<String>,
    interval: Option<u32>,
}

#[derive(Deserialize)]
struct User {
    login: String,
}

pub(crate) struct LoginServer {
    github: Option<GitHub>,
    key: TokenKey,
    http: reqwest::Client,
}

impl LoginServer {
    pub(crate) fn new(github: Option<GitHub>, key: TokenKey) -> Self {
        let http = reqwest::Client::builder()
            .user_agent(concat!("scion-server/", env!("CARGO_PKG_VERSION")))
            .timeout(GITHUB_DEADLINE)
            .build()
            .expect("the TLS backend initializes");
        Self { github, key, http }
    }

    fn github(&self) -> Result<&GitHub, Status> {
        self.github.as_ref().ok_or_else(|| {
            Status::failed_precondition(
                "this server has no GitHub login (SCION_GITHUB_CLIENT_ID is not set): ask its \
                 operator for a token from `scion-server token` and set SCN_TOKEN",
            )
        })
    }

    /// POST `form` to `url` and parse the JSON reply as `T`.
    async fn post<T: for<'de> Deserialize<'de>>(
        &self,
        url: &str,
        form: &[(&str, &str)],
    ) -> Result<T, Status> {
        let reply = self
            .http
            .post(url)
            .header(reqwest::header::ACCEPT, "application/json")
            .form(form)
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .map_err(unavailable)?;
        reply.json().await.map_err(unavailable)
    }
}

fn unavailable(e: reqwest::Error) -> Status {
    Status::unavailable(format!("GitHub: {}", e.without_url()))
}

#[tonic::async_trait]
impl LoginService for LoginServer {
    async fn start_login(
        &self,
        _: Request<StartLoginRequest>,
    ) -> Result<Response<StartLoginResponse>, Status> {
        let github = self.github()?;
        let url = format!("{}/login/device/code", github.web);
        let code: CodeReply = self
            .post(&url, &[("client_id", &github.client_id), ("scope", "")])
            .await?;
        Ok(Response::new(StartLoginResponse {
            device_code: code.device_code,
            user_code: code.user_code,
            verification_uri: code.verification_uri,
            expires_in_seconds: code.expires_in,
            interval_seconds: code.interval,
        }))
    }

    async fn finish_login(
        &self,
        request: Request<FinishLoginRequest>,
    ) -> Result<Response<FinishLoginResponse>, Status> {
        let github = self.github()?;
        let device_code = request.into_inner().device_code;
        let url = format!("{}/login/oauth/access_token", github.web);
        let reply: TokenReply = self
            .post(
                &url,
                &[
                    ("client_id", &github.client_id),
                    ("device_code", &device_code),
                    ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                ],
            )
            .await?;
        let access_token = match (reply.access_token, reply.error.as_deref()) {
            (Some(t), None) => t,
            (_, Some("authorization_pending" | "slow_down")) => {
                return Ok(Response::new(FinishLoginResponse {
                    interval_seconds: reply.interval.unwrap_or_default(),
                    ..FinishLoginResponse::default()
                }));
            }
            (_, Some("access_denied")) => {
                return Err(Status::permission_denied(
                    "the login was denied on GitHub; run `scn login` again to retry",
                ));
            }
            (_, Some("expired_token")) => {
                return Err(Status::deadline_exceeded(
                    "the GitHub code expired before it was entered; run `scn login` again",
                ));
            }
            (_, error) => {
                let why = reply
                    .error_description
                    .or_else(|| error.map(str::to_owned))
                    .unwrap_or_else(|| "no access token".to_owned());
                return Err(Status::unavailable(format!("GitHub: {why}")));
            }
        };
        let user: User = self
            .http
            .get(format!("{}/user", github.api))
            .bearer_auth(access_token)
            .header(reqwest::header::ACCEPT, "application/vnd.github+json")
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .map_err(unavailable)?
            .json()
            .await
            .map_err(unavailable)?;
        let principal = format!("github:{}", user.login);
        check_principal(&principal)
            .map_err(|e| Status::unavailable(format!("GitHub sent a bad login: {e}")))?;
        let expiry = SystemTime::now() + LOGIN_TTL;
        let token = self
            .key
            .issue(&principal, expiry)
            .map_err(|e| Status::internal(e.to_string()))?;
        Ok(Response::new(FinishLoginResponse {
            token,
            principal,
            expires_unix_seconds: expiry
                .duration_since(SystemTime::UNIX_EPOCH)
                .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX)),
            interval_seconds: 0,
        }))
    }
}
