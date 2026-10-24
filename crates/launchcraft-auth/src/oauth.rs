use std::time::Duration;
use chrono::{Duration as ChronoDuration, Utc};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::{AuthError, Result};
use crate::models::{LaunchcraftAccount, MinecraftProfile};

pub const DEFAULT_CLIENT_ID: &str = "00000000402b5328"; // Standard client ID commonly utilized for Minecraft OAuth
pub const REDIRECT_URI: &str = "https://login.live.com/oauth20_desktop.srf";

#[derive(Debug, Clone, Deserialize)]
pub struct DeviceCodeResponse {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    pub expires_in: u64,
    pub interval: u64,
    pub message: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: String,
    expires_in: i64,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    error_description: Option<String>,
}

#[derive(Debug, Serialize)]
struct XboxLiveProperties {
    #[serde(rename = "AuthMethod")]
    auth_method: String,
    #[serde(rename = "SiteName")]
    site_name: String,
    #[serde(rename = "RpsTicket")]
    rps_ticket: String,
}

#[derive(Debug, Serialize)]
struct XboxLiveAuthRequest {
    #[serde(rename = "Properties")]
    properties: XboxLiveProperties,
    #[serde(rename = "RelyingParty")]
    relying_party: String,
    #[serde(rename = "TokenType")]
    token_type: String,
}

#[derive(Debug, Deserialize)]
struct XboxLiveAuthResponse {
    #[serde(rename = "Token")]
    token: String,
    #[serde(rename = "DisplayClaims")]
    display_claims: DisplayClaims,
}

#[derive(Debug, Deserialize)]
struct DisplayClaims {
    xui: Vec<XuiClaim>,
}

#[derive(Debug, Deserialize)]
struct XuiClaim {
    uhs: String,
}

#[derive(Debug, Serialize)]
struct XstsProperties {
    #[serde(rename = "SandboxId")]
    sandbox_id: String,
    #[serde(rename = "UserTokens")]
    user_tokens: Vec<String>,
}

#[derive(Debug, Serialize)]
struct XstsAuthRequest {
    #[serde(rename = "Properties")]
    properties: XstsProperties,
    #[serde(rename = "RelyingParty")]
    relying_party: String,
    #[serde(rename = "TokenType")]
    token_type: String,
}

#[derive(Debug, Serialize)]
struct McLoginRequest {
    #[serde(rename = "identityToken")]
    identity_token: String,
}

#[derive(Debug, Deserialize)]
struct McLoginResponse {
    access_token: String,
    expires_in: i64,
}

#[derive(Debug, Deserialize)]
struct McOwnershipResponse {
    items: Vec<McItem>,
}

#[derive(Debug, Deserialize)]
struct McItem {
    name: String,
}

pub struct MicrosoftAuthClient {
    http: Client,
    client_id: String,
}

impl MicrosoftAuthClient {
    pub fn new(client_id: Option<String>) -> Self {
        let http = Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .expect("Failed to create HTTP client");

        Self {
            http,
            client_id: client_id.unwrap_or_else(|| DEFAULT_CLIENT_ID.to_string()),
        }
    }

    /// Initiate device code flow
    pub async fn initiate_device_flow(&self) -> Result<DeviceCodeResponse> {
        let url = "https://login.microsoftonline.com/consumers/oauth2/v2.0/devicecode";
        let params = [
            ("client_id", self.client_id.as_str()),
            ("scope", "XboxLive.signin offline_access"),
        ];

        let res = self.http.post(url).form(&params).send().await?;
        if !res.status().is_success() {
            let body = res.text().await?;
            return Err(AuthError::OAuth {
                error: "device_code_init_failed".to_string(),
                description: body,
            });
        }

        let device_code = res.json::<DeviceCodeResponse>().await?;
        Ok(device_code)
    }

    /// Poll for OAuth token via device code flow
    pub async fn poll_device_code_token(&self, device_code: &str) -> Result<(String, String)> {
        let url = "https://login.microsoftonline.com/consumers/oauth2/v2.0/token";
        let params = [
            ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
            ("client_id", self.client_id.as_str()),
            ("device_code", device_code),
        ];

        let res = self.http.post(url).form(&params).send().await?;
        let token_resp: TokenResponse = res.json().await?;

        if let Some(err) = token_resp.error {
            return Err(AuthError::OAuth {
                error: err,
                description: token_resp.error_description.unwrap_or_default(),
            });
        }

        Ok((token_resp.access_token, token_resp.refresh_token))
    }

    /// Exchange authorization code from loopback/redirect for tokens
    pub async fn exchange_auth_code(&self, code: &str, redirect_uri: &str) -> Result<(String, String)> {
        let url = "https://login.microsoftonline.com/consumers/oauth2/v2.0/token";
        let params = [
            ("client_id", self.client_id.as_str()),
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", redirect_uri),
            ("scope", "XboxLive.signin offline_access"),
        ];

        let res = self.http.post(url).form(&params).send().await?;
        let token_resp: TokenResponse = res.json().await?;

        if let Some(err) = token_resp.error {
            return Err(AuthError::OAuth {
                error: err,
                description: token_resp.error_description.unwrap_or_default(),
            });
        }

        Ok((token_resp.access_token, token_resp.refresh_token))
    }

