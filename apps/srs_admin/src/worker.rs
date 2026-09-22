//! Background HTTP worker for the native SRS admin UI.
//!
//! The egui thread never performs network I/O. This worker owns the administrator
//! bearer credential and applies it to every privileged request. The credential is
//! intentionally not returned in events, formatted into status text, or stored in
//! the UI model.

use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::thread;
use std::time::Duration;

use libsrs_licensing_proto::{
    AdminActionResponse, AdminCreateNotificationRequest, AdminSnapshot,
    AdminUpdateLicenseFeaturesRequest, AdminUpdateRecordStateRequest,
    AdminUpdateKeyStatusRequest, IssueKeyRequest, IssueKeyResponse,
};
use reqwest::blocking::{Client, RequestBuilder};
use reqwest::{StatusCode, Url};
use zeroize::Zeroizing;

// Intentionally no Debug derive: commands can contain administrator-entered
// notification subjects/bodies and other privacy-sensitive request data.
pub enum AdminCommand {
    RefreshSnapshot,
    IssueLicense(IssueKeyRequest),
    UpdateLicenseFeatures(AdminUpdateLicenseFeaturesRequest),
    UpdateKeyStatus(AdminUpdateKeyStatusRequest),
    SetRecordState {
        path: String,
        request: AdminUpdateRecordStateRequest,
    },
    ApproveRequest {
        request_id: String,
    },
    CreateNotification(AdminCreateNotificationRequest),
    Shutdown,
}

pub enum AdminEvent {
    Snapshot(Result<AdminSnapshot, AdminClientError>),
    Action(Result<AdminActionResponse, AdminClientError>),
    Issued(Result<IssueKeyResponse, AdminClientError>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdminClientError {
    AuthenticationRequired,
    Forbidden,
    RateLimited,
    ServerFailure,
    Offline,
    Timeout,
    SecurityFailure,
    ProtocolFailure,
}

impl AdminClientError {
    pub const fn user_message(self) -> &'static str {
        match self {
            Self::AuthenticationRequired => "Administrator authentication required or expired.",
            Self::Forbidden => "Administrator access is forbidden by server policy.",
            Self::RateLimited => "The server is rate limiting administrative requests.",
            Self::ServerFailure => "The licensing server reported an internal failure.",
            Self::Offline => "The licensing server is unreachable.",
            Self::Timeout => "The licensing server request timed out.",
            Self::SecurityFailure => "A secure connection to the licensing server could not be established.",
            Self::ProtocolFailure => "The licensing server returned an invalid or unexpected response.",
        }
    }

}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdminEndpointSecurity {
    LocalHttp,
    Https,
}

impl AdminEndpointSecurity {
    pub const fn label(self) -> &'static str {
        match self {
            Self::LocalHttp => "HTTP (local development)",
            Self::Https => "HTTPS",
        }
    }
}

pub fn validate_admin_endpoint(base_url: &str) -> Result<AdminEndpointSecurity, AdminClientError> {
    let url = Url::parse(base_url).map_err(|_| AdminClientError::SecurityFailure)?;
    match url.scheme() {
        "https" => Ok(AdminEndpointSecurity::Https),
        "http" if is_loopback_host(&url) => Ok(AdminEndpointSecurity::LocalHttp),
        _ => Err(AdminClientError::SecurityFailure),
    }
}

fn is_loopback_host(url: &Url) -> bool {
    let Some(host) = url.host_str() else {
        return false;
    };
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    host.parse::<std::net::IpAddr>()
        .map(|ip| ip.is_loopback())
        .unwrap_or(false)
}

pub struct AdminWorker {
    command_tx: SyncSender<AdminCommand>,
    event_rx: Receiver<AdminEvent>,
}

