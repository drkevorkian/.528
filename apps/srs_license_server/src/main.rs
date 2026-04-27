use std::fs;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Context, Result};
use axum::extract::{ConnectInfo, Form, Path as AxumPath, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Redirect};
use axum::routing::{get, post};
use axum::{Json, Router};
use lettre::transport::smtp::authentication::Credentials;
use lettre::{Message, SmtpTransport, Transport};
use libsrs_app_config::{ServerConfig, SrsConfig};
use libsrs_licensing_proto::{
    decode_signing_key, AdminActionResponse, AdminAuditRecord, AdminCreateNotificationRequest,
    AdminInstallationRecord, AdminKeyRecord, AdminLicenseRecord, AdminNotificationRecord,
    AdminPendingRequestRecord, AdminPlaybackRequestRecord, AdminRecordState, AdminSnapshot, AdminStats,
    AdminUpdateKeyStatusRequest, AdminUpdateLicenseFeaturesRequest, AdminUpdateRecordStateRequest,
    ClientNotification, ClientNotificationReadRequest, ClientUnsupportedPlaybackRequest,
    EntitlementClaims, EntitlementStatus,
    IssueKeyRequest, IssueKeyResponse, LicensedFeature, NotificationDeliveryState,
    SignedEntitlementEnvelope, VerifyKeyRequest, VerifyKeyResponse,
};
use rusqlite::{params, Connection, OptionalExtension, Transaction};
use serde::Deserialize;
use serde_json::json;
use sha2::{Digest, Sha256};
use tokio::net::TcpListener;
use tracing::info;
use uuid::Uuid;

const SCHEMA_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS licenses (
    license_id TEXT PRIMARY KEY,
    owner_email TEXT NOT NULL,
    registration_ip TEXT,
    registration_os TEXT,
    created_at_epoch_s INTEGER NOT NULL,
    record_state TEXT NOT NULL DEFAULT 'active',
    state_changed_at_epoch_s INTEGER
);

CREATE TABLE IF NOT EXISTS license_keys (
    key_id TEXT PRIMARY KEY,
    license_id TEXT NOT NULL,
    key_value TEXT NOT NULL UNIQUE,
    key_version INTEGER NOT NULL,
    active INTEGER NOT NULL,
    created_at_epoch_s INTEGER NOT NULL,
    rotated_from_key_id TEXT,
    record_state TEXT NOT NULL DEFAULT 'active',
    state_changed_at_epoch_s INTEGER,
    FOREIGN KEY (license_id) REFERENCES licenses (license_id)
);

CREATE TABLE IF NOT EXISTS license_features (
    license_id TEXT NOT NULL,
    feature_name TEXT NOT NULL,
    PRIMARY KEY (license_id, feature_name),
    FOREIGN KEY (license_id) REFERENCES licenses (license_id)
);

CREATE TABLE IF NOT EXISTS installations (
    installation_id TEXT PRIMARY KEY,
    license_id TEXT NOT NULL,
    device_install_id TEXT NOT NULL,
    first_seen_ip TEXT,
    last_seen_ip TEXT,
    os_family TEXT NOT NULL,
    os_arch TEXT NOT NULL,
    hostname TEXT,
    first_seen_epoch_s INTEGER NOT NULL,
    last_seen_epoch_s INTEGER NOT NULL,
    trusted INTEGER NOT NULL,
    session_secret_hash TEXT,
    record_state TEXT NOT NULL DEFAULT 'active',
    state_changed_at_epoch_s INTEGER,
    UNIQUE (license_id, device_install_id),
    FOREIGN KEY (license_id) REFERENCES licenses (license_id)
);

CREATE TABLE IF NOT EXISTS verification_requests (
    request_id TEXT PRIMARY KEY,
    license_id TEXT NOT NULL,
    source_key_id TEXT NOT NULL,
    device_install_id TEXT NOT NULL,
    requested_ip TEXT,
    requested_os TEXT NOT NULL,
    requested_arch TEXT NOT NULL,
    hostname TEXT,
    token TEXT NOT NULL UNIQUE,
    created_at_epoch_s INTEGER NOT NULL,
    expires_at_epoch_s INTEGER NOT NULL,
    approved_at_epoch_s INTEGER,
    replacement_license_id TEXT,
    replacement_key_id TEXT,
    record_state TEXT NOT NULL DEFAULT 'active',
    state_changed_at_epoch_s INTEGER,
    FOREIGN KEY (license_id) REFERENCES licenses (license_id)
);

CREATE TABLE IF NOT EXISTS audit_events (
    event_id TEXT PRIMARY KEY,
    license_id TEXT NOT NULL,
    key_id TEXT,
    installation_id TEXT,
    event_type TEXT NOT NULL,
    event_payload_json TEXT NOT NULL,
    created_at_epoch_s INTEGER NOT NULL,
    record_state TEXT NOT NULL DEFAULT 'active',
    state_changed_at_epoch_s INTEGER
);

CREATE TABLE IF NOT EXISTS email_outbox (
    email_id TEXT PRIMARY KEY,
    license_id TEXT NOT NULL,
    request_id TEXT,
    recipient TEXT NOT NULL,
    subject TEXT NOT NULL,
    body TEXT NOT NULL,
    sent_at_epoch_s INTEGER,
    delivered_at_epoch_s INTEGER,
    read_at_epoch_s INTEGER,
    notification_state TEXT NOT NULL DEFAULT 'queued',
    record_state TEXT NOT NULL DEFAULT 'active',
    state_changed_at_epoch_s INTEGER,
    created_at_epoch_s INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS playback_requests (
    playback_request_id TEXT PRIMARY KEY,
    license_id TEXT,
    device_install_id TEXT NOT NULL,
    source TEXT NOT NULL,
    app_name TEXT NOT NULL,
    app_version TEXT NOT NULL,
    tracks_json TEXT NOT NULL,
    created_at_epoch_s INTEGER NOT NULL,
    record_state TEXT NOT NULL DEFAULT 'active',
    state_changed_at_epoch_s INTEGER
);
"#;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let config = SrsConfig::load()?.server;
    let state = Arc::new(AppState::new(config)?);
    let bind_addr: SocketAddr = state
        .config
        .bind_addr
        .parse()
        .with_context(|| format!("parse bind addr {}", state.config.bind_addr))?;

    let app = Router::new()
        .route("/", get(index))
        .route("/healthz", get(healthz))
        .route("/issue", post(issue_form))
        .route("/confirm/{token}", get(confirm_request))
        .route("/admin", get(admin_dashboard))
        .route("/admin/licenses/features", post(update_license_features_handler))
        .route(
            "/admin/licenses/{license_id}/delete",
            post(delete_license_handler),
        )
        .route("/admin/keys/status", post(update_key_status_handler))
        .route("/admin/keys/{key_id}/delete", post(delete_key_handler))
        .route(
            "/admin/requests/{request_id}/approve",
            post(approve_request_handler),
        )
        .route(
            "/admin/requests/{request_id}/delete",
            post(delete_request_handler),
        )
        .route(
            "/admin/installations/{installation_id}/delete",
            post(delete_installation_handler),
        )
        .route(
            "/admin/audits/{event_id}/delete",
            post(delete_audit_handler),
        )
        .route("/api/v1/admin/snapshot", get(admin_snapshot_json))
        .route(
            "/api/v1/admin/notifications/create",
            post(create_notification_json),
        )
        .route(
            "/api/v1/admin/licenses/features",
            post(update_license_features_json),
        )
        .route(
            "/api/v1/admin/licenses/{license_id}/state",
            post(update_license_state_json),
        )
        .route(
            "/api/v1/admin/licenses/{license_id}/delete",
            post(delete_license_json),
        )
        .route("/api/v1/admin/keys/status", post(update_key_status_json))
        .route(
            "/api/v1/admin/keys/{key_id}/state",
            post(update_key_state_json),
        )
        .route(
            "/api/v1/admin/keys/{key_id}/delete",
            post(delete_key_json),
        )
        .route(
            "/api/v1/admin/requests/{request_id}/approve",
            post(approve_request_json),
        )
        .route(
            "/api/v1/admin/requests/{request_id}/state",
            post(update_request_state_json),
        )
        .route(
            "/api/v1/admin/requests/{request_id}/delete",
            post(delete_request_json),
        )
        .route(
            "/api/v1/admin/installations/{installation_id}/state",
            post(update_installation_state_json),
        )
        .route(
            "/api/v1/admin/installations/{installation_id}/delete",
            post(delete_installation_json),
        )
        .route(
            "/api/v1/admin/audits/{event_id}/state",
            post(update_audit_state_json),
        )
        .route(
            "/api/v1/admin/audits/{event_id}/delete",
            post(delete_audit_json),
        )
        .route("/api/v1/issue", post(issue_json))
        .route("/api/v1/verify", post(verify_json))
        .route(
            "/api/v1/client/notifications/read",
            post(read_client_notifications_json),
        )
        .route(
            "/api/v1/client/playback/unsupported",
            post(report_unsupported_playback_json),
        )
        .with_state(state);

    let listener = TcpListener::bind(bind_addr)
        .await
        .with_context(|| format!("bind {}", bind_addr))?;
    info!("srs_license_server listening on {}", bind_addr);
    axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>())
        .await
        .context("serve axum")?;
    Ok(())
}

#[derive(Clone)]
struct AppState {
    config: ServerConfig,
    db: Arc<Database>,
}

impl AppState {
    fn new(config: ServerConfig) -> Result<Self> {
        Ok(Self {
            db: Arc::new(Database::open(&config)?),
            config,
        })
    }
}

struct Database {
    conn: Mutex<Connection>,
}

impl Database {
    fn open(config: &ServerConfig) -> Result<Self> {
        let path = config.resolved_database_path();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
        }
        let conn = Connection::open(&path)
            .with_context(|| format!("open database {}", path.display()))?;
        conn.execute_batch(SCHEMA_SQL)
            .context("initialize database schema")?;
        migrate_record_state_columns(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    fn issue_license(&self, request: &IssueKeyRequest) -> Result<IssueKeyResponse> {
        let now = now_epoch_s();
        let license_id = new_id("lic");
        let key_id = new_id("key");
        let key = generate_key();
        let features = request
            .requested_features
            .clone()
            .unwrap_or_else(LicensedFeature::basic_defaults);

        let mut conn = self
            .conn
            .lock()
            .map_err(|_| anyhow!("database mutex poisoned"))?;
        let tx = conn.transaction()?;
        tx.execute(
            "INSERT INTO licenses (license_id, owner_email, registration_ip, registration_os, created_at_epoch_s)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                &license_id,
                request.email,
                request.registrant_ip,
                request.registrant_os,
                now as i64
            ],
        )?;
        tx.execute(
            "INSERT INTO license_keys (key_id, license_id, key_value, key_version, active, created_at_epoch_s, rotated_from_key_id)
             VALUES (?1, ?2, ?3, 1, 1, ?4, NULL)",
            params![&key_id, &license_id, &key, now as i64],
        )?;
        for feature in &features {
            tx.execute(
                "INSERT INTO license_features (license_id, feature_name) VALUES (?1, ?2)",
                params![&license_id, feature_name(*feature)],
            )?;
        }
        insert_audit_event(
            &tx,
            &license_id,
            Some(&key_id),
            None,
            "license_issued",
            json!({
                "email": request.email,
                "features": features.iter().map(|f| feature_name(*f)).collect::<Vec<_>>(),
            }),
        )?;
        tx.commit()?;

        Ok(IssueKeyResponse {
            license_id,
            key,
            features,
        })
    }