    /// Refresh Microsoft OAuth tokens
    pub async fn refresh_oauth_token(&self, refresh_token: &str) -> Result<(String, String)> {
        let url = "https://login.microsoftonline.com/consumers/oauth2/v2.0/token";
        let params = [
            ("client_id", self.client_id.as_str()),
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
            ("scope", "XboxLive.signin offline_access"),
        ];

        let res = self.http.post(url).form(&params).send().await?;
        let token_resp: TokenResponse = res.json().await?;

        if let Some(err) = token_resp.error {
            return Err(AuthError::OAuth {
                error: err,
                description: token_resp.error_description.unwrap_or_default(),
            });
        }

        Ok((token_resp.access_token, token_resp.refresh_token))
    }

    /// Complete full Minecraft authentication chain from Microsoft OAuth access token
    pub async fn authenticate_minecraft(&self, ms_access_token: &str, refresh_token: &str) -> Result<LaunchcraftAccount> {
        // Step 1: Xbox Live authentication
        let (xbl_token, uhs) = self.authenticate_xbox_live(ms_access_token).await?;

        // Step 2: XSTS authentication
        let xsts_token = self.authenticate_xsts(&xbl_token).await?;

        // Step 3: Minecraft Services Login
        let (mc_token, expires_in) = self.login_with_xbox(&xsts_token, &uhs).await?;

        // Step 4: Verify Game Ownership
        self.check_game_ownership(&mc_token).await?;

        // Step 5: Fetch Profile
        let profile = self.get_profile(&mc_token).await?;

        let expires_at = Utc::now() + ChronoDuration::seconds(expires_in);
        let skin_url = profile.skins.into_iter().find(|s| s.state == "ACTIVE").map(|s| s.url);

        Ok(LaunchcraftAccount {
            uuid: profile.id,
            username: profile.name,
            refresh_token: refresh_token.to_string(),
            access_token: mc_token,
            expires_at,
            skin_url,
        })
    }

    /// Full account refresh: refreshes MS OAuth token and completes MC auth chain
    pub async fn refresh_account(&self, account: &LaunchcraftAccount) -> Result<LaunchcraftAccount> {
        let (new_ms_token, new_refresh_token) = self.refresh_oauth_token(&account.refresh_token).await?;
        self.authenticate_minecraft(&new_ms_token, &new_refresh_token).await
    }

    async fn authenticate_xbox_live(&self, ms_token: &str) -> Result<(String, String)> {
        let url = "https://user.auth.xboxlive.com/user/authenticate";
        let req = XboxLiveAuthRequest {
            properties: XboxLiveProperties {
                auth_method: "RPS".to_string(),
                site_name: "user.auth.xboxlive.com".to_string(),
                rps_ticket: format!("d={}", ms_token),
            },
            relying_party: "http://auth.xboxlive.com".to_string(),
            token_type: "JWT".to_string(),
        };

        let res = self.http.post(url).json(&req).send().await?;
        if !res.status().is_success() {
            let status = res.status().as_u16() as u64;
            return Err(AuthError::XboxLiveError(status));
        }

        let body: XboxLiveAuthResponse = res.json().await?;
        let uhs = body
            .display_claims
            .xui
            .into_iter()
            .next()
            .map(|x| x.uhs)
            .ok_or_else(|| AuthError::XboxLiveError(0))?;

        Ok((body.token, uhs))
    }

    async fn authenticate_xsts(&self, xbl_token: &str) -> Result<String> {
        let url = "https://xsts.auth.xboxlive.com/xsts/authorize";
        let req = XstsAuthRequest {
            properties: XstsProperties {
                sandbox_id: "RETAIL".to_string(),
                user_tokens: vec![xbl_token.to_string()],
            },
            relying_party: "rp://api.minecraftservices.com/".to_string(),
            token_type: "JWT".to_string(),
        };

        let res = self.http.post(url).json(&req).send().await?;
        if !res.status().is_success() {
            let body = res.text().await?;
            return Err(AuthError::XstsError(body));
        }

        let body: XboxLiveAuthResponse = res.json().await?;
        Ok(body.token)
    }

    async fn login_with_xbox(&self, xsts_token: &str, uhs: &str) -> Result<(String, i64)> {
        let url = "https://api.minecraftservices.com/authentication/login_with_xbox";
        let req = McLoginRequest {
            identity_token: format!("XBL3.0 x={};{}", uhs, xsts_token),
        };

        let res = self.http.post(url).json(&req).send().await?;
        if !res.status().is_success() {
            let body = res.text().await?;
            return Err(AuthError::MinecraftService(body));
        }

        let body: McLoginResponse = res.json().await?;
        Ok((body.access_token, body.expires_in))
    }

    async fn check_game_ownership(&self, mc_token: &str) -> Result<()> {
        let url = "https://api.minecraftservices.com/entitlements/mcstore";
        let res = self.http.get(url).bearer_auth(mc_token).send().await?;

        if !res.status().is_success() {
            let body = res.text().await?;
            return Err(AuthError::MinecraftService(body));
        }

        let ownership: McOwnershipResponse = res.json().await?;
        let owns_game = ownership.items.iter().any(|item| {
            item.name == "product_minecraft" || item.name == "game_minecraft"
        });

        if !owns_game {
            return Err(AuthError::GameNotOwned);
        }

        Ok(())
    }

    pub async fn get_profile(&self, mc_token: &str) -> Result<MinecraftProfile> {
        let url = "https://api.minecraftservices.com/minecraft/profile";
        let res = self.http.get(url).bearer_auth(mc_token).send().await?;

        if !res.status().is_success() {
            let body = res.text().await?;
            return Err(AuthError::MinecraftService(body));
        }

        let profile: MinecraftProfile = res.json().await?;
        Ok(profile)
    }
}
