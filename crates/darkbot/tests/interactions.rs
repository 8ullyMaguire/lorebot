use axum::{
    body::Body,
    http::{header, Request, StatusCode},
};
use darkbot_core::{
    config::{BotConfig, MIN_STORE_KEY_BYTES},
    dispatch::Dispatcher,
    store::TokenStore,
};
use ed25519_dalek::{Signer, SigningKey};
use serde_json::json;
use std::sync::Arc;
use tower::ServiceExt;

fn signature(key: &SigningKey, timestamp: &str, body: &[u8]) -> String {
    let mut message = timestamp.as_bytes().to_vec();
    message.extend_from_slice(body);
    hex::encode(key.sign(&message).to_bytes())
}

#[tokio::test]
async fn ping_requires_a_valid_signature() {
    let key = SigningKey::from_bytes(&[7u8; 32]);
    let public = hex::encode(key.verifying_key().to_bytes());
    std::env::set_var("DARKBOT_DISCORD_PUBLIC_KEY", &public);
    let config = Arc::new(BotConfig {
        instance_url: "https://darkhold.example.test/api/v1".into(),
        instance_name: "example".into(),
        platforms: Default::default(),
        store_path: "unused".into(),
        store_key: [0u8; MIN_STORE_KEY_BYTES],
        ephemeral_store: true,
    });
    let dispatcher = Arc::new(Dispatcher::new(config, TokenStore::ephemeral()));
    let app = darkbot::interactions::router(dispatcher);
    let body = br#"{"type":1}"#;

    let unsigned = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/discord/interactions")
                .body(Body::from(body.to_vec()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(unsigned.status(), StatusCode::UNAUTHORIZED);
    let text = axum::body::to_bytes(unsigned.into_body(), 4096)
        .await
        .unwrap();
    assert!(String::from_utf8_lossy(&text).contains("invalid request signature"));

    let timestamp = "1700000000";
    let signed = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/discord/interactions")
                .header(header::CONTENT_TYPE, "application/json")
                .header("x-signature-timestamp", timestamp)
                .header("x-signature-ed25519", signature(&key, timestamp, body))
                .body(Body::from(body.to_vec()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(signed.status(), StatusCode::OK);
    let text = axum::body::to_bytes(signed.into_body(), 4096)
        .await
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&text), r#"{"type":1}"#);
}

#[test]
fn registered_tree_has_all_current_commands() {
    let definitions = darkbot::commands::definitions();
    assert_eq!(definitions[0]["name"], "darkhold");
    let names: Vec<&str> = definitions[0]["options"]
        .as_array()
        .expect("options")
        .iter()
        .map(|option| option["name"].as_str().expect("name"))
        .collect();
    assert_eq!(
        names,
        vec![
            "help",
            "search",
            "work",
            "kudos",
            "link",
            "link_confirm",
            "import",
            "jobs",
            "bookmark",
            "export",
            "status",
            "unlink",
            "privacy"
        ]
    );
}

fn test_app() -> axum::Router {
    let config = Arc::new(BotConfig {
        instance_url: "https://darkhold.example.test/api/v1".into(),
        instance_name: "example".into(),
        platforms: Default::default(),
        store_path: "unused".into(),
        store_key: [0u8; MIN_STORE_KEY_BYTES],
        ephemeral_store: true,
    });
    let dispatcher = Arc::new(Dispatcher::new(config, TokenStore::ephemeral()));
    darkbot::interactions::router(dispatcher)
}

#[tokio::test]
async fn signed_help_interaction_reaches_the_existing_dispatcher() {
    let key = SigningKey::from_bytes(&[7u8; 32]);
    std::env::set_var(
        "DARKBOT_DISCORD_PUBLIC_KEY",
        hex::encode(key.verifying_key().to_bytes()),
    );
    let body = serde_json::to_vec(&json!({
        "type": 2,
        "data": {
            "name": "darkhold",
            "options": [{ "name": "help", "options": [] }]
        },
        "member": { "user": { "id": "42" } },
        "channel_id": "7"
    }))
    .expect("serialise interaction");
    let timestamp = "1700000001";
    let response = test_app()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/discord/interactions")
                .header(header::CONTENT_TYPE, "application/json")
                .header("x-signature-timestamp", timestamp)
                .header("x-signature-ed25519", signature(&key, timestamp, &body))
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(response.into_body(), 8192)
        .await
        .unwrap();
    let value: serde_json::Value = serde_json::from_slice(&bytes).expect("json response");
    assert_eq!(value["data"]["flags"], 64);
    assert!(value["data"]["content"]
        .as_str()
        .expect("content")
        .contains("No account needed"));
}
