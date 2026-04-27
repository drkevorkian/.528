use std::env;
use std::fs;
use std::io::ErrorKind;
use std::net::UdpSocket;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use keyring::Entry;
use libsrs_app_config::ClientConfig;
use libsrs_licensing_proto::{
    decode_verifying_key, ClientAppInfo, ClientNotification, ClientNotificationReadRequest,
    ClientOsInfo, ClientUnsupportedPlaybackRequest, DeviceFingerprint, EntitlementClaims,
    EntitlementStatus, SignedEntitlementEnvelope, UnsupportedCodecTrack, VerifyKeyRequest,
    VerifyKeyResponse,
};
use reqwest::blocking::Client;
use reqwest::Url;
use serde::{Deserialize, Serialize};
use tracing::warn;
use uuid::Uuid;

const KEYRING_SERVICE: &str = "srs-media-system";

#[derive(Debug, Clone)]
pub struct LicensingClient {
    config: ClientConfig,
    http: Client,
    state_dir_override: Option<PathBuf>,
}

impl LicensingClient {
    pub fn new(config: ClientConfig) -> Result<Self> {
        let http = Client::builder()
            .connect_timeout(Duration::from_millis(config.connect_timeout_ms))
            .timeout(Duration::from_millis(config.request_timeout_ms))
            .build()
            .context("build reqwest client")?;
        Ok(Self {
            config,
            http,
            state_dir_override: None,
        })
    }

    pub fn with_state_dir(mut self, state_dir: PathBuf) -> Self {
        self.state_dir_override = Some(state_dir);
        self
    }

    pub fn config(&self) -> &ClientConfig {
        &self.config
    }

    pub fn set_license_key(&self, key: String) -> Result<()> {
        let mut state = self.load_state();
        state.current_key = Some(key);
        self.save_state(&state)
    }

    pub fn current_key(&self) -> Option<String> {
        let state = self.load_state();
        state.current_key.or_else(|| self.config.license_key.clone())
    }

    pub fn refresh_entitlement(&self, app_name: &str, app_version: &str) -> LicenseSnapshot {
        let Some(key) = self.current_key() else {
            return LicenseSnapshot {
                current_key: None,
                claims: None,
                verification_state: VerificationState::MissingKey,
                effective_mode: EffectiveMode::PlayOnly,
                endpoint: None,
                message: "No license key configured; editor mode unavailable.".to_string(),
                server_notifications: Vec::new(),
            };
        };

        let install_id = match self.load_or_create_install_id() {
            Ok(value) => value,
            Err(err) => {
                return LicenseSnapshot {
                    current_key: Some(key),
                    claims: None,
                    verification_state: VerificationState::InvalidResponse,
                    effective_mode: EffectiveMode::PlayOnly,
                    endpoint: None,
                    message: format!("Failed to initialize install identity: {err}"),
                    server_notifications: Vec::new(),
                };
            }
        };
        let session_secret = self.load_session_secret(&install_id);
        let request = VerifyKeyRequest {
            key: key.clone(),
            claimed_ip: detect_best_effort_ip(&self.config.primary_url),
            os: detect_os_info(),
            device: DeviceFingerprint {
                install_id: install_id.clone(),
                hostname: detect_hostname(),
            },
            app: ClientAppInfo {
                name: app_name.to_string(),
                version: app_version.to_string(),
                channel: None,
            },
            session_secret,
        };

        let endpoints = preferred_endpoints(&self.config);
        let verifying_key = match decode_verifying_key(&self.config.public_key_b64) {
            Ok(key) => key,
            Err(err) => {
                return LicenseSnapshot {
                    current_key: Some(key),
                    claims: None,
                    verification_state: VerificationState::InvalidResponse,
                    effective_mode: EffectiveMode::PlayOnly,
                    endpoint: None,
                    message: format!("Invalid public key configuration: {err}"),
                    server_notifications: Vec::new(),
                };
            }
        };

        let mut errors = Vec::new();
        for endpoint in endpoints {
            match self.verify_against_endpoint(&endpoint, &request, &verifying_key) {
                Ok((response, claims)) => {
                    let server_notifications = self
                        .read_client_notifications(
                            &endpoint,
                            &key,
                            &claims.license_id,
                            &install_id,
                        )
                        .unwrap_or_else(|err| {
                            warn!("failed to read client notifications: {err}");
                            Vec::new()
                        });
                    let current_key = response
                        .replacement_key
                        .clone()
                        .or_else(|| claims.replacement_key.clone())
                        .unwrap_or_else(|| key.clone());
                    let mut state = self.load_state();
                    state.current_key = Some(current_key.clone());
                    state.cached_envelope = Some(response.envelope.clone());
                    state.last_endpoint = Some(endpoint.clone());
                    state.last_verified_at_epoch_s = Some(now_epoch_s());
                    if let Err(err) = self.save_state(&state) {
                        warn!("failed to persist licensing state: {err}");
                    }
                    if let Some(secret) = response.new_session_secret.clone() {
                        self.store_session_secret(&install_id, &secret);
                    }
                    return snapshot_from_live_response(
                        current_key,
                        claims,
                        endpoint,
                        response.message,
                        server_notifications,
                    );
                }
                Err(err) => errors.push(format!("{endpoint}: {err}")),
            }
        }

        snapshot_from_offline_fallback(self.load_state(), key, &verifying_key, errors)
    }

