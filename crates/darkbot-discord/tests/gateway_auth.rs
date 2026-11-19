//! Discord's gateway endpoint requires `Authorization: Bot ...`; a bearer header gets a 401.

use darkbot_discord::DiscordAdapter;

#[test]
fn the_gateway_uses_a_bot_authorization_header() {
    let adapter = DiscordAdapter::new("a-token").expect("adapter");
    let header = adapter.authorization_header();
    assert!(header.starts_with("Bot "), "{header}");
    assert!(!header.to_ascii_lowercase().contains("bearer"));
}

#[test]
fn message_content_is_opt_in_until_the_portal_enables_it() {
    let adapter = DiscordAdapter::new("a-token").expect("adapter");
    let base = adapter.identify_with_intents(false);
    let intents = base["d"]["intents"].as_u64().expect("intents");
    assert_eq!(intents, darkbot_discord::GUILDS);
    let full = adapter.identify_with_intents(true);
    assert_eq!(
        full["d"]["intents"].as_u64(),
        Some(darkbot_discord::GUILDS | darkbot_discord::MESSAGE_CONTENT)
    );
}
