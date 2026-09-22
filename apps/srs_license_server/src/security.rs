//! Central administrative authorization and production startup policy.
//!
//! Privileged routes (admin dashboard/API and license issuance) must go through
//! [`authorize_admin`]. Loopback is an extra development restriction, not identity.
//! The administrator bearer token is supplied externally (`SRS_ADMIN_TOKEN` /
//! `server.admin_token`) and is never embedded in source.

use std::net::SocketAddr;

use anyhow::{anyhow, Result};
use axum::http::HeaderMap;
use libsrs_app_config::ServerConfig;
use subtle::ConstantTimeEq;

/// Minimum accepted administrator bearer length.
pub const MIN_ADMIN_TOKEN_LEN: usize = 32;

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
            if !loopback {
                return Err(anyhow!(
                    "development mode requires a loopback bind_addr (got {})",
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

/// Extract the canonical administrator bearer credential.
pub fn extract_presented_token(headers: &HeaderMap) -> Option<String> {
    let value = headers.get(axum::http::header::AUTHORIZATION)?;
    let text = value.to_str().ok()?.trim();
    let token = text
        .strip_prefix("Bearer ")
        .or_else(|| text.strip_prefix("bearer "))?
        .trim();
    (!token.is_empty()).then(|| token.to_string())
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


#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::{header::AUTHORIZATION, HeaderValue};
    use libsrs_app_config::ServerConfig;
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};

    fn peer(ip: [u8; 4]) -> SocketAddr {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::from(ip)), 12345)
    }

    fn config_with_token() -> ServerConfig {
        let mut config = ServerConfig::default();
        config.admin_token = Some("0123456789abcdef0123456789abcdef".to_string());
        config
    }

    #[test]
    fn bearer_is_the_only_supported_admin_credential() {
        let mut headers = HeaderMap::new();
        headers.insert(AUTHORIZATION, HeaderValue::from_static(
            "Bearer 0123456789abcdef0123456789abcdef",
        ));
        assert_eq!(
            extract_presented_token(&headers).as_deref(),
            Some("0123456789abcdef0123456789abcdef")
        );

        let mut legacy = HeaderMap::new();
        legacy.insert(
            "x-srs-admin-token",
            HeaderValue::from_static("0123456789abcdef0123456789abcdef"),
        );
        assert!(extract_presented_token(&legacy).is_none());
    }

    #[test]
    fn development_requires_loopback_even_with_valid_token() {
        let config = config_with_token();
        let mut headers = HeaderMap::new();
        headers.insert(AUTHORIZATION, HeaderValue::from_static(
            "Bearer 0123456789abcdef0123456789abcdef",
        ));
        assert_eq!(
            authorize_admin(&config, &peer([203, 0, 113, 9]), &headers),
            Err(AdminAuthError::LoopbackRequired)
        );
    }

    #[test]
    fn development_allows_loopback_with_valid_token() {
        let config = config_with_token();
        let mut headers = HeaderMap::new();
        headers.insert(AUTHORIZATION, HeaderValue::from_static(
            "Bearer 0123456789abcdef0123456789abcdef",
        ));
        assert_eq!(
            authorize_admin(&config, &peer([127, 0, 0, 1]), &headers),
            Ok(())
        );
    }

    #[test]
    fn startup_rejects_missing_or_short_admin_token() {
        let mut config = ServerConfig::default();
        assert!(validate_startup(&config).is_err());
        config.admin_token = Some("too-short".to_string());
        assert!(validate_startup(&config).is_err());
    }

    #[test]
    fn production_rejects_published_dev_seed() {
        let mut config = config_with_token();
        config.operating_mode = "production".to_string();
        assert!(validate_startup(&config).is_err());
    }

    #[test]
    fn development_rejects_public_bind_with_dev_seed() {
        let mut config = config_with_token();
        config.bind_addr = "0.0.0.0:3000".to_string();
        assert!(validate_startup(&config).is_err());
    }

    #[test]
    fn redaction_never_returns_full_key() {
        let redacted = redact_license_key("SRS-AAAA-BBBB-CCCC-DDDD");
        assert_eq!(redacted, "****DDDD");
        assert_ne!(redacted, "SRS-AAAA-BBBB-CCCC-DDDD");
    }
}
