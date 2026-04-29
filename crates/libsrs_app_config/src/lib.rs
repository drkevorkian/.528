use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use directories::ProjectDirs;
use libsrs_licensing_proto::{decode_signing_key, encode_verifying_key};
use serde::{Deserialize, Serialize};

pub const DEFAULT_PRIMARY_URL: &str = "http://localhost:3000";
pub const DEFAULT_BACKUP_URL: &str = "http://127.0.0.1:3000";
pub const LOCALHOST_DEV_SIGNING_KEY_SEED_B64: &str = "bG9jYWxob3N0LWRldi1zaWduaW5nLXNlZWQtMDAwMSE=";

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SrsConfig {
    #[serde(default)]
    pub client: ClientConfig,
    #[serde(default)]
    pub server: ServerConfig,
}

impl SrsConfig {
    pub fn load() -> Result<Self> {
        let path = env::var_os("SRS_CONFIG_PATH")
            .map(PathBuf::from)
            .unwrap_or_else(default_config_path);
        Self::load_from_path(&path)
    }

    pub fn load_from_path(path: &Path) -> Result<Self> {
        let mut config = if path.exists() {
            let content =
                fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
            toml::from_str::<SrsConfig>(&content)
                .with_context(|| format!("parse {}", path.display()))?
        } else {
            SrsConfig::default()
        };
        config.apply_env_overrides();
        Ok(config)
    }

    fn apply_env_overrides(&mut self) {
        if let Ok(value) = env::var("SRS_LICENSE_KEY") {
            self.client.license_key = Some(value);
        }
        if let Ok(value) = env::var("SRS_LICENSE_PRIMARY_URL") {
            self.client.primary_url = value;
        }
        if let Ok(value) = env::var("SRS_LICENSE_BACKUP_URL") {
            self.client.backup_url = value;
        }
        if let Ok(value) = env::var("SRS_LICENSE_PUBLIC_KEY_B64") {
            self.client.public_key_b64 = value;
        }
        if let Ok(value) = env::var("SRS_SERVER_BIND_ADDR") {
            self.server.bind_addr = value;
        }
        if let Ok(value) = env::var("SRS_SERVER_BASE_URL") {
            self.server.base_url = value;
        }
        if let Ok(value) = env::var("SRS_SERVER_DATABASE_PATH") {
            self.server.database_path = value;
        }
        if let Ok(value) = env::var("SRS_SERVER_SIGNING_KEY_SEED_B64") {
            self.server.signing_key_seed_b64 = Some(value);
        }
        if let Ok(value) = env::var("SRS_SERVER_MAIL_FROM") {
            self.server.mail_from = Some(value);
        }
        if let Ok(value) = env::var("SRS_SERVER_SMTP_SERVER") {
            self.server.smtp_server = Some(value);
        }
        if let Ok(value) = env::var("SRS_SERVER_SMTP_USERNAME") {
            self.server.smtp_username = Some(value);
        }
        if let Ok(value) = env::var("SRS_SERVER_SMTP_PASSWORD") {
            self.server.smtp_password = Some(value);
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientConfig {
    #[serde(default = "default_primary_url")]
    pub primary_url: String,
    #[serde(default = "default_backup_url")]
    pub backup_url: String,
    #[serde(default)]
    pub license_key: Option<String>,
    #[serde(default = "default_public_key_b64")]
    pub public_key_b64: String,
    #[serde(default = "default_connect_timeout_ms")]
    pub connect_timeout_ms: u64,
    #[serde(default = "default_request_timeout_ms")]
    pub request_timeout_ms: u64,
    #[serde(default = "default_refresh_interval_s")]
    pub refresh_interval_s: u64,
    #[serde(default = "default_contact_email")]
    pub contact_email: String,
    #[serde(default = "default_help_url")]
    pub help_url: String,
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self {
            primary_url: default_primary_url(),
            backup_url: default_backup_url(),
            license_key: None,
            public_key_b64: default_public_key_b64(),
            connect_timeout_ms: default_connect_timeout_ms(),
            request_timeout_ms: default_request_timeout_ms(),
            refresh_interval_s: default_refresh_interval_s(),
            contact_email: default_contact_email(),
            help_url: default_help_url(),
        }
    }
}

impl ClientConfig {
    pub fn project_dirs(&self) -> Result<ProjectDirs> {
        ProjectDirs::from("dev", "srs", "srs-media-system").context("resolve project directories")
    }

    pub fn state_dir(&self) -> Result<PathBuf> {
        Ok(self.project_dirs()?.data_local_dir().to_path_buf())
    }

    pub fn cache_file(&self) -> Result<PathBuf> {
        Ok(self.state_dir()?.join("license_cache.json"))
    }

    pub fn install_id_file(&self) -> Result<PathBuf> {
        Ok(self.state_dir()?.join("install_id"))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    #[serde(default = "default_bind_addr")]
    pub bind_addr: String,
    #[serde(default = "default_primary_url")]
    pub base_url: String,
    #[serde(default = "default_database_path")]
    pub database_path: String,
    #[serde(default = "default_confirmation_window_hours")]
    pub confirmation_window_hours: u64,
    #[serde(default = "default_token_ttl_hours")]
    pub token_ttl_hours: u64,
    #[serde(default)]
    pub signing_key_seed_b64: Option<String>,
    #[serde(default)]
    pub mail_from: Option<String>,
    #[serde(default)]
    pub smtp_server: Option<String>,
    #[serde(default)]
    pub smtp_username: Option<String>,
    #[serde(default)]
    pub smtp_password: Option<String>,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bind_addr: default_bind_addr(),
            base_url: default_primary_url(),
            database_path: default_database_path(),
            confirmation_window_hours: default_confirmation_window_hours(),
            token_ttl_hours: default_token_ttl_hours(),
            signing_key_seed_b64: Some(LOCALHOST_DEV_SIGNING_KEY_SEED_B64.to_string()),
            mail_from: None,
            smtp_server: None,
            smtp_username: None,
            smtp_password: None,
        }
    }
}

impl ServerConfig {
    pub fn signing_key_seed(&self) -> &str {
        self.signing_key_seed_b64
            .as_deref()
            .unwrap_or(LOCALHOST_DEV_SIGNING_KEY_SEED_B64)
    }

