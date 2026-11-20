//! The registered `/darkhold` command tree.
//!
//! One root command keeps Discord's `/` picker readable. Every subcommand maps
//! to an existing `darkbot-core` command; no new behaviour is invented here.

use serde_json::{json, Value};

fn required_string(name: &str, description: &str) -> Value {
    json!({
        "type": 3,
        "name": name,
        "description": description,
        "required": true
    })
}

fn subcommand(name: &str, description: &str, options: Vec<Value>) -> Value {
    json!({
        "type": 1,
        "name": name,
        "description": description,
        "options": options
    })
}

#[must_use]
pub fn definitions() -> Value {
    json!([{
        "name": "darkhold",
        "description": "Search and manage your Darkhold library",
        "type": 1,
        "options": [
            subcommand("help", "Show all commands and privacy notes", vec![]),
            subcommand("search", "Search public works", vec![required_string("words", "Words to search for")]),
            subcommand("work", "Show one work's metadata", vec![required_string("id", "Work id")]),
            subcommand("kudos", "Thank an author", vec![required_string("id", "Work id")]),
            subcommand("link", "Link this Discord account to Darkhold", vec![]),
            subcommand("link_confirm", "Confirm a link with the code from the site", vec![required_string("code", "Code from the site")]),
            subcommand("import", "Start a private import", vec![required_string("url", "Story link")]),
            subcommand("jobs", "Check your import jobs", vec![]),
            subcommand("bookmark", "Bookmark a work", vec![required_string("id", "Work id")]),
            subcommand("export", "Request an export", vec![required_string("id", "Work id")]),
            subcommand("status", "Show what your link can do", vec![]),
            subcommand("unlink", "Delete your link now", vec![]),
            subcommand("privacy", "What I send, keep and show", vec![]),
        ]
    }])
}
