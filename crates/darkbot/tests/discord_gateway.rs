//! M45-43: the Discord loop must feed its one parser, not invent a second one.

fn main_source() -> String {
    std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/main.rs")).expect("main.rs")
}

#[test]
fn the_discord_loop_uses_the_adapter_parser() {
    let source = main_source();
    let start = source.find("fn spawn_discord").expect("spawn_discord");
    let end = source.find("fn spawn_telegram").expect("spawn_telegram");
    let loop_source = &source[start..end];
    assert!(
        loop_source.contains("on_payload"),
        "the gateway loop must call DiscordAdapter::on_payload, not re-parse MESSAGE_CREATE"
    );
    assert!(
        loop_source.contains("connect_async"),
        "the gateway loop must open the Discord websocket"
    );
    assert!(
        loop_source.contains("heartbeat"),
        "the gateway loop must keep Discord's heartbeat"
    );
}
