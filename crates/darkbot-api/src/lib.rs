//! # darkbot-api
//!
//! The typed client for Darkhold's public API — spec §23.1.
//!
//! This crate is the bot's only view of Darkhold. It speaks HTTP and nothing
//! else: no database driver, no direct access to Darkhold's internals, and no
//! import of a `darkhold-*` crate. If Darkhold is offline the bot reports
//! that, and it cannot fall back to reading the database — which is the
//! property §23.2 is really asking for.
//!
//! ## What is here
//!
//! * [`client`] — [`client::DarkholdClient`], one method per §23.2 action.
//! * [`model`] — the wire types, matching what Darkhold's handlers return.
//!
//! ## The two things a reviewer should read first
//!
//! **A public call sends no credential.** `search_public_works` and
//! `public_work` go out with no `Authorization` header even when the client
//! has a token attached. Putting a bearer token on a request that did not need
//! it puts it in every proxy log between the bot and Darkhold for no
//! benefit.
//!
//! **A 401 and a 403 are not the same failure.** A 401 means the token was
//! revoked and the user must re-link. A 403 with `MISSING_SCOPE` means the
//! token is fine and lacks a permission, and re-linking will not help unless
//! they grant more. [`client::ApiError`] keeps them apart, and
//! `no_code_path_reaches_a_database` in `darkbot-core` keeps the rest honest.
//!
//! ## Testing
//!
//! The client is tested against `wiremock`, a real HTTP server on localhost,
//! rather than against a hand-written mock of the client's own assumptions. A
//! mock that agrees with the client proves the client is self-consistent, which
//! is not the question. The question is whether the client agrees with
//! Darkhold, and that needs actual status codes and actual bodies.

pub mod client;
pub mod model;

pub use client::{ApiError, ClientConfig, Credential, DarkholdClient};
pub use model::{
    ApiErrorBody, BookmarkRequest, BookmarkView, ExportRequest, ExportView, ImportView, JobList,
    JobView, KudosView, LinkChallenge, LinkToken, Scope, ScopeSet, SearchPage, StartImport,
    TokenView, WorkSummary,
};

/// The client's version.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