    pub fn resolved_database_path(&self) -> PathBuf {
        PathBuf::from(&self.database_path)
    }

    pub fn local_base_url(&self) -> String {
        let bind = self.bind_addr.trim();
        let (host, port) = bind.rsplit_once(':').unwrap_or(("127.0.0.1", "3000"));
        let host = match host {
            "0.0.0.0" | "*" | "" => "127.0.0.1",
            value => value,
        };
        format!("http://{host}:{port}")
    }
}

pub fn default_config_path() -> PathBuf {
    PathBuf::from("config/srs.toml")
}

pub fn default_primary_url() -> String {
    DEFAULT_PRIMARY_URL.to_string()
}

pub fn default_backup_url() -> String {
    DEFAULT_BACKUP_URL.to_string()
}

pub fn default_public_key_b64() -> String {
    let signing_key = decode_signing_key(LOCALHOST_DEV_SIGNING_KEY_SEED_B64)
        .expect("static dev signing seed must be valid");
    encode_verifying_key(&signing_key.verifying_key())
}

pub fn default_connect_timeout_ms() -> u64 {
    1_500
}

pub fn default_request_timeout_ms() -> u64 {
    5_000
}

pub fn default_refresh_interval_s() -> u64 {
    3_600
}

pub fn default_contact_email() -> String {
    "support@localhost".to_string()
}

pub fn default_help_url() -> String {
    DEFAULT_PRIMARY_URL.to_string()
}

pub fn default_bind_addr() -> String {
    "127.0.0.1:3000".to_string()
}

pub fn default_database_path() -> String {
    "var/srs_license.sqlite3".to_string()
}

pub fn default_confirmation_window_hours() -> u64 {
    72
}

pub fn default_token_ttl_hours() -> u64 {
    24
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_use_localhost_endpoints() {
        let config = SrsConfig::default();
        assert_eq!(config.client.primary_url, DEFAULT_PRIMARY_URL);
        assert_eq!(config.client.backup_url, DEFAULT_BACKUP_URL);
    }

    #[test]
    fn default_public_key_is_derived_from_dev_seed() {
        let derived = default_public_key_b64();
        assert!(!derived.is_empty());
    }
}