    pub fn cached_claims(&self) -> Option<EntitlementClaims> {
        let state = self.load_state();
        let envelope = state.cached_envelope?;
        let verifying_key = decode_verifying_key(&self.config.public_key_b64).ok()?;
        let claims = envelope.verify(&verifying_key).ok()?;
        if claims.expires_at_epoch_s <= now_epoch_s() {
            return None;
        }
        Some(claims)
    }

    pub fn report_unsupported_playback(
        &self,
        endpoint: Option<&str>,
        license_id: Option<&str>,
        source: &str,
        tracks: Vec<UnsupportedCodecTrack>,
        app_name: &str,
        app_version: &str,
    ) -> Result<()> {
        let install_id = self.load_or_create_install_id()?;
        let request = ClientUnsupportedPlaybackRequest {
            key: self.current_key(),
            license_id: license_id.map(ToOwned::to_owned),
            device_install_id: install_id,
            source: source.to_string(),
            app_name: app_name.to_string(),
            app_version: app_version.to_string(),
            tracks,
        };
        let endpoint = endpoint
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| self.config.primary_url.clone());
        self.http
            .post(format!("{endpoint}/api/v1/client/playback/unsupported"))
            .json(&request)
            .send()
            .with_context(|| format!("POST {endpoint}/api/v1/client/playback/unsupported"))?
            .error_for_status()
            .with_context(|| format!("unsupported playback report rejected by {endpoint}"))?;
        Ok(())
    }

    fn verify_against_endpoint(
        &self,
        endpoint: &str,
        request: &VerifyKeyRequest,
        verifying_key: &ed25519_dalek::VerifyingKey,
    ) -> Result<(VerifyKeyResponse, EntitlementClaims)> {
        let response = self
            .http
            .post(format!("{endpoint}/api/v1/verify"))
            .json(request)
            .send()
            .with_context(|| format!("POST {endpoint}/api/v1/verify"))?
            .error_for_status()
            .with_context(|| format!("verification rejected by {endpoint}"))?
            .json::<VerifyKeyResponse>()
            .with_context(|| format!("decode verification response from {endpoint}"))?;
        let claims = response.envelope.verify(verifying_key)?;
        Ok((response, claims))
    }

    fn read_client_notifications(
        &self,
        endpoint: &str,
        key: &str,
        license_id: &str,
        device_install_id: &str,
    ) -> Result<Vec<ClientNotification>> {
        let request = ClientNotificationReadRequest {
            key: key.to_string(),
            license_id: license_id.to_string(),
            device_install_id: device_install_id.to_string(),
        };
        self.http
            .post(format!("{endpoint}/api/v1/client/notifications/read"))
            .json(&request)
            .send()
            .with_context(|| format!("POST {endpoint}/api/v1/client/notifications/read"))?
            .error_for_status()
            .with_context(|| format!("client notification read rejected by {endpoint}"))?
            .json::<Vec<ClientNotification>>()
            .with_context(|| format!("decode client notifications from {endpoint}"))
    }

    fn state_dir(&self) -> Result<PathBuf> {
        if let Some(path) = &self.state_dir_override {
            Ok(path.clone())
        } else {
            self.config.state_dir()
        }
    }

    fn state_file(&self) -> Result<PathBuf> {
        Ok(self.state_dir()?.join("license_state.json"))
    }

    fn install_id_file(&self) -> Result<PathBuf> {
        Ok(self.state_dir()?.join("install_id"))
    }

    fn ensure_state_dir(&self) -> Result<PathBuf> {
        let path = self.state_dir()?;
        fs::create_dir_all(&path).with_context(|| format!("create {}", path.display()))?;
        Ok(path)
    }

    fn load_state(&self) -> ClientStateFile {
        let path = match self.state_file() {
            Ok(path) => path,
            Err(err) => {
                warn!("failed to resolve state file: {err}");
                return ClientStateFile::default();
            }
        };
        match fs::read_to_string(&path) {
            Ok(content) => serde_json::from_str(&content).unwrap_or_else(|err| {
                warn!("failed to parse {}: {err}", path.display());
                ClientStateFile::default()
            }),
            Err(err) if err.kind() == ErrorKind::NotFound => ClientStateFile::default(),
            Err(err) => {
                warn!("failed to read {}: {err}", path.display());
                ClientStateFile::default()
            }
        }
    }

    fn save_state(&self, state: &ClientStateFile) -> Result<()> {
        self.ensure_state_dir()?;
        let path = self.state_file()?;
        let content = serde_json::to_vec_pretty(state)?;
        fs::write(&path, content).with_context(|| format!("write {}", path.display()))
    }

    fn load_or_create_install_id(&self) -> Result<String> {
        self.ensure_state_dir()?;
        let path = self.install_id_file()?;
        match fs::read_to_string(&path) {
            Ok(value) => Ok(value.trim().to_string()),
            Err(err) if err.kind() == ErrorKind::NotFound => {
                let install_id = Uuid::new_v4().to_string();
                fs::write(&path, &install_id)
                    .with_context(|| format!("write {}", path.display()))?;
                Ok(install_id)
            }
            Err(err) => Err(err).with_context(|| format!("read {}", path.display())),
        }
    }

    fn load_session_secret(&self, install_id: &str) -> Option<String> {
        let entry = Entry::new(KEYRING_SERVICE, install_id).ok()?;
        entry.get_password().ok()
    }

    fn store_session_secret(&self, install_id: &str, secret: &str) {
        match Entry::new(KEYRING_SERVICE, install_id) {
            Ok(entry) => {
                if let Err(err) = entry.set_password(secret) {
                    warn!("failed to store session secret in keyring: {err}");
                }
            }
            Err(err) => warn!("failed to create keyring entry: {err}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LicenseSnapshot {
    pub current_key: Option<String>,
    pub claims: Option<EntitlementClaims>,
    pub verification_state: VerificationState,
    pub effective_mode: EffectiveMode,
    pub endpoint: Option<String>,
    pub message: String,
    pub server_notifications: Vec<ClientNotification>,
}

impl LicenseSnapshot {
    pub fn allows_editor(&self) -> bool {
        self.effective_mode == EffectiveMode::Editor
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EffectiveMode {
    PlayOnly,
    Editor,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerificationState {
    MissingKey,
    Verified,
    PendingConfirmation,
    Revoked,
    ReplacementIssued,
    OfflineFallback,
    InvalidResponse,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct ClientStateFile {
    current_key: Option<String>,
    cached_envelope: Option<SignedEntitlementEnvelope>,
    last_endpoint: Option<String>,
    last_verified_at_epoch_s: Option<u64>,
}

fn snapshot_from_live_response(
    current_key: String,
    claims: EntitlementClaims,
    endpoint: String,
    mut message: String,
    server_notifications: Vec<ClientNotification>,
) -> LicenseSnapshot {
    let verification_state = match claims.status {
        EntitlementStatus::Active => VerificationState::Verified,
        EntitlementStatus::PendingConfirmation => VerificationState::PendingConfirmation,
        EntitlementStatus::Revoked => VerificationState::Revoked,
        EntitlementStatus::ReplacementIssued => VerificationState::ReplacementIssued,
    };
    let effective_mode = if verification_state == VerificationState::Verified
        && claims.is_editor_enabled()
    {
        EffectiveMode::Editor
    } else {
        EffectiveMode::PlayOnly
    };
    if message.is_empty() {
        message = claims.message.clone();
    }
    LicenseSnapshot {
        current_key: Some(current_key),
        claims: Some(claims),
        verification_state,
        effective_mode,
        endpoint: Some(endpoint),
        message,
        server_notifications,
    }
}

fn snapshot_from_offline_fallback(
    state: ClientStateFile,
    key: String,
    verifying_key: &ed25519_dalek::VerifyingKey,
    errors: Vec<String>,
) -> LicenseSnapshot {
    let claims = state
        .cached_envelope
        .and_then(|envelope| envelope.verify(verifying_key).ok())
        .filter(|claims| claims.expires_at_epoch_s > now_epoch_s());
    let cached_message = claims
        .as_ref()
        .map(|claims| format!(" Cached entitlement status: {:?}.", claims.status))
        .unwrap_or_default();
    let error_message = if errors.is_empty() {
        "Licensing server unavailable.".to_string()
    } else {
        format!("Licensing server unavailable: {}.", errors.join(" | "))
    };
    LicenseSnapshot {
        current_key: Some(key),
        claims,
        verification_state: VerificationState::OfflineFallback,
        effective_mode: EffectiveMode::PlayOnly,
        endpoint: state.last_endpoint,
        message: format!("{error_message} Falling back to play-only mode.{cached_message}"),
        server_notifications: Vec::new(),
    }
}

fn preferred_endpoints(config: &ClientConfig) -> Vec<String> {
    if config.primary_url == config.backup_url {
        vec![config.primary_url.clone()]
    } else {
        vec![config.primary_url.clone(), config.backup_url.clone()]
    }
}

fn detect_os_info() -> ClientOsInfo {
    ClientOsInfo {
        family: env::consts::OS.to_string(),
        version: None,
        arch: env::consts::ARCH.to_string(),
    }
}

fn detect_hostname() -> Option<String> {
    env::var("HOSTNAME")
        .ok()
        .or_else(|| env::var("COMPUTERNAME").ok())
}

fn detect_best_effort_ip(endpoint: &str) -> Option<String> {
    let url = Url::parse(endpoint).ok()?;
    let host = url.host_str()?;
    let port = url.port_or_known_default()?;
    let bind_addr = if host.contains(':') { "[::]:0" } else { "0.0.0.0:0" };
    let socket = UdpSocket::bind(bind_addr).ok()?;
    socket.connect((host, port)).ok()?;
    socket.local_addr().ok().map(|addr| addr.ip().to_string())
}

fn now_epoch_s() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;
    use libsrs_app_config::ClientConfig;
    use libsrs_licensing_proto::{
        encode_verifying_key, EntitlementStatus, LicensedFeature, SignedEntitlementEnvelope,
    };
    use std::time::Duration;

    fn test_state_dir(name: &str) -> PathBuf {
        let path = env::temp_dir().join(format!("srs-lic-client-test-{name}-{}", Uuid::new_v4()));
        fs::create_dir_all(&path).expect("create temp dir");
        path
    }

    fn test_config() -> ClientConfig {
        let signing_key =
            libsrs_licensing_proto::decode_signing_key(libsrs_app_config::LOCALHOST_DEV_SIGNING_KEY_SEED_B64)
                .expect("decode dev seed");
        let mut config = ClientConfig::default();
        config.public_key_b64 = encode_verifying_key(&signing_key.verifying_key());
        config.primary_url = "http://localhost:9".to_string();
        config.backup_url = "http://127.0.0.1:9".to_string();
        config
    }

    fn sample_envelope() -> SignedEntitlementEnvelope {
        let signing_key =
            libsrs_licensing_proto::decode_signing_key(libsrs_app_config::LOCALHOST_DEV_SIGNING_KEY_SEED_B64)
                .expect("decode dev seed");
        SignedEntitlementEnvelope::sign(
            &EntitlementClaims {
                license_id: "license-1".to_string(),
                key_id: "key-1".to_string(),
                features: vec![LicensedFeature::Basic, LicensedFeature::EditorWorkspace],
                status: EntitlementStatus::Active,
                issued_at_epoch_s: now_epoch_s(),
                expires_at_epoch_s: now_epoch_s() + Duration::from_secs(3600).as_secs(),
                device_install_id: "install-1".to_string(),
                message: "ok".to_string(),
                replacement_key: None,
            },
            &signing_key,
        )
        .expect("sign envelope")
    }

    #[test]
    fn set_license_key_persists_current_key() {
        let client = LicensingClient::new(test_config())
            .expect("build client")
            .with_state_dir(test_state_dir("set-key"));
        client
            .set_license_key("abc-123".to_string())
            .expect("persist key");
        assert_eq!(client.current_key().as_deref(), Some("abc-123"));
    }

    #[test]
    fn offline_fallback_keeps_play_only_even_with_cached_editor_claims() {
        let client = LicensingClient::new(test_config())
            .expect("build client")
            .with_state_dir(test_state_dir("offline"));
        let state = ClientStateFile {
            current_key: Some("abc-123".to_string()),
            cached_envelope: Some(sample_envelope()),
            last_endpoint: Some("http://localhost:3000".to_string()),
            last_verified_at_epoch_s: Some(now_epoch_s()),
        };
        client.save_state(&state).expect("save state");
        let snapshot = client.refresh_entitlement("srs-player", "0.1.0");
        assert_eq!(snapshot.verification_state, VerificationState::OfflineFallback);
        assert_eq!(snapshot.effective_mode, EffectiveMode::PlayOnly);
        assert!(snapshot.claims.is_some());
    }
}