impl AdminWorker {
    pub fn spawn(
        base_url: String,
        bearer_token: String,
        connect_timeout: Duration,
        request_timeout: Duration,
    ) -> Result<Self, String> {
        validate_admin_endpoint(&base_url)
            .map_err(|error| error.user_message().to_string())?;

        let client = Client::builder()
            .connect_timeout(connect_timeout)
            .timeout(request_timeout)
            .build()
            .map_err(|_| "Unable to initialize the administrator HTTP client.".to_string())?;

        // Bounded queues prevent repeated UI actions from growing memory without limit.
        // The UI uses try_send so saturation never blocks the egui frame thread.
        let (command_tx, command_rx) = mpsc::sync_channel(32);
        let (event_tx, event_rx) = mpsc::sync_channel(32);

        let bearer_token = Zeroizing::new(bearer_token);
        thread::Builder::new()
            .name("srs-admin-http".to_string())
            .spawn(move || {
                run_worker(client, base_url, bearer_token, command_rx, event_tx);
            })
            .map_err(|_| "Unable to start the administrator HTTP worker.".to_string())?;

        Ok(Self {
            command_tx,
            event_rx,
        })
    }

    pub fn send(&self, command: AdminCommand) -> Result<(), String> {
        match self.command_tx.try_send(command) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) => {
                Err("Administrator request queue is busy; try again shortly.".to_string())
            }
            Err(TrySendError::Disconnected(_)) => {
                Err("Administrator HTTP worker is unavailable.".to_string())
            }
        }
    }

    pub fn try_recv(&self) -> Option<AdminEvent> {
        self.event_rx.try_recv().ok()
    }
}

impl Drop for AdminWorker {
    fn drop(&mut self) {
        // Never block the UI thread during window shutdown if the bounded queue is full.
        let _ = self.command_tx.try_send(AdminCommand::Shutdown);
    }
}

fn run_worker(
    client: Client,
    base_url: String,
    bearer_token: Zeroizing<String>,
    command_rx: Receiver<AdminCommand>,
    event_tx: SyncSender<AdminEvent>,
) {
    let base_url = base_url.trim_end_matches('/').to_string();

    while let Ok(command) = command_rx.recv() {
        let event = match command {
            AdminCommand::RefreshSnapshot => AdminEvent::Snapshot(
                send_json::<AdminSnapshot>(authorized(
                    client.get(format!("{base_url}/api/v1/admin/snapshot")),
                    bearer_token.as_str(),
                )),
            ),
            AdminCommand::IssueLicense(request) => AdminEvent::Issued(
                send_json::<IssueKeyResponse>(
                    authorized(client.post(format!("{base_url}/api/v1/issue")), bearer_token.as_str())
                        .json(&request),
                ),
            ),
            AdminCommand::UpdateLicenseFeatures(request) => AdminEvent::Action(
                send_json::<AdminActionResponse>(
                    authorized(
                        client.post(format!("{base_url}/api/v1/admin/licenses/features")),
                        bearer_token.as_str(),
                    )
                    .json(&request),
                ),
            ),
            AdminCommand::UpdateKeyStatus(request) => AdminEvent::Action(
                send_json::<AdminActionResponse>(
                    authorized(
                        client.post(format!("{base_url}/api/v1/admin/keys/status")),
                        bearer_token.as_str(),
                    )
                    .json(&request),
                ),
            ),
            AdminCommand::SetRecordState { path, request } => AdminEvent::Action(
                send_json::<AdminActionResponse>(
                    authorized(client.post(format!("{base_url}/{path}")), bearer_token.as_str())
                        .json(&request),
                ),
            ),
            AdminCommand::ApproveRequest { request_id } => AdminEvent::Action(
                send_json::<AdminActionResponse>(authorized(
                    client.post(format!(
                        "{base_url}/api/v1/admin/requests/{request_id}/approve"
                    )),
                    bearer_token.as_str(),
                )),
            ),
            AdminCommand::CreateNotification(request) => AdminEvent::Action(
                send_json::<AdminActionResponse>(
                    authorized(
                        client.post(format!("{base_url}/api/v1/admin/notifications/create")),
                        bearer_token.as_str(),
                    )
                    .json(&request),
                ),
            ),
            AdminCommand::Shutdown => break,
        };

        if event_tx.send(event).is_err() {
            break;
        }
    }
}

