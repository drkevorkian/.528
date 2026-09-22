//! Central administrative authorization and production startup policy.
//!
//! Privileged routes (admin dashboard/API and license issuance) must go through
//! [`authorize_admin`]. Loopback is an extra development restriction, not identity.
//! The administrator bearer token is supplied externally (`SRS_ADMIN_TOKEN` /
//! `server.admin_token`) and is never embedded in source.

use std::net::SocketAddr;

use anyhow::{anyhow, Result};
use axum::http::HeaderMap;
use libsrs_app_config::{ServerConfig, LOCALHOST_DEV_SIGNING_KEY_SEED_B64};
use subtle::ConstantTimeEq;

/// Minimum accepted administrator token length (high-entropy bearer, not a password KDF).
pub const MIN_ADMIN_TOKEN_LEN: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OperatingMode {
    Development,
    Production,
}

impl OperatingMode {
    pub fn from_config(config: &ServerConfig) -> Self {
        if config.is_production() {
            Self::Production
        } else {
            Self::Development
        }
    }
}

/// Fail closed before the listener binds when the configuration is unsafe.
pub fn validate_startup(config: &ServerConfig) -> Result<()> {
    let mode = OperatingMode::from_config(config);
    let uses_dev_seed = config.uses_published_dev_signing_seed();
    let loopback = config.bind_is_loopback_only();
    let token = config.admin_token.as_deref().unwrap_or("").trim();

    if token.is_empty() {
        return Err(anyhow!(
            "administrator token missing: set SRS_ADMIN_TOKEN or server.admin_token \
             (issuance and admin APIs are authenticated/admin-only)"
        ));
    }
    if token.len() < MIN_ADMIN_TOKEN_LEN {
        return Err(anyhow!(
            "administrator token is too short (need at least {MIN_ADMIN_TOKEN_LEN} characters)"
        ));
    }

    match mode {
        OperatingMode::Development => {
            if !loopback && uses_dev_seed {
                return Err(anyhow!(
                    "development signing seed {} cannot be used when bind_addr is not loopback ({})",
                    LOCALHOST_DEV_SIGNING_KEY_SEED_B64,
                    config.bind_addr
                ));
            }
        }
        OperatingMode::Production => {
            if uses_dev_seed {
                return Err(anyhow!(
                    "production mode forbids the published development signing seed; \
                     set SRS_SERVER_SIGNING_KEY_SEED_B64 to a unique secret"
                ));
            }
            if config.signing_key_seed_b64.is_none() {
                return Err(anyhow!(
                    "production mode requires an explicit signing_key_seed_b64"
                ));
            }
            let base = config.base_url.trim();
            if !base.starts_with("https://") && !loopback {
                return Err(anyhow!(
                    "production mode requires https base_url unless bind_addr is loopback (got {})",
                    config.base_url
                ));
            }
        }
    }
    Ok(())
}

/// Extract a presented administrator credential from Authorization, cookie, or X-SRS-Admin-Token.
pub fn extract_presented_token(headers: &HeaderMap) -> Option<String> {
    if let Some(value) = headers.get("x-srs-admin-token") {
        if let Ok(text) = value.to_str() {
            let trimmed = text.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
    }
    if let Some(value) = headers.get(axum::http::header::COOKIE) {
        if let Ok(text) = value.to_str() {
            for part in text.split(';') {
                let part = part.trim();
                if let Some(value) = part.strip_prefix("srs_admin=") {
                    let value = value.trim();
                    if !value.is_empty() {
                        return Some(value.to_string());
                    }
                }
            }
        }
    }
    if let Some(value) = headers.get(axum::http::header::AUTHORIZATION) {
        if let Ok(text) = value.to_str() {
            let trimmed = text.trim();
            let bearer = trimmed
                .strip_prefix("Bearer ")
                .or_else(|| trimmed.strip_prefix("bearer "));
            if let Some(token) = bearer {
                let token = token.trim();
                if !token.is_empty() {
                    return Some(token.to_string());
                }
            }
        }
    }
    None
}

fn tokens_equal(left: &str, right: &str) -> bool {
    let a = left.as_bytes();
    let b = right.as_bytes();
    if a.len() != b.len() {
        return false;
    }
    bool::from(a.ct_eq(b))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdminAuthError {
    MissingToken,
    InvalidToken,
    LoopbackRequired,
}

impl AdminAuthError {
    pub fn status_message(self) -> (axum::http::StatusCode, &'static str) {
        match self {
            Self::MissingToken => (
                axum::http::StatusCode::UNAUTHORIZED,
                "administrator authentication required",
            ),
            Self::InvalidToken => (
                axum::http::StatusCode::UNAUTHORIZED,
                "administrator authentication failed",
            ),
            Self::LoopbackRequired => (
                axum::http::StatusCode::FORBIDDEN,
                "development admin routes require a loopback peer",
            ),
        }
    }
}

/// Central gate for privileged routes.
///
/// Development: valid admin token + loopback TCP peer.
/// Production: valid admin token (peer may be remote).
pub fn authorize_admin(
    config: &ServerConfig,
    peer: &SocketAddr,
    headers: &HeaderMap,
) -> Result<(), AdminAuthError> {
    let expected = config
        .admin_token
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .ok_or(AdminAuthError::MissingToken)?;

    let presented = extract_presented_token(headers).ok_or(AdminAuthError::MissingToken)?;
    if !tokens_equal(&presented, expected) {
        return Err(AdminAuthError::InvalidToken);
    }

    if OperatingMode::from_config(config) == OperatingMode::Development && !peer.ip().is_loopback() {
        return Err(AdminAuthError::LoopbackRequired);
    }
    Ok(())
}

/// Redact a stored license key for default admin views.
pub fn redact_license_key(key: &str) -> String {
    let trimmed = key.trim();
    if trimmed.len() <= 4 {
        return "****".to_string();
    }
    let tail = &trimmed[trimmed.len() - 4..];
    format!("****{tail}")
}
