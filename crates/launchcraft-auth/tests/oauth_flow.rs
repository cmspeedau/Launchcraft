//! End-to-end OAuth chain against a local mock of every upstream hop:
//! device-code poll (including one `authorization_pending` round), Xbox
//! Live, XSTS (xuid capture), Minecraft login, entitlements and profile.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use axum::{extract::Form, routing, Json, Router};
use serde_json::{json, Value};

use launchcraft_auth::{AuthEndpoints, AuthError, MicrosoftAuthClient};

#[derive(Clone)]
struct MockState {
    polls: Arc<AtomicUsize>,
    owns_game: bool,
}

fn mock_app(state: MockState) -> Router {
    Router::new()
        .route(
            "/oauth2/v2.0/devicecode",
            routing::post(|_: Form<HashMap<String, String>>| async {
                Json(json!({
                    "device_code": "dc-123",
                    "user_code": "ABC-DEF",
                    "verification_uri": "https://example.invalid/device",
                    "expires_in": 900,
                    "interval": 0,
                    "message": "mock"
                }))
            }),
        )
        .route(
            "/oauth2/v2.0/token",
            routing::post(|_: Form<HashMap<String, String>>| async move {
                // The real endpoint omits token fields entirely while
                // the user has not consented yet: reproduce that.
                let n = state.polls.fetch_add(1, Ordering::SeqCst);
                if n == 0 {
                    Json(json!({
                        "error": "authorization_pending",
                        "error_description": "AADSTS70016: pending"
                    }))
                } else {
                    Json(json!({
                        "access_token": "ms-access-1",
                        "refresh_token": "ms-refresh-1",
                        "expires_in": 3600,
                        "token_type": "bearer"
                    }))
                }
            }),
        )
        .route(
            "/user/authenticate",
            routing::post(|Json(body): Json<Value>| async move {
                let ticket = body["Properties"]["RpsTicket"].as_str().unwrap_or("");
                assert!(ticket.starts_with("d=ms-access"), "xbl got {ticket}");
                Json(json!({
                    "Token": "xbl-token",
                    "DisplayClaims": { "xui": [{ "uhs": "uhs-21" }] }
                }))
            }),
        )
        .route(
            "/xsts/authorize",
            routing::post(|Json(body): Json<Value>| async move {
                assert_eq!(
                    body["RelyingParty"].as_str().unwrap(),
                    "rp://api.minecraftservices.com/"
                );
                Json(json!({
                    "Token": "xsts-token",
                    "DisplayClaims": { "xui": [{ "uhs": "uhs-21", "xid": "1234567890" }] }
                }))
            }),
        )
        .route(
            "/authentication/login_with_xbox",
            routing::post(|Json(body): Json<Value>| async move {
                assert_eq!(
                    body["identityToken"].as_str().unwrap(),
                    "XBL3.0 x=uhs-21;xsts-token"
                );
                Json(json!({
                    "access_token": "mc-token",
                    "expires_in": 86400,
                    "token_type": "Bearer"
                }))
            }),
        )
        .route(
            "/entitlements/mcstore",
            routing::get(move || {
                let owns = state.owns_game;
                async move {
                    Json(json!({
                        "items": if owns {
                            vec![json!({"name": "game_minecraft", "signatureDate": ""})]
                        } else {
                            vec![]
                        },
                        "signature": ""
                    }))
                }
            }),
        )
        .route(
            "/minecraft/profile",
            routing::get(|| async {
                Json(json!({
                    "id": "853c80ef-3b39-48fd-a267-da3fc4a4d0f7",
                    "name": "Craftsman",
                    "skins": [{
                        "id": "skin-1",
                        "state": "ACTIVE",
                        "url": "https://textures.example.invalid/skin.png",
                        "variant": "classic"
                    }],
                    "capes": []
                }))
            }),
        )
}

async fn spawn_mock(owns_game: bool) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let state = MockState {
        polls: Arc::new(AtomicUsize::new(0)),
        owns_game,
    };
    let app = mock_app(state);
    let handle = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://{addr}"), handle)
}

fn client_for(base: &str) -> MicrosoftAuthClient {
    MicrosoftAuthClient::with_endpoints(
        Some("test-client".to_string()),
        AuthEndpoints {
            ms_authority: base.to_string(),
            xbox_live: base.to_string(),
            xsts: base.to_string(),
            minecraft_services: base.to_string(),
        },
    )
}

#[tokio::test]
async fn device_login_completes_full_chain() {
    let (base, server) = spawn_mock(true).await;
    let client = client_for(&base);

    let flow = client.initiate_device_flow().await.unwrap();
    assert_eq!(flow.user_code, "ABC-DEF");

    let mut pending_rounds = 0;
    let account = client
        .finish_device_login(&flow, |_| pending_rounds += 1)
        .await
        .unwrap();

    assert_eq!(pending_rounds, 1, "one authorization_pending poll expected");
    assert_eq!(account.username, "Craftsman");
    assert_eq!(account.xuid, "1234567890");
    assert_eq!(account.access_token, "mc-token");
    assert_eq!(account.refresh_token, "ms-refresh-1");
    assert_eq!(
        account.skin_url.as_deref(),
        Some("https://textures.example.invalid/skin.png")
    );
    assert!(!account.is_expired());
    assert_eq!(
        account.uuid.to_string(),
        "853c80ef-3b39-48fd-a267-da3fc4a4d0f7"
    );

    server.abort();
}

#[tokio::test]
async fn game_not_owned_is_reported() {
    let (base, server) = spawn_mock(false).await;
    let client = client_for(&base);

    let flow = client.initiate_device_flow().await.unwrap();
    let err = client.finish_device_login(&flow, |_| {}).await.unwrap_err();
    assert!(matches!(err, AuthError::GameNotOwned), "got {err:?}");

    server.abort();
}

#[tokio::test]
async fn refresh_account_reruns_the_chain() {
    let (base, server) = spawn_mock(true).await;
    let client = client_for(&base);

    // Seed an account whose tokens are about to lapse.
    let flow = client.initiate_device_flow().await.unwrap();
    let first = client.finish_device_login(&flow, |_| {}).await.unwrap();

    // Second mock instance: token endpoint starts fresh, so the refresh
    // grant (any request after the first) returns tokens immediately…
    let refreshed = client.refresh_account(&first).await.unwrap();
    assert_eq!(refreshed.username, "Craftsman");
    assert_eq!(refreshed.xuid, "1234567890");
    assert!(!refreshed.is_expired());

    server.abort();
}