fn authorized(request: RequestBuilder, bearer_token: &str) -> RequestBuilder {
    request.bearer_auth(bearer_token)
}

fn send_json<T: serde::de::DeserializeOwned>(
    request: RequestBuilder,
) -> Result<T, AdminClientError> {
    let response = request.send().map_err(classify_transport)?;
    let status = response.status();
    if !status.is_success() {
        return Err(classify_status(status));
    }
    response
        .json::<T>()
        .map_err(|_| AdminClientError::ProtocolFailure)
}

fn classify_status(status: StatusCode) -> AdminClientError {
    match status {
        StatusCode::UNAUTHORIZED => AdminClientError::AuthenticationRequired,
        StatusCode::FORBIDDEN => AdminClientError::Forbidden,
        StatusCode::TOO_MANY_REQUESTS => AdminClientError::RateLimited,
        status if status.is_server_error() => AdminClientError::ServerFailure,
        _ => AdminClientError::ProtocolFailure,
    }
}

fn classify_transport(error: reqwest::Error) -> AdminClientError {
    if error.is_timeout() {
        return AdminClientError::Timeout;
    }

    // reqwest does not currently expose a stable dedicated TLS-error predicate.
    // Keep raw transport text out of UI state while recognizing common certificate/
    // TLS failures as a distinct security condition.
    let detail = error.to_string().to_ascii_lowercase();
    if detail.contains("certificate")
        || detail.contains("tls")
        || detail.contains("ssl")
        || detail.contains("unknown issuer")
    {
        return AdminClientError::SecurityFailure;
    }

    if error.is_connect() {
        AdminClientError::Offline
    } else {
        AdminClientError::ProtocolFailure
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn http_statuses_map_to_sanitized_admin_errors() {
        assert_eq!(
            classify_status(StatusCode::UNAUTHORIZED),
            AdminClientError::AuthenticationRequired
        );
        assert_eq!(
            classify_status(StatusCode::FORBIDDEN),
            AdminClientError::Forbidden
        );
        assert_eq!(
            classify_status(StatusCode::TOO_MANY_REQUESTS),
            AdminClientError::RateLimited
        );
        assert_eq!(
            classify_status(StatusCode::INTERNAL_SERVER_ERROR),
            AdminClientError::ServerFailure
        );
        assert_eq!(
            classify_status(StatusCode::BAD_REQUEST),
            AdminClientError::ProtocolFailure
        );
    }

    #[test]
    fn endpoint_security_rejects_remote_plain_http() {
        assert_eq!(
            validate_admin_endpoint("http://127.0.0.1:3000").unwrap(),
            AdminEndpointSecurity::LocalHttp
        );
        assert_eq!(
            validate_admin_endpoint("http://localhost:3000").unwrap(),
            AdminEndpointSecurity::LocalHttp
        );
        assert_eq!(
            validate_admin_endpoint("https://admin.example.test").unwrap(),
            AdminEndpointSecurity::Https
        );
        assert_eq!(
            validate_admin_endpoint("http://admin.example.test"),
            Err(AdminClientError::SecurityFailure)
        );
    }

    #[test]
    fn public_error_messages_do_not_embed_credentials_or_transport_details() {
        for error in [
            AdminClientError::AuthenticationRequired,
            AdminClientError::Forbidden,
            AdminClientError::RateLimited,
            AdminClientError::ServerFailure,
            AdminClientError::Offline,
            AdminClientError::Timeout,
            AdminClientError::SecurityFailure,
            AdminClientError::ProtocolFailure,
        ] {
            let message = error.user_message();
            assert!(!message.contains("Bearer "));
            assert!(!message.contains("SRS_ADMIN_TOKEN"));
            assert!(!message.contains("http://"));
            assert!(!message.contains("https://"));
        }
    }
}
