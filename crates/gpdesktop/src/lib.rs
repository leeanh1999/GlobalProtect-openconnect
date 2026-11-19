//! UI-agnostic core of the open-source desktop GUIs.
//!
//! Same architecture as the official GUI: sign-in (password, SAML/SSO via
//! `gpauth`, MFA) runs in the user session ([`auth`]), then the privileged
//! `gpservice` is asked to open the tunnel over its authenticated
//! Noise/WebSocket channel ([`service`]). [`controller::Controller`] owns the
//! connection state machine (status, auto reconnect, re-authentication) so a
//! front end only renders [`controller::UiEvent`]s and sends
//! [`controller::Command`]s.

pub mod auth;
pub mod config;
pub mod controller;
pub mod log_level;
pub mod procs;
#[cfg(windows)]
pub mod secrets;
pub mod service;
pub mod state;

pub use log_level::LogLevel;
