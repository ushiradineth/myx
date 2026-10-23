//! Mandatory daemon-owned Spotify authorization and bounded Web API access.
mod auth;
mod cache;
pub mod catalog;
mod client;

pub use auth::{Authorization, AuthorizationFlow};
pub use client::{ApiClient, ApiError, HttpMethod};