    fn confirm_request(&self, token: &str) -> Result<Option<String>> {
        let now = now_epoch_s();
        let mut conn = self
            .conn
            .lock()
            .map_err(|_| anyhow!("database mutex poisoned"))?;
        let tx = conn.transaction()?;
        let record = tx
            .query_row(
                "SELECT request_id, license_id, approved_at_epoch_s FROM verification_requests WHERE token = ?1",
                params![token],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Option<i64>>(2)?,
                    ))
                },
            )
            .optional()?;
        let Some((request_id, license_id, approved_at)) = record else {
            tx.commit()?;
            return Ok(None);
        };
        if approved_at.is_none() {
            tx.execute(
                "UPDATE verification_requests SET approved_at_epoch_s = ?1 WHERE request_id = ?2",
                params![now as i64, &request_id],
            )?;
            tx.execute(
                "UPDATE email_outbox
                 SET notification_state = 'read',
                     read_at_epoch_s = ?1,
                     record_state = 'deleted',
                     state_changed_at_epoch_s = ?1
                 WHERE request_id = ?2 AND record_state != 'deleted'",
                params![now as i64, &request_id],
            )?;
            insert_audit_event(
                &tx,
                &license_id,
                None,
                None,
                "verification_request_confirmed",
                json!({ "request_id": request_id }),
            )?;
        }
        tx.commit()?;
        Ok(Some(license_id))
    }

    fn verify_key(
        &self,
        config: &ServerConfig,
        request: &VerifyKeyRequest,
        remote_ip: Option<&str>,
    ) -> Result<VerifyKeyResponse> {
        let now = now_epoch_s();
        let mut conn = self
            .conn
            .lock()
            .map_err(|_| anyhow!("database mutex poisoned"))?;
        let tx = conn.transaction()?;

        let Some(key_row) = find_active_key(&tx, &request.key)? else {
            return Err(anyhow!("license key not found or inactive"));
        };

        let features = load_features(&tx, &key_row.license_id)?;
        let effective_ip = request
            .claimed_ip
            .as_deref()
            .or(remote_ip)
            .map(ToOwned::to_owned);
        let existing_installation =
            find_installation(&tx, &key_row.license_id, &request.device.install_id)?;
        let trusted_install_count = count_trusted_installations(&tx, &key_row.license_id)?;

        if trusted_install_count == 0 {
            let installation_id = upsert_trusted_installation(
                &tx,
                &key_row.license_id,
                request,
                effective_ip.as_deref(),
                now,
            )?;
            let new_session_secret =
                rotate_session_secret(&tx, &installation_id, request.session_secret.as_deref())?;
            insert_audit_event(
                &tx,
                &key_row.license_id,
                Some(&key_row.key_id),
                Some(&installation_id),
                "initial_installation_trusted",
                json!({
                    "device_install_id": request.device.install_id,
                    "ip": effective_ip,
                }),
            )?;
            let response = signed_response(
                config,
                &key_row.license_id,
                &key_row.key_id,
                &request.device.install_id,
                features,
                EntitlementStatus::Active,
                "Initial installation trusted.".to_string(),
                None,
                Some(new_session_secret),
            )?;
            tx.commit()?;
            return Ok(response);
        }

        if let Some(installation) = existing_installation {
            if installation.trusted {
                let session_mismatch = installation
                    .session_secret_hash
                    .as_deref()
                    .zip(request.session_secret.as_deref())
                    .is_some_and(|(expected, provided)| expected != hash_secret(provided));
                update_installation_last_seen(
                    &tx,
                    &installation.installation_id,
                    effective_ip.as_deref(),
                    request,
                    now,
                )?;
                if session_mismatch {
                    insert_audit_event(
                        &tx,
                        &key_row.license_id,
                        Some(&key_row.key_id),
                        Some(&installation.installation_id),
                        "session_secret_mismatch",
                        json!({ "device_install_id": request.device.install_id }),
                    )?;
                }
                let new_session_secret =
                    rotate_session_secret(&tx, &installation.installation_id, None)?;
                let response = signed_response(
                    config,
                    &key_row.license_id,
                    &key_row.key_id,
                    &request.device.install_id,
                    features,
                    EntitlementStatus::Active,
                    "Installation verified.".to_string(),
                    None,
                    Some(new_session_secret),
                )?;
                tx.commit()?;
                return Ok(response);
            }
        }

        if let Some(pending) =
            find_pending_request(&tx, &key_row.license_id, &request.device.install_id)?
        {
            if pending.approved_at_epoch_s.is_some() {
                let installation_id = upsert_trusted_installation(
                    &tx,
                    &key_row.license_id,
                    request,
                    effective_ip.as_deref(),
                    now,
                )?;
                let new_session_secret =
                    rotate_session_secret(&tx, &installation_id, request.session_secret.as_deref())?;
                insert_audit_event(
                    &tx,
                    &key_row.license_id,
                    Some(&key_row.key_id),
                    Some(&installation_id),
                    "pending_request_promoted_to_trusted",
                    json!({ "request_id": pending.request_id }),
                )?;
                let response = signed_response(
                    config,
                    &key_row.license_id,
                    &key_row.key_id,
                    &request.device.install_id,
                    features,
                    EntitlementStatus::Active,
                    "Confirmation received; editor eligibility restored.".to_string(),
                    None,
                    Some(new_session_secret),
                )?;
                tx.commit()?;
                return Ok(response);
            }

            if now >= pending.expires_at_epoch_s as u64 {
                let replacement = issue_replacement_license(
                    &tx,
                    &key_row.owner_email,
                    &key_row.key_id,
                    request,
                    effective_ip.as_deref(),
                    now,
                )?;
                tx.execute(
                    "UPDATE verification_requests
                     SET replacement_license_id = ?1, replacement_key_id = ?2
                     WHERE request_id = ?3",
                    params![
                        &replacement.license_id,
                        &replacement.key_id,
                        &pending.request_id
                    ],
                )?;
                insert_audit_event(
                    &tx,
                    &replacement.license_id,
                    Some(&replacement.key_id),
                    Some(&replacement.installation_id),
                    "replacement_key_issued",
                    json!({
                        "superseded_request_id": pending.request_id,
                        "replacement_key": replacement.key_value,
                    }),
                )?;
                let response = signed_response(
                    config,
                    &replacement.license_id,
                    &replacement.key_id,
                    &request.device.install_id,
                    LicensedFeature::basic_defaults(),
                    EntitlementStatus::ReplacementIssued,
                    "Confirmation window expired; a replacement basic key was issued.".to_string(),
                    Some(replacement.key_value.clone()),
                    Some(replacement.session_secret),
                )?;
                tx.commit()?;
                return Ok(response);
            }

            let response = signed_response(
                config,
                &key_row.license_id,
                &key_row.key_id,
                &request.device.install_id,
                LicensedFeature::basic_defaults(),
                EntitlementStatus::PendingConfirmation,
                "Confirmation email sent; editor mode remains disabled until approved.".to_string(),
                None,
                None,
            )?;
            tx.commit()?;
            return Ok(response);
        }

        let token = new_id("confirm");
        let request_id = new_id("req");
        let expires_at = now + (config.confirmation_window_hours * 3600);
        tx.execute(
            "INSERT INTO verification_requests
             (request_id, license_id, source_key_id, device_install_id, requested_ip, requested_os, requested_arch, hostname, token, created_at_epoch_s, expires_at_epoch_s, approved_at_epoch_s, replacement_license_id, replacement_key_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, NULL, NULL, NULL)",
            params![
                &request_id,
                &key_row.license_id,
                &key_row.key_id,
                &request.device.install_id,
                &effective_ip,
                &request.os.family,
                &request.os.arch,
                &request.device.hostname,
                &token,
                now as i64,
                expires_at as i64,
            ],
        )?;
        let confirmation_link = format!("{}/confirm/{}", config.base_url.trim_end_matches('/'), token);
        let subject = "Was this you? Confirm a new SRS installation".to_string();
        let body = format!(
            "A new installation requested access.\n\nKey: {}\nIP: {}\nOS: {}/{}\nHostname: {}\n\nConfirm: {}\n\nIf you do nothing for {} hours, the requester will receive a separate basic key.",
            request.key,
            effective_ip.clone().unwrap_or_else(|| "unknown".to_string()),
            request.os.family,
            request.os.arch,
            request.device.hostname.clone().unwrap_or_else(|| "unknown".to_string()),
            confirmation_link,
            config.confirmation_window_hours,
        );
        let email_id = enqueue_email(
            &tx,
            &key_row.license_id,
            Some(&request_id),
            &key_row.owner_email,
            &subject,
            &body,
            now,
        )?;
        insert_audit_event(
            &tx,
            &key_row.license_id,
            Some(&key_row.key_id),
            None,
            "verification_request_created",
            json!({
                "request_id": request_id,
                "device_install_id": request.device.install_id,
                "confirmation_link": confirmation_link,
            }),
        )?;
        let response = signed_response(
            config,
            &key_row.license_id,
            &key_row.key_id,
            &request.device.install_id,
            LicensedFeature::basic_defaults(),
            EntitlementStatus::PendingConfirmation,
            "New origin detected. Confirmation email sent to the original owner.".to_string(),
            None,
            None,
        )?;
        tx.commit()?;
        drop(conn);
        self.mark_notification_sent(&email_id)?;
        if deliver_email(&key_row.owner_email, &subject, &body, config) {
            self.mark_notification_delivered(&email_id)?;
        }
        Ok(response)
    }

    fn admin_snapshot(&self) -> Result<AdminSnapshot> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| anyhow!("database mutex poisoned"))?;

        let stats = AdminStats {
            license_count: scalar_count(&conn, "SELECT COUNT(*) FROM licenses")?,
            key_count: scalar_count(&conn, "SELECT COUNT(*) FROM license_keys")?,
            active_key_count: scalar_count(&conn, "SELECT COUNT(*) FROM license_keys WHERE active = 1")?,
            installation_count: scalar_count(&conn, "SELECT COUNT(*) FROM installations")?,
            trusted_installation_count: scalar_count(
                &conn,
                "SELECT COUNT(*) FROM installations WHERE trusted = 1",
            )?,
            pending_request_count: scalar_count(
                &conn,
                "SELECT COUNT(*) FROM verification_requests WHERE approved_at_epoch_s IS NULL",
            )?,
            audit_event_count: scalar_count(&conn, "SELECT COUNT(*) FROM audit_events")?,
            playback_request_count: scalar_count(&conn, "SELECT COUNT(*) FROM playback_requests")?,
        };

        let mut licenses_stmt = conn.prepare(
            "SELECT
                l.license_id,
                l.owner_email,
                l.record_state,
                (
                    SELECT COUNT(*)
                    FROM license_keys lk
                    WHERE lk.license_id = l.license_id AND lk.active = 1 AND lk.record_state = 'active'
                )
             FROM licenses l
             ORDER BY
                CASE l.record_state WHEN 'active' THEN 0 WHEN 'archived' THEN 1 ELSE 2 END,
                l.created_at_epoch_s DESC",
        )?;
        let licenses = licenses_stmt
            .query_map([], |row| {
                Ok(AdminLicenseRecord {
                    license_id: row.get(0)?,
                    owner_email: row.get(1)?,
                    features: Vec::new(),
                    active_key_count: row.get::<_, i64>(3)? as u64,
                    record_state: parse_record_state(&row.get::<_, String>(2)?),
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?
            .into_iter()
            .map(|mut license| {
                license.features = load_features_conn(&conn, &license.license_id)?;
                Ok(license)
            })
            .collect::<Result<Vec<_>>>()?;

        let mut keys_stmt = conn.prepare(
            "SELECT key_id, license_id, key_value, key_version, active, created_at_epoch_s, record_state
             FROM license_keys
             ORDER BY
                CASE record_state WHEN 'active' THEN 0 WHEN 'archived' THEN 1 ELSE 2 END,
                created_at_epoch_s DESC
             LIMIT 200",
        )?;
        let keys = keys_stmt
            .query_map([], |row| {
                Ok(AdminKeyRecord {
                    key_id: row.get(0)?,
                    license_id: row.get(1)?,
                    key_value: row.get(2)?,
                    key_version: row.get::<_, i64>(3)?,
                    active: row.get::<_, i64>(4)? == 1,
                    created_at_epoch_s: row.get::<_, i64>(5)? as u64,
                    record_state: parse_record_state(&row.get::<_, String>(6)?),
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        let mut installations_stmt = conn.prepare(
            "SELECT installation_id, license_id, device_install_id, first_seen_ip, last_seen_ip, os_family, os_arch, hostname, first_seen_epoch_s, last_seen_epoch_s, trusted, record_state
             FROM installations
             ORDER BY
                CASE record_state WHEN 'active' THEN 0 WHEN 'archived' THEN 1 ELSE 2 END,
                last_seen_epoch_s DESC
             LIMIT 200",
        )?;
        let installations = installations_stmt
            .query_map([], |row| {
                Ok(AdminInstallationRecord {
                    installation_id: row.get(0)?,
                    license_id: row.get(1)?,
                    device_install_id: row.get(2)?,
                    first_seen_ip: row.get(3)?,
                    last_seen_ip: row.get(4)?,
                    os_family: row.get(5)?,
                    os_arch: row.get(6)?,
                    hostname: row.get(7)?,
                    first_seen_epoch_s: row.get::<_, i64>(8)? as u64,
                    last_seen_epoch_s: row.get::<_, i64>(9)? as u64,
                    trusted: row.get::<_, i64>(10)? == 1,
                    record_state: parse_record_state(&row.get::<_, String>(11)?),
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        let mut requests_stmt = conn.prepare(
            "SELECT request_id, license_id, device_install_id, requested_ip, requested_os, requested_arch, hostname, created_at_epoch_s, expires_at_epoch_s, approved_at_epoch_s, record_state
             FROM verification_requests
             ORDER BY
                CASE record_state WHEN 'active' THEN 0 WHEN 'archived' THEN 1 ELSE 2 END,
                created_at_epoch_s DESC
             LIMIT 200",
        )?;
        let pending_requests = requests_stmt
            .query_map([], |row| {
                Ok(AdminPendingRequestRecord {
                    request_id: row.get(0)?,
                    license_id: row.get(1)?,
                    device_install_id: row.get(2)?,
                    requested_ip: row.get(3)?,
                    requested_os: row.get(4)?,
                    requested_arch: row.get(5)?,
                    hostname: row.get(6)?,
                    created_at_epoch_s: row.get::<_, i64>(7)? as u64,
                    expires_at_epoch_s: row.get::<_, i64>(8)? as u64,
                    approved_at_epoch_s: row.get::<_, Option<i64>>(9)?.map(|value| value as u64),
                    record_state: parse_record_state(&row.get::<_, String>(10)?),
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        let mut audits_stmt = conn.prepare(
            "SELECT event_id, license_id, key_id, installation_id, event_type, event_payload_json, created_at_epoch_s, record_state
             FROM audit_events
             ORDER BY
                CASE record_state WHEN 'active' THEN 0 WHEN 'archived' THEN 1 ELSE 2 END,
                created_at_epoch_s DESC
             LIMIT 200",
        )?;
        let audits = audits_stmt
            .query_map([], |row| {
                Ok(AdminAuditRecord {
                    event_id: row.get(0)?,
                    license_id: row.get(1)?,
                    key_id: row.get(2)?,
                    installation_id: row.get(3)?,
                    event_type: row.get(4)?,
                    event_payload_json: row.get(5)?,
                    created_at_epoch_s: row.get::<_, i64>(6)? as u64,
                    record_state: parse_record_state(&row.get::<_, String>(7)?),
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        let mut notifications_stmt = conn.prepare(
            "SELECT email_id, license_id, request_id, recipient, subject, notification_state, created_at_epoch_s, sent_at_epoch_s, delivered_at_epoch_s, read_at_epoch_s, record_state
             FROM email_outbox
             ORDER BY created_at_epoch_s DESC
             LIMIT 200",
        )?;
        let notifications = notifications_stmt
            .query_map([], |row| {
                Ok(AdminNotificationRecord {
                    email_id: row.get(0)?,
                    license_id: row.get(1)?,
                    request_id: row.get(2)?,
                    recipient: row.get(3)?,
                    subject: row.get(4)?,
                    notification_state: parse_notification_state(&row.get::<_, String>(5)?),
                    created_at_epoch_s: row.get::<_, i64>(6)? as u64,
                    sent_at_epoch_s: row.get::<_, Option<i64>>(7)?.map(|value| value as u64),
                    delivered_at_epoch_s: row.get::<_, Option<i64>>(8)?.map(|value| value as u64),
                    read_at_epoch_s: row.get::<_, Option<i64>>(9)?.map(|value| value as u64),
                    record_state: parse_record_state(&row.get::<_, String>(10)?),
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        let mut playback_stmt = conn.prepare(
            "SELECT playback_request_id, license_id, device_install_id, source, app_name, app_version, tracks_json, created_at_epoch_s, record_state
             FROM playback_requests
             ORDER BY
                CASE record_state WHEN 'active' THEN 0 WHEN 'archived' THEN 1 ELSE 2 END,
                created_at_epoch_s DESC
             LIMIT 200",
        )?;
        let playback_requests = playback_stmt
            .query_map([], |row| {
                Ok(AdminPlaybackRequestRecord {
                    playback_request_id: row.get(0)?,
                    license_id: row.get(1)?,
                    device_install_id: row.get(2)?,
                    source: row.get(3)?,
                    app_name: row.get(4)?,
                    app_version: row.get(5)?,
                    tracks_json: row.get(6)?,
                    created_at_epoch_s: row.get::<_, i64>(7)? as u64,
                    record_state: parse_record_state(&row.get::<_, String>(8)?),
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        Ok(AdminSnapshot {
            stats,
            licenses,
            keys,
            installations,
            pending_requests,
            audits,
            notifications,
            playback_requests,
        })
    }

    fn update_license_features(&self, license_id: &str, features: &[LicensedFeature]) -> Result<()> {
        let mut conn = self
            .conn
            .lock()
            .map_err(|_| anyhow!("database mutex poisoned"))?;
        let tx = conn.transaction()?;
        let exists = scalar_count_with_param(&tx, "SELECT COUNT(*) FROM licenses WHERE license_id = ?1", license_id)?;
        if exists == 0 {
            return Err(anyhow!("license not found"));
        }
        tx.execute(
            "DELETE FROM license_features WHERE license_id = ?1",
            params![license_id],
        )?;
        for feature in features {
            tx.execute(
                "INSERT INTO license_features (license_id, feature_name) VALUES (?1, ?2)",
                params![license_id, feature.as_str()],
            )?;
        }
        insert_audit_event(
            &tx,
            license_id,
            None,
            None,
            "admin_features_updated",
            json!({
                "features": features.iter().map(|feature| feature.as_str()).collect::<Vec<_>>(),
            }),
        )?;
        tx.commit()?;
        Ok(())
    }

    fn set_key_active(&self, key_id: &str, active: bool) -> Result<()> {
        let mut conn = self
            .conn
            .lock()
            .map_err(|_| anyhow!("database mutex poisoned"))?;
        let tx = conn.transaction()?;
        let Some(license_id) = tx
            .query_row(
                "SELECT license_id FROM license_keys WHERE key_id = ?1",
                params![key_id],
                |row| row.get::<_, String>(0),
            )
            .optional()?
        else {
            return Err(anyhow!("key not found"));
        };
        tx.execute(
            "UPDATE license_keys SET active = ?1 WHERE key_id = ?2",
            params![if active { 1 } else { 0 }, key_id],
        )?;
        insert_audit_event(
            &tx,
            &license_id,
            Some(key_id),
            None,
            "admin_key_status_updated",
            json!({ "active": active }),
        )?;
        tx.commit()?;
        Ok(())
    }

    fn approve_request_by_id(&self, request_id: &str) -> Result<()> {
        let now = now_epoch_s();
        let mut conn = self
            .conn
            .lock()
            .map_err(|_| anyhow!("database mutex poisoned"))?;
        let tx = conn.transaction()?;
        let Some((license_id, approved_at)) = tx
            .query_row(
                "SELECT license_id, approved_at_epoch_s FROM verification_requests WHERE request_id = ?1",
                params![request_id],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<i64>>(1)?)),
            )
            .optional()?
        else {
            return Err(anyhow!("verification request not found"));
        };
        if approved_at.is_none() {
            tx.execute(
                "UPDATE verification_requests SET approved_at_epoch_s = ?1 WHERE request_id = ?2",
                params![now as i64, request_id],
            )?;
            insert_audit_event(
                &tx,
                &license_id,
                None,
                None,
                "admin_request_approved",
                json!({ "request_id": request_id }),
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    fn set_license_record_state(
        &self,
        license_id: &str,
        state: AdminRecordState,
    ) -> Result<()> {
        let now = now_epoch_s();
        let mut conn = self
            .conn
            .lock()
            .map_err(|_| anyhow!("database mutex poisoned"))?;
        let tx = conn.transaction()?;
        let exists = scalar_count_with_param(
            &tx,
            "SELECT COUNT(*) FROM licenses WHERE license_id = ?1",
            license_id,
        )?;
        if exists == 0 {
            return Err(anyhow!("license not found"));
        }

        tx.execute(
            "UPDATE licenses SET record_state = ?1, state_changed_at_epoch_s = ?2 WHERE license_id = ?3",
            params![state.as_str(), now as i64, license_id],
        )?;
        tx.execute(
            "UPDATE license_keys SET record_state = ?1, state_changed_at_epoch_s = ?2 WHERE license_id = ?3",
            params![state.as_str(), now as i64, license_id],
        )?;
        tx.execute(
            "UPDATE installations SET record_state = ?1, state_changed_at_epoch_s = ?2 WHERE license_id = ?3",
            params![state.as_str(), now as i64, license_id],
        )?;
        tx.execute(
            "UPDATE verification_requests SET record_state = ?1, state_changed_at_epoch_s = ?2 WHERE license_id = ?3",
            params![state.as_str(), now as i64, license_id],
        )?;
        tx.execute(
            "UPDATE audit_events SET record_state = ?1, state_changed_at_epoch_s = ?2 WHERE license_id = ?3",
            params![state.as_str(), now as i64, license_id],
        )?;
        insert_audit_event(
            &tx,
            license_id,
            None,
            None,
            "admin_license_state_updated",
            json!({ "state": state.as_str() }),
        )?;
        tx.commit()?;
        Ok(())
    }

    fn set_key_record_state(&self, key_id: &str, state: AdminRecordState) -> Result<()> {
        let now = now_epoch_s();
        let mut conn = self
            .conn
            .lock()
            .map_err(|_| anyhow!("database mutex poisoned"))?;
        let tx = conn.transaction()?;
        let Some(license_id) = tx
            .query_row(
                "SELECT license_id FROM license_keys WHERE key_id = ?1",
                params![key_id],
                |row| row.get::<_, String>(0),
            )
            .optional()?
        else {
            return Err(anyhow!("key not found"));
        };
        tx.execute(
            "UPDATE license_keys SET record_state = ?1, state_changed_at_epoch_s = ?2 WHERE key_id = ?3",
            params![state.as_str(), now as i64, key_id],
        )?;
        insert_audit_event(
            &tx,
            &license_id,
            Some(key_id),
            None,
            "admin_key_state_updated",
            json!({ "state": state.as_str() }),
        )?;
        tx.commit()?;
        Ok(())
    }

    fn set_installation_record_state(
        &self,
        installation_id: &str,
        state: AdminRecordState,
    ) -> Result<()> {
        let now = now_epoch_s();
        let mut conn = self
            .conn
            .lock()
            .map_err(|_| anyhow!("database mutex poisoned"))?;
        let tx = conn.transaction()?;
        let Some(license_id) = tx
            .query_row(
                "SELECT license_id FROM installations WHERE installation_id = ?1",
                params![installation_id],
                |row| row.get::<_, String>(0),
            )
            .optional()?
        else {
            return Err(anyhow!("installation not found"));
        };
        tx.execute(
            "UPDATE installations SET record_state = ?1, state_changed_at_epoch_s = ?2 WHERE installation_id = ?3",
            params![state.as_str(), now as i64, installation_id],
        )?;
        insert_audit_event(
            &tx,
            &license_id,
            None,
            Some(installation_id),
            "admin_installation_state_updated",
            json!({ "state": state.as_str() }),
        )?;
        tx.commit()?;
        Ok(())
    }

    fn set_request_record_state(
        &self,
        request_id: &str,
        state: AdminRecordState,
    ) -> Result<()> {
        let now = now_epoch_s();
        let mut conn = self
            .conn
            .lock()
            .map_err(|_| anyhow!("database mutex poisoned"))?;
        let tx = conn.transaction()?;
        let Some(license_id) = tx
            .query_row(
                "SELECT license_id FROM verification_requests WHERE request_id = ?1",
                params![request_id],
                |row| row.get::<_, String>(0),
            )
            .optional()?
        else {
            return Err(anyhow!("verification request not found"));
        };
        tx.execute(
            "UPDATE verification_requests SET record_state = ?1, state_changed_at_epoch_s = ?2 WHERE request_id = ?3",
            params![state.as_str(), now as i64, request_id],
        )?;
        insert_audit_event(
            &tx,
            &license_id,
            None,
            None,
            "admin_request_state_updated",
            json!({ "request_id": request_id, "state": state.as_str() }),
        )?;
        tx.commit()?;
        Ok(())
    }

    fn set_audit_record_state(&self, event_id: &str, state: AdminRecordState) -> Result<()> {
        let now = now_epoch_s();
        let mut conn = self
            .conn
            .lock()
            .map_err(|_| anyhow!("database mutex poisoned"))?;
        let tx = conn.transaction()?;
        let Some(license_id) = tx
            .query_row(
                "SELECT license_id FROM audit_events WHERE event_id = ?1",
                params![event_id],
                |row| row.get::<_, String>(0),
            )
            .optional()?
        else {
            return Err(anyhow!("audit event not found"));
        };
        tx.execute(
            "UPDATE audit_events SET record_state = ?1, state_changed_at_epoch_s = ?2 WHERE event_id = ?3",
            params![state.as_str(), now as i64, event_id],
        )?;
        insert_audit_event(
            &tx,
            &license_id,
            None,
            None,
            "admin_audit_state_updated",
            json!({ "event_id": event_id, "state": state.as_str() }),
        )?;
        tx.commit()?;
        Ok(())
    }

    fn delete_license(&self, license_id: &str) -> Result<()> {
        self.set_license_record_state(license_id, AdminRecordState::Deleted)
    }

    fn delete_key(&self, key_id: &str) -> Result<()> {
        self.set_key_record_state(key_id, AdminRecordState::Deleted)
    }

    fn delete_installation(&self, installation_id: &str) -> Result<()> {
        self.set_installation_record_state(installation_id, AdminRecordState::Deleted)
    }

    fn delete_request(&self, request_id: &str) -> Result<()> {
        self.set_request_record_state(request_id, AdminRecordState::Deleted)
    }

    fn delete_audit(&self, event_id: &str) -> Result<()> {
        self.set_audit_record_state(event_id, AdminRecordState::Deleted)
    }

    fn create_and_send_notification(
        &self,
        config: &ServerConfig,
        request: &AdminCreateNotificationRequest,
    ) -> Result<String> {
        let now = now_epoch_s();
        let mut conn = self
            .conn
            .lock()
            .map_err(|_| anyhow!("database mutex poisoned"))?;
        let tx = conn.transaction()?;
        let exists = scalar_count_with_param(
            &tx,
            "SELECT COUNT(*) FROM licenses WHERE license_id = ?1",
            &request.license_id,
        )?;
        if exists == 0 {
            return Err(anyhow!("license not found"));
        }
        if request.recipient.trim().is_empty() {
            return Err(anyhow!("notification recipient is required"));
        }
        if request.subject.trim().is_empty() {
            return Err(anyhow!("notification subject is required"));
        }
        if request.body.trim().is_empty() {
            return Err(anyhow!("notification body is required"));
        }

        let email_id = enqueue_email(
            &tx,
            &request.license_id,
            None,
            request.recipient.trim(),
            request.subject.trim(),
            request.body.trim(),
            now,
        )?;
        insert_audit_event(
            &tx,
            &request.license_id,
            None,
            None,
            "admin_notification_created",
            json!({
                "email_id": email_id,
                "recipient": request.recipient,
                "subject": request.subject,
            }),
        )?;
        tx.commit()?;
        drop(conn);

        self.mark_notification_sent(&email_id)?;
        if deliver_email(
            request.recipient.trim(),
            request.subject.trim(),
            request.body.trim(),
            config,
        ) {
            self.mark_notification_delivered(&email_id)?;
        }

        Ok(email_id)
    }

    fn read_client_notifications(
        &self,
        request: &ClientNotificationReadRequest,
    ) -> Result<Vec<ClientNotification>> {
        let now = now_epoch_s();
        let mut conn = self
            .conn
            .lock()
            .map_err(|_| anyhow!("database mutex poisoned"))?;
        let tx = conn.transaction()?;
        let Some(key_row) = find_active_key(&tx, &request.key)? else {
            return Err(anyhow!("license key not found or inactive"));
        };
        if key_row.license_id != request.license_id {
            return Err(anyhow!("license key does not match requested license"));
        }

        let mut stmt = tx.prepare(
            "SELECT email_id, subject, body, created_at_epoch_s
             FROM email_outbox
             WHERE license_id = ?1
               AND request_id IS NULL
               AND record_state = 'active'
               AND notification_state IN ('sent', 'delivered')
             ORDER BY created_at_epoch_s ASC",
        )?;
        let notifications = stmt
            .query_map(params![&request.license_id], |row| {
                Ok(ClientNotification {
                    notification_id: row.get(0)?,
                    subject: row.get(1)?,
                    body: row.get(2)?,
                    created_at_epoch_s: row.get::<_, i64>(3)? as u64,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(stmt);

        for notification in &notifications {
            tx.execute(
                "UPDATE email_outbox
                 SET notification_state = 'read',
                     read_at_epoch_s = ?1,
                     record_state = 'deleted',
                     state_changed_at_epoch_s = ?1
                 WHERE email_id = ?2",
                params![now as i64, &notification.notification_id],
            )?;
            insert_audit_event(
                &tx,
                &request.license_id,
                Some(&key_row.key_id),
                None,
                "client_notification_read",
                json!({
                    "email_id": notification.notification_id,
                    "device_install_id": request.device_install_id,
                }),
            )?;
        }
        tx.commit()?;
        Ok(notifications)
    }

    fn record_unsupported_playback(
        &self,
        request: &ClientUnsupportedPlaybackRequest,
    ) -> Result<String> {
        let now = now_epoch_s();
        let playback_request_id = new_id("playback");
        let mut conn = self
            .conn
            .lock()
            .map_err(|_| anyhow!("database mutex poisoned"))?;
        let tx = conn.transaction()?;
        let license_id = if let Some(key) = &request.key {
            match find_active_key(&tx, key)? {
                Some(key_row) => Some(key_row.license_id),
                None => request.license_id.clone(),
            }
        } else {
            request.license_id.clone()
        };
        let tracks_json = serde_json::to_string(&request.tracks)?;
        tx.execute(
            "INSERT INTO playback_requests
             (playback_request_id, license_id, device_install_id, source, app_name, app_version, tracks_json, created_at_epoch_s, record_state, state_changed_at_epoch_s)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'active', ?8)",
            params![
                &playback_request_id,
                &license_id,
                &request.device_install_id,
                &request.source,
                &request.app_name,
                &request.app_version,
                &tracks_json,
                now as i64,
            ],
        )?;
        if let Some(license_id) = &license_id {
            insert_audit_event(
                &tx,
                license_id,
                None,
                None,
                "unsupported_playback_requested",
                json!({
                    "playback_request_id": playback_request_id,
                    "source": request.source,
                    "tracks": request.tracks,
                }),
            )?;
        }
        tx.commit()?;
        Ok(playback_request_id)
    }

    fn mark_notification_sent(&self, email_id: &str) -> Result<()> {
        let now = now_epoch_s();
        let conn = self
            .conn
            .lock()
            .map_err(|_| anyhow!("database mutex poisoned"))?;
        conn.execute(
            "UPDATE email_outbox
             SET sent_at_epoch_s = COALESCE(sent_at_epoch_s, ?1),
                 notification_state = 'sent',
                 state_changed_at_epoch_s = ?1
             WHERE email_id = ?2",
            params![now as i64, email_id],
        )?;
        Ok(())
    }

    fn mark_notification_delivered(&self, email_id: &str) -> Result<()> {
        let now = now_epoch_s();
        let conn = self
            .conn
            .lock()
            .map_err(|_| anyhow!("database mutex poisoned"))?;
        conn.execute(
            "UPDATE email_outbox
             SET sent_at_epoch_s = COALESCE(sent_at_epoch_s, ?1),
                 delivered_at_epoch_s = COALESCE(delivered_at_epoch_s, ?1),
                 notification_state = 'delivered',
                 state_changed_at_epoch_s = ?1
             WHERE email_id = ?2",
            params![now as i64, email_id],
        )?;
        Ok(())
    }
}

#[derive(Debug)]
struct KeyRow {
    key_id: String,
    license_id: String,
    owner_email: String,
}

#[derive(Debug)]
struct InstallationRow {
    installation_id: String,
    trusted: bool,
    session_secret_hash: Option<String>,
}

#[derive(Debug)]
struct PendingRequestRow {
    request_id: String,
    expires_at_epoch_s: i64,
    approved_at_epoch_s: Option<i64>,
}

#[derive(Debug)]
struct ReplacementOutcome {
    license_id: String,
    key_id: String,
    key_value: String,
    installation_id: String,
    session_secret: String,
}

#[derive(Deserialize)]
struct IssueForm {
    email: String,
}

#[derive(Deserialize)]
struct AdminFeatureForm {
    license_id: String,
    features_csv: String,
}

#[derive(Deserialize)]
struct AdminKeyStatusForm {
    key_id: String,
    active: String,
}

async fn index() -> Html<String> {
    Html(render_index_page(None))
}

async fn healthz() -> &'static str {
    "ok"
}

async fn issue_form(
    State(state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Form(form): Form<IssueForm>,
) -> AppResult<Html<String>> {
    let response = state.db.issue_license(&IssueKeyRequest {
        email: form.email,
        requested_features: None,
        registrant_os: user_agent_string(&headers),
        registrant_ip: Some(addr.ip().to_string()),
    })?;
    Ok(Html(render_index_page(Some(&response))))
}

async fn issue_json(
    State(state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(mut request): Json<IssueKeyRequest>,
) -> AppResult<Json<IssueKeyResponse>> {
    if request.registrant_ip.is_none() {
        request.registrant_ip = Some(addr.ip().to_string());
    }
    if request.registrant_os.is_none() {
        request.registrant_os = user_agent_string(&headers);
    }
    Ok(Json(state.db.issue_license(&request)?))
}

async fn verify_json(
    State(state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Json(request): Json<VerifyKeyRequest>,
) -> AppResult<Json<VerifyKeyResponse>> {
    let remote_ip = Some(addr.ip().to_string());
    let response = state
        .db
        .verify_key(&state.config, &request, remote_ip.as_deref())?;
    Ok(Json(response))
}

async fn read_client_notifications_json(
    State(state): State<Arc<AppState>>,
    Json(request): Json<ClientNotificationReadRequest>,
) -> AppResult<Json<Vec<ClientNotification>>> {
    Ok(Json(state.db.read_client_notifications(&request)?))
}

async fn report_unsupported_playback_json(
    State(state): State<Arc<AppState>>,
    Json(request): Json<ClientUnsupportedPlaybackRequest>,
) -> AppResult<Json<AdminActionResponse>> {
    let id = state.db.record_unsupported_playback(&request)?;
    Ok(Json(AdminActionResponse {
        ok: true,
        message: format!("unsupported playback request recorded: {id}"),
    }))
}

async fn confirm_request(
    State(state): State<Arc<AppState>>,
    AxumPath(token): AxumPath<String>,
) -> AppResult<Html<String>> {
    match state.db.confirm_request(&token)? {
        Some(license_id) => Ok(Html(format!(
            "<html><body><h1>Confirmation Recorded</h1><p>License {}</p><p>The next client refresh will trust this installation.</p></body></html>",
            html_escape(&license_id)
        ))),
        None => Err(AppError::not_found("confirmation token not found")),
    }
}

async fn admin_dashboard(
    State(state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
) -> AppResult<Html<String>> {
    ensure_local_admin(&addr)?;
    let snapshot = state.db.admin_snapshot()?;
    Ok(Html(render_admin_page(&snapshot)))
}

async fn update_license_features_handler(
    State(state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Form(form): Form<AdminFeatureForm>,
) -> AppResult<Redirect> {
    ensure_local_admin(&addr)?;
    state
        .db
        .update_license_features(&form.license_id, &parse_feature_csv(&form.features_csv))?;
    Ok(Redirect::to("/admin"))
}

async fn delete_license_handler(
    State(state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    AxumPath(license_id): AxumPath<String>,
) -> AppResult<Redirect> {
    ensure_local_admin(&addr)?;
    state.db.delete_license(&license_id)?;
    Ok(Redirect::to("/admin"))
}

async fn update_key_status_handler(
    State(state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Form(form): Form<AdminKeyStatusForm>,
) -> AppResult<Redirect> {
    ensure_local_admin(&addr)?;
    let active = matches!(form.active.as_str(), "1" | "true" | "on" | "yes");
    state.db.set_key_active(&form.key_id, active)?;
    Ok(Redirect::to("/admin"))
}

async fn delete_key_handler(
    State(state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    AxumPath(key_id): AxumPath<String>,
) -> AppResult<Redirect> {
    ensure_local_admin(&addr)?;
    state.db.delete_key(&key_id)?;
    Ok(Redirect::to("/admin"))
}

async fn approve_request_handler(
    State(state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    AxumPath(request_id): AxumPath<String>,
) -> AppResult<Redirect> {
    ensure_local_admin(&addr)?;
    state.db.approve_request_by_id(&request_id)?;
    Ok(Redirect::to("/admin"))
}

async fn delete_request_handler(
    State(state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    AxumPath(request_id): AxumPath<String>,
) -> AppResult<Redirect> {
    ensure_local_admin(&addr)?;
    state.db.delete_request(&request_id)?;
    Ok(Redirect::to("/admin"))
}

async fn delete_installation_handler(
    State(state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    AxumPath(installation_id): AxumPath<String>,
) -> AppResult<Redirect> {
    ensure_local_admin(&addr)?;
    state.db.delete_installation(&installation_id)?;
    Ok(Redirect::to("/admin"))
}

async fn delete_audit_handler(
    State(state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    AxumPath(event_id): AxumPath<String>,
) -> AppResult<Redirect> {
    ensure_local_admin(&addr)?;
    state.db.delete_audit(&event_id)?;
    Ok(Redirect::to("/admin"))
}

async fn admin_snapshot_json(
    State(state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
) -> AppResult<Json<AdminSnapshot>> {
    ensure_local_admin(&addr)?;
    Ok(Json(state.db.admin_snapshot()?))
}

async fn create_notification_json(
    State(state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Json(request): Json<AdminCreateNotificationRequest>,
) -> AppResult<Json<AdminActionResponse>> {
    ensure_local_admin(&addr)?;
    let email_id = state
        .db
        .create_and_send_notification(&state.config, &request)?;
    Ok(Json(AdminActionResponse {
        ok: true,
        message: format!("notification queued and sent: {email_id}"),
    }))
}

async fn update_license_features_json(
    State(state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Json(request): Json<AdminUpdateLicenseFeaturesRequest>,
) -> AppResult<Json<AdminActionResponse>> {
    ensure_local_admin(&addr)?;
    state
        .db
        .update_license_features(&request.license_id, &request.features)?;
    Ok(Json(AdminActionResponse {
        ok: true,
        message: "license features updated".to_string(),
    }))
}

async fn delete_license_json(
    State(state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    AxumPath(license_id): AxumPath<String>,
) -> AppResult<Json<AdminActionResponse>> {
    ensure_local_admin(&addr)?;
    state.db.delete_license(&license_id)?;
    Ok(Json(AdminActionResponse {
        ok: true,
        message: "license deleted".to_string(),
    }))
}

async fn update_license_state_json(
    State(state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    AxumPath(license_id): AxumPath<String>,
    Json(request): Json<AdminUpdateRecordStateRequest>,
) -> AppResult<Json<AdminActionResponse>> {
    ensure_local_admin(&addr)?;
    state
        .db
        .set_license_record_state(&license_id, request.state)?;
    Ok(Json(AdminActionResponse {
        ok: true,
        message: format!("license state updated to {}", request.state.as_str()),
    }))
}

async fn update_key_status_json(
    State(state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Json(request): Json<AdminUpdateKeyStatusRequest>,
) -> AppResult<Json<AdminActionResponse>> {
    ensure_local_admin(&addr)?;
    state.db.set_key_active(&request.key_id, request.active)?;
    Ok(Json(AdminActionResponse {
        ok: true,
        message: "key status updated".to_string(),
    }))
}

async fn update_key_state_json(
    State(state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    AxumPath(key_id): AxumPath<String>,
    Json(request): Json<AdminUpdateRecordStateRequest>,
) -> AppResult<Json<AdminActionResponse>> {
    ensure_local_admin(&addr)?;
    state.db.set_key_record_state(&key_id, request.state)?;
    Ok(Json(AdminActionResponse {
        ok: true,
        message: format!("key state updated to {}", request.state.as_str()),
    }))
}

async fn delete_key_json(
    State(state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    AxumPath(key_id): AxumPath<String>,
) -> AppResult<Json<AdminActionResponse>> {
    ensure_local_admin(&addr)?;
    state.db.delete_key(&key_id)?;
    Ok(Json(AdminActionResponse {
        ok: true,
        message: "key deleted".to_string(),
    }))
}

async fn approve_request_json(
    State(state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    AxumPath(request_id): AxumPath<String>,
) -> AppResult<Json<AdminActionResponse>> {
    ensure_local_admin(&addr)?;
    state.db.approve_request_by_id(&request_id)?;
    Ok(Json(AdminActionResponse {
        ok: true,
        message: "verification request approved".to_string(),
    }))
}

async fn update_request_state_json(
    State(state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    AxumPath(request_id): AxumPath<String>,
    Json(request): Json<AdminUpdateRecordStateRequest>,
) -> AppResult<Json<AdminActionResponse>> {
    ensure_local_admin(&addr)?;
    state.db.set_request_record_state(&request_id, request.state)?;
    Ok(Json(AdminActionResponse {
        ok: true,
        message: format!(
            "verification request state updated to {}",
            request.state.as_str()
        ),
    }))
}

async fn delete_request_json(
    State(state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    AxumPath(request_id): AxumPath<String>,
) -> AppResult<Json<AdminActionResponse>> {
    ensure_local_admin(&addr)?;
    state.db.delete_request(&request_id)?;
    Ok(Json(AdminActionResponse {
        ok: true,
        message: "verification request deleted".to_string(),
    }))
}

async fn delete_installation_json(
    State(state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    AxumPath(installation_id): AxumPath<String>,
) -> AppResult<Json<AdminActionResponse>> {
    ensure_local_admin(&addr)?;
    state.db.delete_installation(&installation_id)?;
    Ok(Json(AdminActionResponse {
        ok: true,
        message: "installation deleted".to_string(),
    }))
}

async fn update_installation_state_json(
    State(state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    AxumPath(installation_id): AxumPath<String>,
    Json(request): Json<AdminUpdateRecordStateRequest>,
) -> AppResult<Json<AdminActionResponse>> {
    ensure_local_admin(&addr)?;
    state
        .db
        .set_installation_record_state(&installation_id, request.state)?;
    Ok(Json(AdminActionResponse {
        ok: true,
        message: format!("installation state updated to {}", request.state.as_str()),
    }))
}

async fn delete_audit_json(
    State(state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    AxumPath(event_id): AxumPath<String>,
) -> AppResult<Json<AdminActionResponse>> {
    ensure_local_admin(&addr)?;
    state.db.delete_audit(&event_id)?;
    Ok(Json(AdminActionResponse {
        ok: true,
        message: "audit event deleted".to_string(),
    }))
}

async fn update_audit_state_json(
    State(state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    AxumPath(event_id): AxumPath<String>,
    Json(request): Json<AdminUpdateRecordStateRequest>,
) -> AppResult<Json<AdminActionResponse>> {
    ensure_local_admin(&addr)?;
    state.db.set_audit_record_state(&event_id, request.state)?;
    Ok(Json(AdminActionResponse {
        ok: true,
        message: format!("audit state updated to {}", request.state.as_str()),
    }))
}

type AppResult<T> = Result<T, AppError>;

struct AppError {
    status: StatusCode,
    message: String,
}

impl AppError {
    fn not_found(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            message: message.into(),
        }
    }
}

impl<E> From<E> for AppError
where
    E: Into<anyhow::Error>,
{
    fn from(value: E) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: value.into().to_string(),
        }
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> axum::response::Response {
        (self.status, self.message).into_response()
    }
}

fn scalar_count(conn: &Connection, sql: &str) -> Result<u64> {
    Ok(conn.query_row(sql, [], |row| row.get::<_, i64>(0))? as u64)
}

fn scalar_count_with_param(
    tx: &Transaction<'_>,
    sql: &str,
    param: &str,
) -> Result<u64> {
    Ok(tx.query_row(sql, params![param], |row| row.get::<_, i64>(0))? as u64)
}

fn migrate_record_state_columns(conn: &Connection) -> Result<()> {
    ensure_column(conn, "licenses", "record_state", "TEXT NOT NULL DEFAULT 'active'")?;
    ensure_column(conn, "licenses", "state_changed_at_epoch_s", "INTEGER")?;
    ensure_column(conn, "license_keys", "record_state", "TEXT NOT NULL DEFAULT 'active'")?;
    ensure_column(conn, "license_keys", "state_changed_at_epoch_s", "INTEGER")?;
    ensure_column(conn, "installations", "record_state", "TEXT NOT NULL DEFAULT 'active'")?;
    ensure_column(conn, "installations", "state_changed_at_epoch_s", "INTEGER")?;
    ensure_column(conn, "verification_requests", "record_state", "TEXT NOT NULL DEFAULT 'active'")?;
    ensure_column(conn, "verification_requests", "state_changed_at_epoch_s", "INTEGER")?;
    ensure_column(conn, "audit_events", "record_state", "TEXT NOT NULL DEFAULT 'active'")?;
    ensure_column(conn, "audit_events", "state_changed_at_epoch_s", "INTEGER")?;
    ensure_column(conn, "email_outbox", "request_id", "TEXT")?;
    ensure_column(conn, "email_outbox", "delivered_at_epoch_s", "INTEGER")?;
    ensure_column(conn, "email_outbox", "read_at_epoch_s", "INTEGER")?;
    ensure_column(
        conn,
        "email_outbox",
        "notification_state",
        "TEXT NOT NULL DEFAULT 'queued'",
    )?;
    ensure_column(conn, "email_outbox", "record_state", "TEXT NOT NULL DEFAULT 'active'")?;
    ensure_column(conn, "email_outbox", "state_changed_at_epoch_s", "INTEGER")?;
    conn.execute(
        "UPDATE email_outbox
         SET notification_state = 'delivered',
             delivered_at_epoch_s = COALESCE(delivered_at_epoch_s, sent_at_epoch_s),
             record_state = COALESCE(record_state, 'active')
         WHERE sent_at_epoch_s IS NOT NULL
           AND (notification_state IS NULL OR notification_state = 'queued')",
        [],
    )?;
    Ok(())
}

fn ensure_column(conn: &Connection, table: &str, column: &str, definition: &str) -> Result<()> {
    let pragma = format!("PRAGMA table_info({table})");
    let mut stmt = conn.prepare(&pragma)?;
    let existing = stmt
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    if !existing.iter().any(|existing| existing == column) {
        let alter = format!("ALTER TABLE {table} ADD COLUMN {column} {definition}");
        conn.execute(&alter, [])?;
    }
    Ok(())
}

trait FeatureQuerySource {
    fn prepare_features_query<'a>(&'a self, sql: &str) -> rusqlite::Result<rusqlite::Statement<'a>>;
}

impl FeatureQuerySource for Transaction<'_> {
    fn prepare_features_query<'a>(&'a self, sql: &str) -> rusqlite::Result<rusqlite::Statement<'a>> {
        self.prepare(sql)
    }
}

impl FeatureQuerySource for Connection {
    fn prepare_features_query<'a>(&'a self, sql: &str) -> rusqlite::Result<rusqlite::Statement<'a>> {
        self.prepare(sql)
    }
}

fn find_active_key(tx: &Transaction<'_>, key_value: &str) -> Result<Option<KeyRow>> {
    Ok(tx
        .query_row(
            "SELECT lk.key_id, lk.license_id, l.owner_email
             FROM license_keys lk
             JOIN licenses l ON l.license_id = lk.license_id
             WHERE lk.key_value = ?1
               AND lk.active = 1
               AND lk.record_state = 'active'
               AND l.record_state = 'active'",
            params![key_value],
            |row| {
                Ok(KeyRow {
                    key_id: row.get(0)?,
                    license_id: row.get(1)?,
                    owner_email: row.get(2)?,
                })
            },
        )
        .optional()?)
}

fn load_features_from_stmt_source(
    conn: &impl FeatureQuerySource,
    license_id: &str,
) -> Result<Vec<LicensedFeature>> {
    let mut stmt = conn.prepare_features_query(
        "SELECT feature_name FROM license_features WHERE license_id = ?1 ORDER BY feature_name ASC",
    )?;
    let rows = stmt.query_map(params![license_id], |row| row.get::<_, String>(0))?;
    let mut features = Vec::new();
    for feature_name in rows {
        let feature_name = feature_name?;
        if let Some(feature) = LicensedFeature::from_slug(&feature_name) {
            features.push(feature);
        }
    }
    if features.is_empty() {
        Ok(LicensedFeature::basic_defaults())
    } else {
        Ok(features)
    }
}

fn load_features(tx: &Transaction<'_>, license_id: &str) -> Result<Vec<LicensedFeature>> {
    load_features_from_stmt_source(tx, license_id)
}

fn load_features_conn(conn: &Connection, license_id: &str) -> Result<Vec<LicensedFeature>> {
    load_features_from_stmt_source(conn, license_id)
}

fn count_trusted_installations(tx: &Transaction<'_>, license_id: &str) -> Result<u64> {
    Ok(tx.query_row(
        "SELECT COUNT(*) FROM installations WHERE license_id = ?1 AND trusted = 1 AND record_state = 'active'",
        params![license_id],
        |row| row.get::<_, i64>(0),
    )? as u64)
}

fn find_installation(
    tx: &Transaction<'_>,
    license_id: &str,
    device_install_id: &str,
) -> Result<Option<InstallationRow>> {
    Ok(tx
        .query_row(
            "SELECT installation_id, trusted, session_secret_hash
             FROM installations
             WHERE license_id = ?1 AND device_install_id = ?2 AND record_state = 'active'",
            params![license_id, device_install_id],
            |row| {
                Ok(InstallationRow {
                    installation_id: row.get(0)?,
                    trusted: row.get::<_, i64>(1)? == 1,
                    session_secret_hash: row.get(2)?,
                })
            },
        )
        .optional()?)
}

fn find_pending_request(
    tx: &Transaction<'_>,
    license_id: &str,
    device_install_id: &str,
) -> Result<Option<PendingRequestRow>> {
    Ok(tx
        .query_row(
            "SELECT request_id, expires_at_epoch_s, approved_at_epoch_s
             FROM verification_requests
             WHERE license_id = ?1 AND device_install_id = ?2 AND record_state = 'active'
             ORDER BY created_at_epoch_s DESC
             LIMIT 1",
            params![license_id, device_install_id],
            |row| {
                Ok(PendingRequestRow {
                    request_id: row.get(0)?,
                    expires_at_epoch_s: row.get(1)?,
                    approved_at_epoch_s: row.get(2)?,
                })
            },
        )
        .optional()?)
}

fn upsert_trusted_installation(
    tx: &Transaction<'_>,
    license_id: &str,
    request: &VerifyKeyRequest,
    ip: Option<&str>,
    now: u64,
) -> Result<String> {
    let installation_id = find_installation(tx, license_id, &request.device.install_id)?
        .map(|record| record.installation_id)
        .unwrap_or_else(|| new_id("inst"));
    tx.execute(
        "INSERT INTO installations
         (installation_id, license_id, device_install_id, first_seen_ip, last_seen_ip, os_family, os_arch, hostname, first_seen_epoch_s, last_seen_epoch_s, trusted, session_secret_hash, record_state, state_changed_at_epoch_s)
         VALUES (?1, ?2, ?3, ?4, ?4, ?5, ?6, ?7, ?8, ?8, 1, NULL, 'active', ?8)
         ON CONFLICT(license_id, device_install_id) DO UPDATE SET
           last_seen_ip = excluded.last_seen_ip,
           os_family = excluded.os_family,
           os_arch = excluded.os_arch,
           hostname = excluded.hostname,
           last_seen_epoch_s = excluded.last_seen_epoch_s,
           trusted = 1,
           record_state = 'active',
           state_changed_at_epoch_s = excluded.state_changed_at_epoch_s",
        params![
            &installation_id,
            license_id,
            &request.device.install_id,
            ip,
            &request.os.family,
            &request.os.arch,
            &request.device.hostname,
            now as i64,
        ],
    )?;
    Ok(installation_id)
}

fn update_installation_last_seen(
    tx: &Transaction<'_>,
    installation_id: &str,
    ip: Option<&str>,
    request: &VerifyKeyRequest,
    now: u64,
) -> Result<()> {
    tx.execute(
        "UPDATE installations
         SET last_seen_ip = ?1, os_family = ?2, os_arch = ?3, hostname = ?4, last_seen_epoch_s = ?5
         WHERE installation_id = ?6",
        params![
            ip,
            &request.os.family,
            &request.os.arch,
            &request.device.hostname,
            now as i64,
            installation_id
        ],
    )?;
    Ok(())
}

fn rotate_session_secret(
    tx: &Transaction<'_>,
    installation_id: &str,
    _existing_secret: Option<&str>,
) -> Result<String> {
    let secret = Uuid::new_v4().to_string();
    tx.execute(
        "UPDATE installations SET session_secret_hash = ?1 WHERE installation_id = ?2",
        params![hash_secret(&secret), installation_id],
    )?;
    Ok(secret)
}

fn issue_replacement_license(
    tx: &Transaction<'_>,
    owner_email: &str,
    rotated_from_key_id: &str,
    request: &VerifyKeyRequest,
    ip: Option<&str>,
    now: u64,
) -> Result<ReplacementOutcome> {
    let license_id = new_id("lic");
    let key_id = new_id("key");
    let key_value = generate_key();
    let installation_id = new_id("inst");
    let session_secret = Uuid::new_v4().to_string();

    tx.execute(
        "INSERT INTO licenses (license_id, owner_email, registration_ip, registration_os, created_at_epoch_s)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            &license_id,
            owner_email,
            ip,
            format!("{}/{}", request.os.family, request.os.arch),
            now as i64
        ],
    )?;
    tx.execute(
        "INSERT INTO license_keys (key_id, license_id, key_value, key_version, active, created_at_epoch_s, rotated_from_key_id)
         VALUES (?1, ?2, ?3, 1, 1, ?4, ?5)",
        params![&key_id, &license_id, &key_value, now as i64, rotated_from_key_id],
    )?;
    tx.execute(
        "INSERT INTO license_features (license_id, feature_name) VALUES (?1, ?2)",
        params![&license_id, feature_name(LicensedFeature::Basic)],
    )?;
    tx.execute(
        "INSERT INTO installations
         (installation_id, license_id, device_install_id, first_seen_ip, last_seen_ip, os_family, os_arch, hostname, first_seen_epoch_s, last_seen_epoch_s, trusted, session_secret_hash, record_state, state_changed_at_epoch_s)
         VALUES (?1, ?2, ?3, ?4, ?4, ?5, ?6, ?7, ?8, ?8, 1, ?9, 'active', ?8)",
        params![
            &installation_id,
            &license_id,
            &request.device.install_id,
            ip,
            &request.os.family,
            &request.os.arch,
            &request.device.hostname,
            now as i64,
            hash_secret(&session_secret)
        ],
    )?;
    Ok(ReplacementOutcome {
        license_id,
        key_id,
        key_value,
        installation_id,
        session_secret,
    })
}

fn enqueue_email(
    tx: &Transaction<'_>,
    license_id: &str,
    request_id: Option<&str>,
    recipient: &str,
    subject: &str,
    body: &str,
    now: u64,
) -> Result<String> {
    let email_id = new_id("mail");
    tx.execute(
        "INSERT INTO email_outbox
         (email_id, license_id, request_id, recipient, subject, body, sent_at_epoch_s, delivered_at_epoch_s, read_at_epoch_s, notification_state, record_state, state_changed_at_epoch_s, created_at_epoch_s)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL, NULL, NULL, 'queued', 'active', ?7, ?7)",
        params![&email_id, license_id, request_id, recipient, subject, body, now as i64],
    )?;
    Ok(email_id)
}

fn insert_audit_event(
    tx: &Transaction<'_>,
    license_id: &str,
    key_id: Option<&str>,
    installation_id: Option<&str>,
    event_type: &str,
    payload: serde_json::Value,
) -> Result<()> {
    tx.execute(
        "INSERT INTO audit_events (event_id, license_id, key_id, installation_id, event_type, event_payload_json, created_at_epoch_s)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            new_id("audit"),
            license_id,
            key_id,
            installation_id,
            event_type,
            payload.to_string(),
            now_epoch_s() as i64
        ],
    )?;
    Ok(())
}

fn signed_response(
    config: &ServerConfig,
    license_id: &str,
    key_id: &str,
    device_install_id: &str,
    features: Vec<LicensedFeature>,
    status: EntitlementStatus,
    message: String,
    replacement_key: Option<String>,
    new_session_secret: Option<String>,
) -> Result<VerifyKeyResponse> {
    let now = now_epoch_s();
    let claims = EntitlementClaims {
        license_id: license_id.to_string(),
        key_id: key_id.to_string(),
        features,
        status,
        issued_at_epoch_s: now,
        expires_at_epoch_s: now + (config.token_ttl_hours * 3600),
        device_install_id: device_install_id.to_string(),
        message: message.clone(),
        replacement_key: replacement_key.clone(),
    };
    let signing_key = decode_signing_key(config.signing_key_seed())?;
    let envelope = SignedEntitlementEnvelope::sign(&claims, &signing_key)?;
    Ok(VerifyKeyResponse {
        envelope,
        replacement_key,
        new_session_secret,
        message,
    })
}

fn feature_name(feature: LicensedFeature) -> &'static str {
    match feature {
        LicensedFeature::Basic => "basic",
        LicensedFeature::EditorWorkspace => "editor_workspace",
        LicensedFeature::Encode => "encode",
        LicensedFeature::Decode => "decode",
        LicensedFeature::Compress => "compress",
        LicensedFeature::Import => "import",
        LicensedFeature::Transcode => "transcode",
        LicensedFeature::Mux => "mux",
        LicensedFeature::Demux => "demux",
        LicensedFeature::Select => "select",
        LicensedFeature::FrameEdit => "frame_edit",
        LicensedFeature::TimelineEdit => "timeline_edit",
        LicensedFeature::Export => "export",
    }
}

fn parse_record_state(state: &str) -> AdminRecordState {
    AdminRecordState::from_slug(state).unwrap_or(AdminRecordState::Active)
}

fn parse_notification_state(state: &str) -> NotificationDeliveryState {
    NotificationDeliveryState::from_slug(state).unwrap_or(NotificationDeliveryState::Queued)
}

fn parse_feature_name(feature: &str) -> Option<LicensedFeature> {
    match feature {
        "basic" => Some(LicensedFeature::Basic),
        "editor_workspace" => Some(LicensedFeature::EditorWorkspace),
        "encode" => Some(LicensedFeature::Encode),
        "decode" => Some(LicensedFeature::Decode),
        "compress" => Some(LicensedFeature::Compress),
        "import" => Some(LicensedFeature::Import),
        "transcode" => Some(LicensedFeature::Transcode),
        "mux" => Some(LicensedFeature::Mux),
        "demux" => Some(LicensedFeature::Demux),
        "select" => Some(LicensedFeature::Select),
        "frame_edit" => Some(LicensedFeature::FrameEdit),
        "timeline_edit" => Some(LicensedFeature::TimelineEdit),
        "export" => Some(LicensedFeature::Export),
        _ => None,
    }
}

fn deliver_email(recipient: &str, subject: &str, body: &str, config: &ServerConfig) -> bool {
    if let (Some(mail_from), Some(smtp_server)) = (&config.mail_from, &config.smtp_server) {
        let email = match Message::builder()
            .from(match mail_from.parse() {
                Ok(value) => value,
                Err(err) => {
                    info!(
                        target: "srs_license_server::mailer",
                        "invalid mail_from {}: {}",
                        mail_from,
                        err
                    );
                    return false;
                }
            })
            .to(match recipient.parse() {
                Ok(value) => value,
                Err(err) => {
                    info!(
                        target: "srs_license_server::mailer",
                        "invalid recipient {}: {}",
                        recipient,
                        err
                    );
                    return false;
                }
            })
            .subject(subject)
            .body(body.to_string())
        {
            Ok(email) => email,
            Err(err) => {
                info!(target: "srs_license_server::mailer", "message build failed: {}", err);
                return false;
            }
        };

        let mut builder = SmtpTransport::builder_dangerous(smtp_server);
        if let (Some(username), Some(password)) = (&config.smtp_username, &config.smtp_password) {
            builder = builder.credentials(Credentials::new(
                username.clone(),
                password.clone(),
            ));
        }
        let mailer = builder.build();
        return match mailer.send(&email) {
            Ok(_) => {
                info!(
                target: "srs_license_server::mailer",
                "smtp delivered to {} with subject {}",
                recipient,
                subject
            );
                true
            }
            Err(err) => {
                info!(
                target: "srs_license_server::mailer",
                "smtp delivery failed for {}: {}",
                recipient,
                err
            );
                false
            }
        };
    }

    info!(
        target: "srs_license_server::mailer",
        mode = "log",
        recipient,
        subject,
        "{}",
        body
    );
    true
}

fn render_index_page(response: Option<&IssueKeyResponse>) -> String {
    let issued = response.map(|response| {
        format!(
            "<section><h2>Issued Key</h2><p>License: <code>{}</code></p><p>Key: <code>{}</code></p><p>Features: {}</p></section>",
            html_escape(&response.license_id),
            html_escape(&response.key),
            response
                .features
                .iter()
                .map(|feature| feature.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )
    }).unwrap_or_default();
    format!(
        "<html><body><h1>SRS Licensing</h1><p>Free keys default to the <code>basic</code> feature set. Future editor upgrades can be assigned per key.</p><form method=\"post\" action=\"/issue\"><label>Email <input type=\"email\" name=\"email\" required></label><button type=\"submit\">Issue Key</button></form>{issued}<p>Health: <a href=\"/healthz\">/healthz</a></p></body></html>"
    )
}

fn render_admin_page(snapshot: &AdminSnapshot) -> String {
    let stats = format!(
        "<ul>\
            <li>Licenses: {}</li>\
            <li>Keys: {} (active: {})</li>\
            <li>Installations: {} (trusted: {})</li>\
            <li>Pending requests: {}</li>\
            <li>Audit events: {}</li>\
        </ul>",
        snapshot.stats.license_count,
        snapshot.stats.key_count,
        snapshot.stats.active_key_count,
        snapshot.stats.installation_count,
        snapshot.stats.trusted_installation_count,
        snapshot.stats.pending_request_count,
        snapshot.stats.audit_event_count
    );

    let licenses = snapshot
        .licenses
        .iter()
        .map(|license| {
            format!(
                "<tr>\
                    <td><code>{}</code></td>\
                    <td>{}</td>\
                    <td>{}</td>\
                    <td>\
                        <form method=\"post\" action=\"/admin/licenses/features\">\
                            <input type=\"hidden\" name=\"license_id\" value=\"{}\">\
                            <input type=\"text\" name=\"features_csv\" value=\"{}\" size=\"40\">\
                            <button type=\"submit\">Update Features</button>\
                        </form>\
                    </td>\
                </tr>",
                html_escape(&license.license_id),
                html_escape(&license.owner_email),
                license.active_key_count,
                html_escape(&license.license_id),
                html_escape(
                    &license
                        .features
                        .iter()
                        .map(|feature| feature.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            )
        })
        .collect::<Vec<_>>()
        .join("");

    let keys = snapshot
        .keys
        .iter()
        .map(|key| {
            let next_active = if key.active { "0" } else { "1" };
            let button_label = if key.active { "Deactivate" } else { "Activate" };
            format!(
                "<tr>\
                    <td><code>{}</code></td>\
                    <td><code>{}</code></td>\
                    <td><code>{}</code></td>\
                    <td>{}</td>\
                    <td>{}</td>\
                    <td>{}</td>\
                    <td>\
                        <form method=\"post\" action=\"/admin/keys/status\">\
                            <input type=\"hidden\" name=\"key_id\" value=\"{}\">\
                            <input type=\"hidden\" name=\"active\" value=\"{}\">\
                            <button type=\"submit\">{}</button>\
                        </form>\
                    </td>\
                </tr>",
                html_escape(&key.key_id),
                html_escape(&key.license_id),
                html_escape(&key.key_value),
                key.key_version,
                if key.active { "active" } else { "inactive" },
                format_epoch(key.created_at_epoch_s),
                html_escape(&key.key_id),
                next_active,
                button_label
            )
        })
        .collect::<Vec<_>>()
        .join("");

    let installations = snapshot
        .installations
        .iter()
        .map(|installation| {
            format!(
                "<tr>\
                    <td><code>{}</code></td>\
                    <td><code>{}</code></td>\
                    <td><code>{}</code></td>\
                    <td>{}</td>\
                    <td>{}</td>\
                    <td>{}</td>\
                    <td>{}</td>\
                    <td>{}</td>\
                    <td>{}</td>\
                </tr>",
                html_escape(&installation.installation_id),
                html_escape(&installation.license_id),
                html_escape(&installation.device_install_id),
                html_escape(installation.last_seen_ip.as_deref().unwrap_or("unknown")),
                html_escape(installation.first_seen_ip.as_deref().unwrap_or("unknown")),
                html_escape(&format!("{}/{}", installation.os_family, installation.os_arch)),
                html_escape(installation.hostname.as_deref().unwrap_or("unknown")),
                if installation.trusted { "verified" } else { "untrusted" },
                html_escape(&format!(
                    "first={} last={}",
                    format_epoch(installation.first_seen_epoch_s),
                    format_epoch(installation.last_seen_epoch_s)
                ))
            )
        })
        .collect::<Vec<_>>()
        .join("");

    let pending_requests = snapshot
        .pending_requests
        .iter()
        .map(|request| {
            let action = if request.approved_at_epoch_s.is_none() {
                format!(
                    "<form method=\"post\" action=\"/admin/requests/{}/approve\">\
                        <button type=\"submit\">Approve</button>\
                    </form>",
                    html_escape(&request.request_id)
                )
            } else {
                "approved".to_string()
            };
            format!(
                "<tr>\
                    <td><code>{}</code></td>\
                    <td><code>{}</code></td>\
                    <td>{}</td>\
                    <td>{}</td>\
                    <td>{}</td>\
                    <td>{}</td>\
                    <td>{}</td>\
                    <td>{}</td>\
                </tr>",
                html_escape(&request.request_id),
                html_escape(&request.license_id),
                html_escape(&request.device_install_id),
                html_escape(request.requested_ip.as_deref().unwrap_or("unknown")),
                html_escape(&format!("{}/{}", request.requested_os, request.requested_arch)),
                html_escape(request.hostname.as_deref().unwrap_or("unknown")),
                html_escape(&pending_request_status(request)),
                action
            )
        })
        .collect::<Vec<_>>()
        .join("");

    let audits = snapshot
        .audits
        .iter()
        .map(|event| {
            format!(
                "<tr>\
                    <td><code>{}</code></td>\
                    <td>{}</td>\
                    <td><code>{}</code></td>\
                    <td>{}</td>\
                    <td><code>{}</code></td>\
                    <td><code>{}</code></td>\
                    <td><pre>{}</pre></td>\
                </tr>",
                html_escape(&event.event_id),
                format_epoch(event.created_at_epoch_s),
                html_escape(&event.license_id),
                html_escape(&event.event_type),
                html_escape(event.key_id.as_deref().unwrap_or("-")),
                html_escape(event.installation_id.as_deref().unwrap_or("-")),
                html_escape(&event.event_payload_json)
            )
        })
        .collect::<Vec<_>>()
        .join("");

    format!(
        "<html><head><title>SRS Admin</title><style>\
            body {{ font-family: sans-serif; margin: 24px; background: #111; color: #eee; }}\
            h1, h2 {{ color: #9bd; }}\
            a {{ color: #9cf; }}\
            table {{ width: 100%; border-collapse: collapse; margin-bottom: 24px; }}\
            th, td {{ border: 1px solid #444; padding: 8px; vertical-align: top; }}\
            input[type='text'] {{ width: 100%; box-sizing: border-box; }}\
            code, pre {{ white-space: pre-wrap; word-break: break-word; }}\
            .panel {{ background: #1b1b1b; border: 1px solid #333; padding: 16px; margin-bottom: 24px; }}\
        </style></head><body>\
            <h1>SRS License Server Admin</h1>\
            <p>Local-only admin dashboard. Use this GUI on the Gentoo server to inspect and edit licensing state.</p>\
            <div class='panel'><h2>Database Stats</h2>{}</div>\
            <div class='panel'><h2>Licenses And Feature Editing</h2>\
                <table><thead><tr><th>License</th><th>Owner Email</th><th>Active Keys</th><th>Edit Features</th></tr></thead><tbody>{}</tbody></table>\
            </div>\
            <div class='panel'><h2>Keys</h2>\
                <table><thead><tr><th>Key Id</th><th>License</th><th>Key</th><th>Version</th><th>Status</th><th>Created</th><th>Edit</th></tr></thead><tbody>{}</tbody></table>\
            </div>\
            <div class='panel'><h2>Connected Installations And Verification Status</h2>\
                <table><thead><tr><th>Installation</th><th>License</th><th>Device</th><th>Last IP</th><th>First IP</th><th>OS</th><th>Hostname</th><th>Status</th><th>Seen</th></tr></thead><tbody>{}</tbody></table>\
            </div>\
            <div class='panel'><h2>Pending Verification Requests</h2>\
                <table><thead><tr><th>Request</th><th>License</th><th>Device</th><th>Requested IP</th><th>Requested OS</th><th>Hostname</th><th>Status</th><th>Action</th></tr></thead><tbody>{}</tbody></table>\
            </div>\
            <div class='panel'><h2>Recent Audit And Connection Log</h2>\
                <table><thead><tr><th>Event Id</th><th>Time</th><th>License</th><th>Event</th><th>Key</th><th>Installation</th><th>Payload</th></tr></thead><tbody>{}</tbody></table>\
            </div>\
        </body></html>",
        stats, licenses, keys, installations, pending_requests, audits
    )
}

fn ensure_local_admin(addr: &SocketAddr) -> AppResult<()> {
    if addr.ip().is_loopback() {
        Ok(())
    } else {
        Err(AppError {
            status: StatusCode::FORBIDDEN,
            message: "admin dashboard is only available from localhost".to_string(),
        })
    }
}

fn format_epoch(value: u64) -> String {
    value.to_string()
}

fn pending_request_status(request: &AdminPendingRequestRecord) -> String {
    if request.approved_at_epoch_s.is_some() {
        format!("approved at {}", format_epoch(request.approved_at_epoch_s.unwrap_or_default()))
    } else if now_epoch_s() > request.expires_at_epoch_s {
        format!("expired at {}", format_epoch(request.expires_at_epoch_s))
    } else {
        format!(
            "pending until {} (created {})",
            format_epoch(request.expires_at_epoch_s),
            format_epoch(request.created_at_epoch_s)
        )
    }
}

fn parse_feature_csv(features_csv: &str) -> Vec<LicensedFeature> {
    let mut features = features_csv
        .split(',')
        .map(|feature| feature.trim().to_ascii_lowercase().replace('-', "_"))
        .filter_map(|feature| parse_feature_name(&feature))
        .collect::<Vec<_>>();
    if !features.contains(&LicensedFeature::Basic) {
        features.insert(0, LicensedFeature::Basic);
    }
    features.sort_by_key(|feature| feature_name(*feature));
    features.dedup();
    if features.is_empty() {
        LicensedFeature::basic_defaults()
    } else {
        features
    }
}

fn user_agent_string(headers: &HeaderMap) -> Option<String> {
    headers
        .get("user-agent")
        .and_then(|value| value.to_str().ok())
        .map(ToOwned::to_owned)
}

fn generate_key() -> String {
    let raw = Uuid::new_v4().simple().to_string().to_uppercase();
    format!(
        "SRS-{}-{}-{}-{}",
        &raw[0..4],
        &raw[4..8],
        &raw[8..12],
        &raw[12..16]
    )
}

fn new_id(prefix: &str) -> String {
    format!("{prefix}_{}", Uuid::new_v4().simple())
}

fn hash_secret(secret: &str) -> String {
    let digest = Sha256::digest(secret.as_bytes());
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn html_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

fn now_epoch_s() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn test_config(name: &str) -> ServerConfig {
        let mut config = ServerConfig::default();
        config.database_path = std::env::temp_dir()
            .join(format!("srs-license-server-{name}-{}.sqlite3", Uuid::new_v4()))
            .display()
            .to_string();
        config.base_url = "http://localhost:3000".to_string();
        config
    }

    #[test]
    fn issue_and_verify_initial_installation() {
        let config = test_config("initial");
        let db = Database::open(&config).expect("open db");
        let issued = db
            .issue_license(&IssueKeyRequest {
                email: "user@example.com".to_string(),
                requested_features: Some(vec![LicensedFeature::Basic, LicensedFeature::EditorWorkspace]),
                registrant_os: Some("Linux".to_string()),
                registrant_ip: Some("127.0.0.1".to_string()),
            })
            .expect("issue license");
        let response = db
            .verify_key(
                &config,
                &VerifyKeyRequest {
                    key: issued.key,
                    claimed_ip: Some("127.0.0.1".to_string()),
                    os: libsrs_licensing_proto::ClientOsInfo {
                        family: "linux".to_string(),
                        version: None,
                        arch: "x86_64".to_string(),
                    },
                    device: libsrs_licensing_proto::DeviceFingerprint {
                        install_id: "install-1".to_string(),
                        hostname: Some("host-a".to_string()),
                    },
                    app: libsrs_licensing_proto::ClientAppInfo {
                        name: "srs-player".to_string(),
                        version: "0.1.0".to_string(),
                        channel: None,
                    },
                    session_secret: None,
                },
                Some("127.0.0.1"),
            )
            .expect("verify");
        let signing_key = decode_signing_key(config.signing_key_seed()).expect("decode signing key");
        let claims = response
            .envelope
            .verify(&signing_key.verifying_key())
            .expect("verify envelope");
        assert_eq!(claims.status, EntitlementStatus::Active);
        assert!(claims.is_editor_enabled());
    }

    #[test]
    fn second_origin_becomes_pending_then_confirmable() {
        let config = test_config("pending");
        let db = Database::open(&config).expect("open db");
        let issued = db
            .issue_license(&IssueKeyRequest {
                email: "user@example.com".to_string(),
                requested_features: None,
                registrant_os: Some("Linux".to_string()),
                registrant_ip: Some("127.0.0.1".to_string()),
            })
            .expect("issue license");

        let first = VerifyKeyRequest {
            key: issued.key.clone(),
            claimed_ip: Some("10.0.0.2".to_string()),
            os: libsrs_licensing_proto::ClientOsInfo {
                family: "linux".to_string(),
                version: None,
                arch: "x86_64".to_string(),
            },
            device: libsrs_licensing_proto::DeviceFingerprint {
                install_id: "install-a".to_string(),
                hostname: Some("host-a".to_string()),
            },
            app: libsrs_licensing_proto::ClientAppInfo {
                name: "srs-player".to_string(),
                version: "0.1.0".to_string(),
                channel: None,
            },
            session_secret: None,
        };
        db.verify_key(&config, &first, Some("10.0.0.2"))
            .expect("initial verify");

        let second = VerifyKeyRequest {
            key: issued.key,
            claimed_ip: Some("203.0.113.10".to_string()),
            os: libsrs_licensing_proto::ClientOsInfo {
                family: "linux".to_string(),
                version: None,
                arch: "x86_64".to_string(),
            },
            device: libsrs_licensing_proto::DeviceFingerprint {
                install_id: "install-b".to_string(),
                hostname: Some("host-b".to_string()),
            },
            app: libsrs_licensing_proto::ClientAppInfo {
                name: "srs-player".to_string(),
                version: "0.1.0".to_string(),
                channel: None,
            },
            session_secret: None,
        };
        let pending = db
            .verify_key(&config, &second, Some("203.0.113.10"))
            .expect("pending verify");
        let signing_key = decode_signing_key(config.signing_key_seed()).expect("decode signing key");
        let pending_claims = pending
            .envelope
            .verify(&signing_key.verifying_key())
            .expect("verify envelope");
        assert_eq!(pending_claims.status, EntitlementStatus::PendingConfirmation);

        let token = {
            let conn = db.conn.lock().expect("lock db");
            conn.query_row(
                "SELECT token FROM verification_requests ORDER BY created_at_epoch_s DESC LIMIT 1",
                [],
                |row| row.get::<_, String>(0),
            )
            .expect("fetch token")
        };
        db.confirm_request(&token).expect("confirm");
        let approved = db
            .verify_key(&config, &second, Some("203.0.113.10"))
            .expect("approved verify");
        let approved_claims = approved
            .envelope
            .verify(&signing_key.verifying_key())
            .expect("verify envelope");
        assert_eq!(approved_claims.status, EntitlementStatus::Active);

        let _ = fs::remove_file(std::path::Path::new(&config.database_path));
    }

    #[test]
    fn expired_pending_request_issues_replacement_key() {
        let config = test_config("replacement");
        let db = Database::open(&config).expect("open db");
        let issued = db
            .issue_license(&IssueKeyRequest {
                email: "user@example.com".to_string(),
                requested_features: Some(vec![LicensedFeature::Basic, LicensedFeature::EditorWorkspace]),
                registrant_os: Some("Linux".to_string()),
                registrant_ip: Some("127.0.0.1".to_string()),
            })
            .expect("issue license");

        let trusted_request = VerifyKeyRequest {
            key: issued.key.clone(),
            claimed_ip: Some("10.0.0.2".to_string()),
            os: libsrs_licensing_proto::ClientOsInfo {
                family: "linux".to_string(),
                version: None,
                arch: "x86_64".to_string(),
            },
            device: libsrs_licensing_proto::DeviceFingerprint {
                install_id: "install-a".to_string(),
                hostname: Some("host-a".to_string()),
            },
            app: libsrs_licensing_proto::ClientAppInfo {
                name: "srs-player".to_string(),
                version: "0.1.0".to_string(),
                channel: None,
            },
            session_secret: None,
        };
        db.verify_key(&config, &trusted_request, Some("10.0.0.2"))
            .expect("trust initial install");

        let second_request = VerifyKeyRequest {
            key: issued.key,
            claimed_ip: Some("203.0.113.10".to_string()),
            os: libsrs_licensing_proto::ClientOsInfo {
                family: "linux".to_string(),
                version: None,
                arch: "x86_64".to_string(),
            },
            device: libsrs_licensing_proto::DeviceFingerprint {
                install_id: "install-b".to_string(),
                hostname: Some("host-b".to_string()),
            },
            app: libsrs_licensing_proto::ClientAppInfo {
                name: "srs-player".to_string(),
                version: "0.1.0".to_string(),
                channel: None,
            },
            session_secret: None,
        };
        db.verify_key(&config, &second_request, Some("203.0.113.10"))
            .expect("create pending request");

        {
            let conn = db.conn.lock().expect("lock db");
            conn.execute(
                "UPDATE verification_requests SET expires_at_epoch_s = 0 WHERE device_install_id = ?1",
                params!["install-b"],
            )
            .expect("expire pending request");
        }

        let replacement = db
            .verify_key(&config, &second_request, Some("203.0.113.10"))
            .expect("issue replacement");
        let signing_key = decode_signing_key(config.signing_key_seed()).expect("decode signing key");
        let claims = replacement
            .envelope
            .verify(&signing_key.verifying_key())
            .expect("verify replacement envelope");
        assert_eq!(claims.status, EntitlementStatus::ReplacementIssued);
        assert_eq!(claims.features, LicensedFeature::basic_defaults());
        assert!(replacement.replacement_key.is_some());

        let _ = fs::remove_file(std::path::Path::new(&config.database_path));
    }

    #[test]
    fn admin_can_soft_delete_license_and_related_rows() {
        let config = test_config("delete-license");
        let db = Database::open(&config).expect("open db");
        let issued = db
            .issue_license(&IssueKeyRequest {
                email: "user@example.com".to_string(),
                requested_features: Some(LicensedFeature::editor_defaults()),
                registrant_os: Some("Linux".to_string()),
                registrant_ip: Some("127.0.0.1".to_string()),
            })
            .expect("issue license");

        db.delete_license(&issued.license_id)
            .expect("delete license");

        let conn = db.conn.lock().expect("lock db");
        let license_state: String = conn
            .query_row(
                "SELECT record_state FROM licenses WHERE license_id = ?1",
                params![issued.license_id],
                |row| row.get(0),
            )
            .expect("fetch license state");
        let key_state: String = conn
            .query_row(
                "SELECT record_state FROM license_keys WHERE license_id = ?1 LIMIT 1",
                params![issued.license_id],
                |row| row.get(0),
            )
            .expect("fetch key state");
        assert_eq!(license_state, "deleted");
        assert_eq!(key_state, "deleted");

        let _ = fs::remove_file(std::path::Path::new(&config.database_path));
    }

    #[test]
    fn archived_license_is_not_verified_until_restored() {
        let config = test_config("archive-restore");
        let db = Database::open(&config).expect("open db");
        let issued = db
            .issue_license(&IssueKeyRequest {
                email: "user@example.com".to_string(),
                requested_features: Some(LicensedFeature::editor_defaults()),
                registrant_os: Some("Linux".to_string()),
                registrant_ip: Some("127.0.0.1".to_string()),
            })
            .expect("issue license");

        let request = VerifyKeyRequest {
            key: issued.key,
            claimed_ip: Some("127.0.0.1".to_string()),
            os: libsrs_licensing_proto::ClientOsInfo {
                family: "linux".to_string(),
                version: None,
                arch: "x86_64".to_string(),
            },
            device: libsrs_licensing_proto::DeviceFingerprint {
                install_id: "install-1".to_string(),
                hostname: Some("host-a".to_string()),
            },
            app: libsrs_licensing_proto::ClientAppInfo {
                name: "srs-player".to_string(),
                version: "0.1.0".to_string(),
                channel: None,
            },
            session_secret: None,
        };

        db.set_license_record_state(&issued.license_id, AdminRecordState::Archived)
            .expect("archive license");
        let verify_err = db
            .verify_key(&config, &request, Some("127.0.0.1"))
            .expect_err("archived license should not verify");
        assert!(verify_err.to_string().contains("license key not found or inactive"));

        db.set_license_record_state(&issued.license_id, AdminRecordState::Active)
            .expect("restore license");
        let verified = db
            .verify_key(&config, &request, Some("127.0.0.1"))
            .expect("restored license should verify");
        let signing_key = decode_signing_key(config.signing_key_seed()).expect("decode signing key");
        let claims = verified
            .envelope
            .verify(&signing_key.verifying_key())
            .expect("verify restored envelope");
        assert_eq!(claims.status, EntitlementStatus::Active);

        let _ = fs::remove_file(std::path::Path::new(&config.database_path));
    }

    #[test]
    fn notification_progresses_to_read_and_soft_deleted_on_confirmation() {
        let config = test_config("notifications");
        let db = Database::open(&config).expect("open db");
        let issued = db
            .issue_license(&IssueKeyRequest {
                email: "user@example.com".to_string(),
                requested_features: None,
                registrant_os: Some("Linux".to_string()),
                registrant_ip: Some("127.0.0.1".to_string()),
            })
            .expect("issue license");

        let first = VerifyKeyRequest {
            key: issued.key.clone(),
            claimed_ip: Some("10.0.0.2".to_string()),
            os: libsrs_licensing_proto::ClientOsInfo {
                family: "linux".to_string(),
                version: None,
                arch: "x86_64".to_string(),
            },
            device: libsrs_licensing_proto::DeviceFingerprint {
                install_id: "install-a".to_string(),
                hostname: Some("host-a".to_string()),
            },
            app: libsrs_licensing_proto::ClientAppInfo {
                name: "srs-player".to_string(),
                version: "0.1.0".to_string(),
                channel: None,
            },
            session_secret: None,
        };
        db.verify_key(&config, &first, Some("10.0.0.2"))
            .expect("initial verify");

        let second = VerifyKeyRequest {
            key: issued.key,
            claimed_ip: Some("203.0.113.10".to_string()),
            os: libsrs_licensing_proto::ClientOsInfo {
                family: "linux".to_string(),
                version: None,
                arch: "x86_64".to_string(),
            },
            device: libsrs_licensing_proto::DeviceFingerprint {
                install_id: "install-b".to_string(),
                hostname: Some("host-b".to_string()),
            },
            app: libsrs_licensing_proto::ClientAppInfo {
                name: "srs-player".to_string(),
                version: "0.1.0".to_string(),
                channel: None,
            },
            session_secret: None,
        };
        db.verify_key(&config, &second, Some("203.0.113.10"))
            .expect("create pending request");

        let (token, request_id, sent_at, delivered_at, state_before, record_state_before): (
            String,
            String,
            Option<i64>,
            Option<i64>,
            String,
            String,
        ) = {
            let conn = db.conn.lock().expect("lock db");
            let token = conn
                .query_row(
                    "SELECT token, request_id FROM verification_requests ORDER BY created_at_epoch_s DESC LIMIT 1",
                    [],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
                )
                .expect("fetch token");
            let notification = conn
                .query_row(
                    "SELECT sent_at_epoch_s, delivered_at_epoch_s, notification_state, record_state
                     FROM email_outbox
                     WHERE request_id = ?1
                     ORDER BY created_at_epoch_s DESC
                     LIMIT 1",
                    params![&token.1],
                    |row| {
                        Ok((
                            row.get::<_, Option<i64>>(0)?,
                            row.get::<_, Option<i64>>(1)?,
                            row.get::<_, String>(2)?,
                            row.get::<_, String>(3)?,
                        ))
                    },
                )
                .expect("fetch notification");
            (token.0, token.1, notification.0, notification.1, notification.2, notification.3)
        };

        assert!(sent_at.is_some());
        assert!(delivered_at.is_some());
        assert_eq!(state_before, "delivered");
        assert_eq!(record_state_before, "active");

        db.confirm_request(&token).expect("confirm request");

        let (read_at, state_after, record_state_after): (Option<i64>, String, String) = {
            let conn = db.conn.lock().expect("lock db");
            conn.query_row(
                "SELECT read_at_epoch_s, notification_state, record_state
                 FROM email_outbox
                 WHERE request_id = ?1
                 ORDER BY created_at_epoch_s DESC
                 LIMIT 1",
                params![&request_id],
                |row| {
                    Ok((
                        row.get::<_, Option<i64>>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                },
            )
            .expect("fetch confirmed notification")
        };

        assert!(read_at.is_some());
        assert_eq!(state_after, "read");
        assert_eq!(record_state_after, "deleted");

        let _ = fs::remove_file(std::path::Path::new(&config.database_path));
    }

    #[test]
    fn admin_can_create_and_send_notification() {
        let config = test_config("manual-notification");
        let db = Database::open(&config).expect("open db");
        let issued = db
            .issue_license(&IssueKeyRequest {
                email: "user@example.com".to_string(),
                requested_features: Some(LicensedFeature::basic_defaults()),
                registrant_os: Some("Linux".to_string()),
                registrant_ip: Some("127.0.0.1".to_string()),
            })
            .expect("issue license");

        let email_id = db
            .create_and_send_notification(
                &config,
                &AdminCreateNotificationRequest {
                    license_id: issued.license_id.clone(),
                    recipient: "user@example.com".to_string(),
                    subject: "Manual admin notice".to_string(),
                    body: "This is a manual notification.".to_string(),
                },
            )
            .expect("create notification");

        let (state, record_state, sent_at, delivered_at): (
            String,
            String,
            Option<i64>,
            Option<i64>,
        ) = {
            let conn = db.conn.lock().expect("lock db");
            conn.query_row(
                "SELECT notification_state, record_state, sent_at_epoch_s, delivered_at_epoch_s
                 FROM email_outbox
                 WHERE email_id = ?1",
                params![email_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Option<i64>>(2)?,
                        row.get::<_, Option<i64>>(3)?,
                    ))
                },
            )
            .expect("fetch notification")
        };

        assert_eq!(state, "delivered");
        assert_eq!(record_state, "active");
        assert!(sent_at.is_some());
        assert!(delivered_at.is_some());

        let _ = fs::remove_file(std::path::Path::new(&config.database_path));
    }

    #[test]
    fn client_reads_manual_notification_once_and_soft_deletes_it() {
        let config = test_config("client-read-notification");
        let db = Database::open(&config).expect("open db");
        let issued = db
            .issue_license(&IssueKeyRequest {
                email: "user@example.com".to_string(),
                requested_features: Some(LicensedFeature::editor_defaults()),
                registrant_os: Some("Linux".to_string()),
                registrant_ip: Some("127.0.0.1".to_string()),
            })
            .expect("issue license");

        let request = VerifyKeyRequest {
            key: issued.key.clone(),
            claimed_ip: Some("127.0.0.1".to_string()),
            os: libsrs_licensing_proto::ClientOsInfo {
                family: "linux".to_string(),
                version: None,
                arch: "x86_64".to_string(),
            },
            device: libsrs_licensing_proto::DeviceFingerprint {
                install_id: "install-1".to_string(),
                hostname: Some("host-a".to_string()),
            },
            app: libsrs_licensing_proto::ClientAppInfo {
                name: "srs-player".to_string(),
                version: "0.1.0".to_string(),
                channel: None,
            },
            session_secret: None,
        };
        db.verify_key(&config, &request, Some("127.0.0.1"))
            .expect("verify initial install");

        let email_id = db
            .create_and_send_notification(
                &config,
                &AdminCreateNotificationRequest {
                    license_id: issued.license_id.clone(),
                    recipient: "user@example.com".to_string(),
                    subject: "Manual admin notice".to_string(),
                    body: "This is a manual notification.".to_string(),
                },
            )
            .expect("create notification");

        let notifications = db
            .read_client_notifications(&ClientNotificationReadRequest {
                key: issued.key,
                license_id: issued.license_id,
                device_install_id: "install-1".to_string(),
            })
            .expect("read notifications");
        assert_eq!(notifications.len(), 1);
        assert_eq!(notifications[0].notification_id, email_id);

        let (state, record_state): (String, String) = {
            let conn = db.conn.lock().expect("lock db");
            conn.query_row(
                "SELECT notification_state, record_state FROM email_outbox WHERE email_id = ?1",
                params![email_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("fetch notification state")
        };
        assert_eq!(state, "read");
        assert_eq!(record_state, "deleted");

        let _ = fs::remove_file(std::path::Path::new(&config.database_path));
    }

    #[test]
    fn unsupported_playback_request_is_recorded_for_admin() {
        let config = test_config("unsupported-playback");
        let db = Database::open(&config).expect("open db");
        let issued = db
            .issue_license(&IssueKeyRequest {
                email: "user@example.com".to_string(),
                requested_features: Some(LicensedFeature::editor_defaults()),
                registrant_os: Some("Linux".to_string()),
                registrant_ip: Some("127.0.0.1".to_string()),
            })
            .expect("issue license");

        let id = db
            .record_unsupported_playback(&ClientUnsupportedPlaybackRequest {
                key: Some(issued.key),
                license_id: Some(issued.license_id.clone()),
                device_install_id: "install-1".to_string(),
                source: "sample-h264.mp4".to_string(),
                app_name: "srs-player".to_string(),
                app_version: "0.1.0".to_string(),
                tracks: vec![libsrs_licensing_proto::UnsupportedCodecTrack {
                    track_id: 0,
                    kind: "Video".to_string(),
                    codec: "H.264/AVC".to_string(),
                    detail: "blocked by policy".to_string(),
                }],
            })
            .expect("record unsupported playback");

        let snapshot = db.admin_snapshot().expect("admin snapshot");
        assert_eq!(snapshot.stats.playback_request_count, 1);
        assert_eq!(snapshot.playback_requests.len(), 1);
        assert_eq!(snapshot.playback_requests[0].playback_request_id, id);
        assert_eq!(
            snapshot.playback_requests[0].license_id.as_deref(),
            Some(issued.license_id.as_str())
        );

        let _ = fs::remove_file(std::path::Path::new(&config.database_path));
    }
}
