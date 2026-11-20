//! Discord's signed interactions transport.
//!
//! An interaction is an HTTPS request with an ed25519 signature over
//! `timestamp || raw_body`. The signature is checked BEFORE the body is parsed:
//! an unsigned body is not a request and must never reach the dispatcher.
//!
//! Ascend's `ascend-api` route is the reference for this contract. The state it
//! dispatches into is deliberately not copied: darkbot keeps its own store.

use axum::{
    body::Bytes,
    http::{header::HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
    Json, Router,
};
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use serde_json::{json, Value};

pub fn router() -> Router {
    Router::new().route("/discord/interactions", post(interactions))
}

fn verified(headers: &HeaderMap, body: &[u8]) -> bool {
    let Ok(public_key) = std::env::var("DARKBOT_DISCORD_PUBLIC_KEY") else {
        return false;
    };
    let (Some(signature), Some(timestamp)) = (
        headers
            .get("x-signature-ed25519")
            .and_then(|value| value.to_str().ok()),
        headers
            .get("x-signature-timestamp")
            .and_then(|value| value.to_str().ok()),
    ) else {
        return false;
    };

    let (Ok(key_bytes), Ok(signature_bytes)) = (hex::decode(public_key), hex::decode(signature))
    else {
        return false;
    };
    let Ok(key_bytes) = <[u8; 32]>::try_from(key_bytes.as_slice()) else {
        return false;
    };
    let Ok(key) = VerifyingKey::from_bytes(&key_bytes) else {
        return false;
    };
    let Ok(signature) = Signature::from_slice(&signature_bytes) else {
        return false;
    };

    let mut message = Vec::with_capacity(timestamp.len() + body.len());
    message.extend_from_slice(timestamp.as_bytes());
    message.extend_from_slice(body);
    key.verify(&message, &signature).is_ok()
}

async fn interactions(headers: HeaderMap, body: Bytes) -> Response {
    if std::env::var("DARKBOT_DISCORD_PUBLIC_KEY").is_err() {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({
                "error": {
                    "code": "discord_not_configured",
                    "message": "DARKBOT_DISCORD_PUBLIC_KEY is unset; the bot cannot verify requests"
                }
            })),
        )
            .into_response();
    }
    if !verified(&headers, &body) {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "error": "invalid request signature" })),
        )
            .into_response();
    }
    let Ok(payload) = serde_json::from_slice::<Value>(&body) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "malformed interaction" })),
        )
            .into_response();
    };

    match payload["type"].as_i64().unwrap_or(0) {
        1 => Json(json!({ "type": 1 })).into_response(),
        other => {
            tracing::warn!(kind = other, "unhandled discord interaction type");
            Json(json!({
                "type": 4,
                "data": {
                    "content": "Darkhold does not do that yet.",
                    "flags": 64
                }
            }))
            .into_response()
        }
    }
}
