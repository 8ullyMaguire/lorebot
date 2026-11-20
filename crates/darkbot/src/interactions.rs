//! Discord's signed interactions transport.
//!
//! An interaction is an HTTPS request with an ed25519 signature over
//! `timestamp || raw_body`. The signature is checked BEFORE the body is parsed:
//! an unsigned body is not a request and must never reach the dispatcher.
//!
//! Ascend's `ascend-api` route is the reference for this contract. The state it
//! dispatches into is deliberately not copied: darkbot keeps its own store.

use std::sync::Arc;

use axum::{
    body::Bytes,
    extract::State,
    http::{header::HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
    Json, Router,
};
use darkbot_adapter::{Adapter, Destination};
use darkbot_core::dispatch::{Actor, Dispatcher, Inbound};
use darkbot_discord::DiscordAdapter;
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use serde_json::{json, Value};

struct InteractionState {
    dispatcher: Arc<Dispatcher>,
    /// Rendering only: an interaction reply is returned in the HTTP response,
    /// so this adapter never sends a request with the empty token.
    adapter: DiscordAdapter,
}

impl InteractionState {
    fn new(dispatcher: Arc<Dispatcher>) -> Self {
        Self {
            dispatcher,
            adapter: DiscordAdapter::new("").expect("a render-only Discord adapter"),
        }
    }
}

pub fn router(dispatcher: Arc<Dispatcher>) -> Router {
    Router::new()
        .route("/discord/interactions", post(interactions))
        .with_state(Arc::new(InteractionState::new(dispatcher)))
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

async fn interactions(
    State(state): State<Arc<InteractionState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
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
        2 => dispatch(&state, &payload).await.into_response(),
        other => {
            tracing::warn!(kind = other, "unhandled discord interaction type");
            reply("Darkhold does not do that yet.").into_response()
        }
    }
}

fn reply(content: impl Into<String>) -> Json<Value> {
    Json(json!({
        "type": 4,
        "data": {
            "content": content.into(),
            "flags": 64
        }
    }))
}

fn interaction_inbound(payload: &Value) -> Inbound {
    let data = &payload["data"];
    let root = data["name"].as_str().unwrap_or("help");
    let options = data["options"].as_array().cloned().unwrap_or_default();
    let (subcommand, params) = if root == "darkhold" {
        let first = options.first().cloned().unwrap_or(Value::Null);
        (
            first["name"].as_str().unwrap_or("help").to_owned(),
            first["options"].as_array().cloned().unwrap_or_default(),
        )
    } else {
        (root.to_owned(), options)
    };
    let option = |key: &str| -> String {
        params
            .iter()
            .find(|item| item["name"] == key)
            .and_then(|item| item["value"].as_str())
            .unwrap_or_default()
            .to_owned()
    };
    let args = match subcommand.as_str() {
        "search" => option("words"),
        "work" | "kudos" | "bookmark" | "export" => option("id"),
        "import" => option("url"),
        "link_confirm" => option("code"),
        _ => String::new(),
    };
    let user = payload["member"]["user"]["id"]
        .as_str()
        .or_else(|| payload["user"]["id"].as_str())
        .unwrap_or("unknown")
        .to_owned();
    let conversation = payload["channel_id"]
        .as_str()
        .unwrap_or("interaction")
        .to_owned();
    Inbound {
        actor: Actor::new("discord", user, conversation),
        text: format!("/{subcommand} {args}"),
    }
}

async fn dispatch(state: &InteractionState, payload: &Value) -> Json<Value> {
    let inbound = interaction_inbound(payload);
    let conversation = inbound.actor.conversation.clone();
    let user = inbound.actor.platform_user.clone();
    let replies = state.dispatcher.handle(inbound).await;
    if replies.is_empty() {
        return reply("Nothing to do.");
    }

    let mut lines = Vec::new();
    let mut private = false;
    for item in replies {
        private = private || item.is_private();
        let destination = if item.is_private() {
            Destination::private(&conversation, &user)
        } else {
            Destination::public(&conversation)
        };
        match state.adapter.render(item.payload(), &destination) {
            Ok(rendered) => {
                for message in rendered {
                    if !message.text.trim().is_empty() {
                        lines.push(message.text);
                    }
                }
            }
            Err(error) => {
                tracing::warn!(%error, "could not render interaction reply");
                lines.push("I could not render that reply.".to_owned());
            }
        }
    }
    let content = if lines.is_empty() {
        "No reply available.".to_owned()
    } else {
        lines.join("\n\n")
    };
    Json(json!({
        "type": 4,
        "data": {
            "content": content,
            "flags": if private { 64 } else { 0 }
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_a_root_command_with_options_into_the_existing_parser() {
        let payload = json!({
            "data": {
                "name": "darkhold",
                "options": [{
                    "name": "search",
                    "options": [{ "name": "words", "value": "quiet lorhaven" }]
                }]
            },
            "member": { "user": { "id": "42" } },
            "channel_id": "7"
        });
        let inbound = interaction_inbound(&payload);
        assert_eq!(inbound.text, "/search quiet lorhaven");
        assert_eq!(inbound.actor.platform, "discord");
        assert_eq!(inbound.actor.platform_user, "42");
        assert_eq!(inbound.actor.conversation, "7");
    }
}
