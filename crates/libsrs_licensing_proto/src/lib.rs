use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LicensedFeature {
    Basic,
    EditorWorkspace,
    Encode,
    Decode,
    Compress,
    Import,
    Transcode,
    Mux,
    Demux,
    Select,
    FrameEdit,
    TimelineEdit,
    Export,
}

impl LicensedFeature {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Basic => "basic",
            Self::EditorWorkspace => "editor_workspace",
            Self::Encode => "encode",
            Self::Decode => "decode",
            Self::Compress => "compress",
            Self::Import => "import",
            Self::Transcode => "transcode",
            Self::Mux => "mux",
            Self::Demux => "demux",
            Self::Select => "select",
            Self::FrameEdit => "frame_edit",
            Self::TimelineEdit => "timeline_edit",
            Self::Export => "export",
        }
    }

    pub fn from_slug(value: &str) -> Option<Self> {
        match value {
            "basic" => Some(Self::Basic),
            "editor_workspace" => Some(Self::EditorWorkspace),
            "encode" => Some(Self::Encode),
            "decode" => Some(Self::Decode),
            "compress" => Some(Self::Compress),
            "import" => Some(Self::Import),
            "transcode" => Some(Self::Transcode),
            "mux" => Some(Self::Mux),
            "demux" => Some(Self::Demux),
            "select" => Some(Self::Select),
            "frame_edit" => Some(Self::FrameEdit),
            "timeline_edit" => Some(Self::TimelineEdit),
            "export" => Some(Self::Export),
            _ => None,
        }
    }

    pub fn all() -> Vec<Self> {
        vec![
            Self::Basic,
            Self::EditorWorkspace,
            Self::Encode,
            Self::Decode,
            Self::Compress,
            Self::Import,
            Self::Transcode,
            Self::Mux,
            Self::Demux,
            Self::Select,
            Self::FrameEdit,
            Self::TimelineEdit,
            Self::Export,
        ]
    }

    pub fn basic_defaults() -> Vec<Self> {
        vec![Self::Basic]
    }

    pub fn editor_defaults() -> Vec<Self> {
        vec![
            Self::Basic,
            Self::EditorWorkspace,
            Self::Encode,
            Self::Decode,
            Self::Compress,
            Self::Import,
            Self::Transcode,
            Self::Mux,
            Self::Demux,
            Self::Select,
            Self::FrameEdit,
            Self::TimelineEdit,
            Self::Export,
        ]
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdminStats {
    pub license_count: u64,
    pub key_count: u64,
    pub active_key_count: u64,
    pub installation_count: u64,
    pub trusted_installation_count: u64,
    pub pending_request_count: u64,
    pub audit_event_count: u64,
    pub playback_request_count: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdminRecordState {
    Active,
    Archived,
    Deleted,
}

impl AdminRecordState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Archived => "archived",
            Self::Deleted => "deleted",
        }
    }

    pub fn from_slug(value: &str) -> Option<Self> {
        match value {
            "active" => Some(Self::Active),
            "archived" => Some(Self::Archived),
            "deleted" => Some(Self::Deleted),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotificationDeliveryState {
    Queued,
    Sent,
    Delivered,
    Read,
}

impl NotificationDeliveryState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Sent => "sent",
            Self::Delivered => "delivered",
            Self::Read => "read",
        }
    }

    pub fn from_slug(value: &str) -> Option<Self> {
        match value {
            "queued" => Some(Self::Queued),
            "sent" => Some(Self::Sent),
            "delivered" => Some(Self::Delivered),
            "read" => Some(Self::Read),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdminLicenseRecord {
    pub license_id: String,
    pub owner_email: String,
    pub features: Vec<LicensedFeature>,
    pub active_key_count: u64,
    pub record_state: AdminRecordState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdminKeyRecord {
    pub key_id: String,
    pub license_id: String,
    pub key_value: String,
    pub key_version: i64,
    pub active: bool,
    pub created_at_epoch_s: u64,
    pub record_state: AdminRecordState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdminInstallationRecord {
    pub installation_id: String,
    pub license_id: String,
    pub device_install_id: String,
    pub first_seen_ip: Option<String>,
    pub last_seen_ip: Option<String>,
    pub os_family: String,
    pub os_arch: String,
    pub hostname: Option<String>,
    pub first_seen_epoch_s: u64,
    pub last_seen_epoch_s: u64,
    pub trusted: bool,
    pub record_state: AdminRecordState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdminPendingRequestRecord {
    pub request_id: String,
    pub license_id: String,
    pub device_install_id: String,
    pub requested_ip: Option<String>,
    pub requested_os: String,
    pub requested_arch: String,
    pub hostname: Option<String>,
    pub created_at_epoch_s: u64,
    pub expires_at_epoch_s: u64,
    pub approved_at_epoch_s: Option<u64>,
    pub record_state: AdminRecordState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdminAuditRecord {
    pub event_id: String,
    pub license_id: String,
    pub key_id: Option<String>,
    pub installation_id: Option<String>,
    pub event_type: String,
    pub event_payload_json: String,
    pub created_at_epoch_s: u64,
    pub record_state: AdminRecordState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdminNotificationRecord {
    pub email_id: String,
    pub license_id: String,
    pub request_id: Option<String>,
    pub recipient: String,
    pub subject: String,
    pub notification_state: NotificationDeliveryState,
    pub created_at_epoch_s: u64,
    pub sent_at_epoch_s: Option<u64>,
    pub delivered_at_epoch_s: Option<u64>,
    pub read_at_epoch_s: Option<u64>,
    pub record_state: AdminRecordState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnsupportedCodecTrack {
    pub track_id: u32,
    pub kind: String,
    pub codec: String,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientUnsupportedPlaybackRequest {
    pub key: Option<String>,
    pub license_id: Option<String>,
    pub device_install_id: String,
    pub source: String,
    pub app_name: String,
    pub app_version: String,
    pub tracks: Vec<UnsupportedCodecTrack>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdminPlaybackRequestRecord {
    pub playback_request_id: String,
    pub license_id: Option<String>,
    pub device_install_id: String,
    pub source: String,
    pub app_name: String,
    pub app_version: String,
    pub tracks_json: String,
    pub created_at_epoch_s: u64,
    pub record_state: AdminRecordState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdminSnapshot {
    pub stats: AdminStats,
    pub licenses: Vec<AdminLicenseRecord>,
    pub keys: Vec<AdminKeyRecord>,
    pub installations: Vec<AdminInstallationRecord>,
    pub pending_requests: Vec<AdminPendingRequestRecord>,
    pub audits: Vec<AdminAuditRecord>,
    pub notifications: Vec<AdminNotificationRecord>,
    pub playback_requests: Vec<AdminPlaybackRequestRecord>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdminUpdateLicenseFeaturesRequest {
    pub license_id: String,
    pub features: Vec<LicensedFeature>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdminUpdateKeyStatusRequest {
    pub key_id: String,
    pub active: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdminUpdateRecordStateRequest {
    pub state: AdminRecordState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdminCreateNotificationRequest {
    pub license_id: String,
    pub recipient: String,
    pub subject: String,
    pub body: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdminActionResponse {
    pub ok: bool,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntitlementStatus {
    Active,
    PendingConfirmation,
    Revoked,
    ReplacementIssued,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientOsInfo {
    pub family: String,
    pub version: Option<String>,
    pub arch: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientAppInfo {
    pub name: String,
    pub version: String,
    pub channel: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceFingerprint {
    pub install_id: String,
    pub hostname: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerifyKeyRequest {
    pub key: String,
    pub claimed_ip: Option<String>,
    pub os: ClientOsInfo,
    pub device: DeviceFingerprint,
    pub app: ClientAppInfo,
    pub session_secret: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerifyKeyResponse {
    pub envelope: SignedEntitlementEnvelope,
    pub replacement_key: Option<String>,
    pub new_session_secret: Option<String>,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientNotificationReadRequest {
    pub key: String,
    pub license_id: String,
    pub device_install_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientNotification {
    pub notification_id: String,
    pub subject: String,
    pub body: String,
    pub created_at_epoch_s: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IssueKeyRequest {
    pub email: String,
    pub requested_features: Option<Vec<LicensedFeature>>,
    pub registrant_os: Option<String>,
    pub registrant_ip: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IssueKeyResponse {
    pub license_id: String,
    pub key: String,
    pub features: Vec<LicensedFeature>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EntitlementClaims {
    pub license_id: String,
    pub key_id: String,
    pub features: Vec<LicensedFeature>,
    pub status: EntitlementStatus,
    pub issued_at_epoch_s: u64,
    pub expires_at_epoch_s: u64,
    pub device_install_id: String,
    pub message: String,
    pub replacement_key: Option<String>,
}

impl EntitlementClaims {
    pub fn allows_feature(&self, feature: LicensedFeature) -> bool {
        self.status == EntitlementStatus::Active && self.features.contains(&feature)
    }

    pub fn is_editor_enabled(&self) -> bool {
        self.allows_feature(LicensedFeature::EditorWorkspace)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignedEntitlementEnvelope {
    pub claims_json: String,
    pub signature_b64: String,
}

impl SignedEntitlementEnvelope {
    pub fn sign(
        claims: &EntitlementClaims,
        signing_key: &SigningKey,
    ) -> Result<Self, LicensingProtoError> {
        let claims_json = serde_json::to_string(claims)?;
        let signature = signing_key.sign(claims_json.as_bytes());
        Ok(Self {
            claims_json,
            signature_b64: BASE64.encode(signature.to_bytes()),
        })
    }

    pub fn verify(
        &self,
        verifying_key: &VerifyingKey,
    ) -> Result<EntitlementClaims, LicensingProtoError> {
        let signature_bytes = BASE64
            .decode(self.signature_b64.as_bytes())
            .map_err(LicensingProtoError::Base64)?;
        let signature = Signature::from_slice(&signature_bytes)
            .map_err(|_| LicensingProtoError::InvalidSignatureBytes)?;
        verifying_key
            .verify(self.claims_json.as_bytes(), &signature)
            .map_err(|_| LicensingProtoError::InvalidSignature)?;
        Ok(serde_json::from_str(&self.claims_json)?)
    }
}

pub fn decode_signing_key(seed_b64: &str) -> Result<SigningKey, LicensingProtoError> {
    let seed = BASE64
        .decode(seed_b64.as_bytes())
        .map_err(LicensingProtoError::Base64)?;
    let seed_len = seed.len();
    let seed: [u8; 32] = seed
        .try_into()
        .map_err(|_| LicensingProtoError::InvalidKeyLength(seed_len))?;
    Ok(SigningKey::from_bytes(&seed))
}

pub fn encode_verifying_key(verifying_key: &VerifyingKey) -> String {
    BASE64.encode(verifying_key.to_bytes())
}

pub fn decode_verifying_key(key_b64: &str) -> Result<VerifyingKey, LicensingProtoError> {
    let bytes = BASE64
        .decode(key_b64.as_bytes())
        .map_err(LicensingProtoError::Base64)?;
    let bytes_len = bytes.len();
    let bytes: [u8; 32] = bytes
        .try_into()
        .map_err(|_| LicensingProtoError::InvalidKeyLength(bytes_len))?;
    VerifyingKey::from_bytes(&bytes).map_err(|_| LicensingProtoError::InvalidPublicKey)
}

#[derive(Debug, Error)]
pub enum LicensingProtoError {
    #[error("base64 decode error: {0}")]
    Base64(#[from] base64::DecodeError),
    #[error("serialization error: {0}")]
    SerdeJson(#[from] serde_json::Error),
    #[error("invalid signature bytes")]
    InvalidSignatureBytes,
    #[error("invalid signature")]
    InvalidSignature,
    #[error("invalid key length {0}, expected 32 bytes")]
    InvalidKeyLength(usize),
    #[error("invalid public key")]
    InvalidPublicKey,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_signing_key() -> SigningKey {
        SigningKey::from_bytes(&[7_u8; 32])
    }

    fn sample_claims() -> EntitlementClaims {
        EntitlementClaims {
            license_id: "license-123".to_string(),
            key_id: "key-123".to_string(),
            features: LicensedFeature::editor_defaults(),
            status: EntitlementStatus::Active,
            issued_at_epoch_s: 1_700_000_000,
            expires_at_epoch_s: 1_700_086_400,
            device_install_id: "install-abc".to_string(),
            message: "ok".to_string(),
            replacement_key: None,
        }
    }

    #[test]
    fn sign_and_verify_round_trip() {
        let signing_key = test_signing_key();
        let verifying_key = signing_key.verifying_key();
        let envelope = SignedEntitlementEnvelope::sign(&sample_claims(), &signing_key)
            .expect("sign claims");
        let claims = envelope.verify(&verifying_key).expect("verify claims");
        assert!(claims.is_editor_enabled());
        assert!(claims.allows_feature(LicensedFeature::FrameEdit));
    }

    #[test]
    fn tampered_claims_fail_verification() {
        let signing_key = test_signing_key();
        let verifying_key = signing_key.verifying_key();
        let mut envelope = SignedEntitlementEnvelope::sign(&sample_claims(), &signing_key)
            .expect("sign claims");
        envelope.claims_json = envelope.claims_json.replace("license-123", "license-999");
        let err = envelope.verify(&verifying_key).expect_err("tamper must fail");
        assert!(matches!(err, LicensingProtoError::InvalidSignature));
    }

    #[test]
    fn verifying_key_round_trip() {
        let verifying_key = test_signing_key().verifying_key();
        let encoded = encode_verifying_key(&verifying_key);
        let decoded = decode_verifying_key(&encoded).expect("decode key");
        assert_eq!(decoded.to_bytes(), verifying_key.to_bytes());
    }
}
