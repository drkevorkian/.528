//! SMTP delivery isolated from SQLite.
//!
//! Logging must never include SMTP passwords, raw confirmation tokens, or full
//! message bodies. Failure returns `false` and leaves outbox rows queued so a
//! later pass can retry without corrupting state.

use lettre::transport::smtp::authentication::Credentials;
use lettre::{Message, SmtpTransport, Transport};
use libsrs_app_config::ServerConfig;
use tracing::info;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MailDelivery {
    /// SMTP accepted the message, or log-fallback succeeded.
    Delivered,
    /// SMTP/config failed. Outbox row must stay `queued`.
    Failed,
    /// No SMTP configured; message was recorded to the redacted log only.
    LoggedOnly,
}

fn redact_addr(value: &str) -> String {
    let value = value.trim();
    if let Some((user, host)) = value.split_once('@') {
        let user_keep = user.chars().next().unwrap_or('*');
        return format!("{user_keep}***@{host}");
    }
    if value.len() <= 2 {
        return "***".to_string();
    }
    format!("{}***", &value[..1])
}

pub fn deliver_email(
    recipient: &str,
    subject: &str,
    body: &str,
    config: &ServerConfig,
) -> MailDelivery {
    let _ = body; // body may contain confirmation tokens; never log it
    if let (Some(mail_from), Some(smtp_server)) = (&config.mail_from, &config.smtp_server) {
        let email = match Message::builder()
            .from(match mail_from.parse() {
                Ok(value) => value,
                Err(_) => {
                    info!(target: "srs_license_server::mailer", "invalid mail_from");
                    return MailDelivery::Failed;
                }
            })
            .to(match recipient.parse() {
                Ok(value) => value,
                Err(_) => {
                    info!(target: "srs_license_server::mailer", "invalid recipient");
                    return MailDelivery::Failed;
                }
            })
            .subject(subject)
            .body(body.to_string())
        {
            Ok(email) => email,
            Err(_) => {
                info!(target: "srs_license_server::mailer", "message build failed");
                return MailDelivery::Failed;
            }
        };

        let mut builder = SmtpTransport::builder_dangerous(smtp_server);
        if let (Some(username), Some(password)) = (&config.smtp_username, &config.smtp_password) {
            builder = builder.credentials(Credentials::new(username.clone(), password.clone()));
        }
        let mailer = builder.build();
        return match mailer.send(&email) {
            Ok(_) => {
                info!(
                    target: "srs_license_server::mailer",
                    recipient = %redact_addr(recipient),
                    "smtp accepted message"
                );
                MailDelivery::Delivered
            }
            Err(_) => {
                info!(
                    target: "srs_license_server::mailer",
                    recipient = %redact_addr(recipient),
                    "smtp delivery failed"
                );
                MailDelivery::Failed
            }
        };
    }

    info!(
        target: "srs_license_server::mailer",
        mode = "log",
        recipient = %redact_addr(recipient),
        subject_len = subject.len(),
        body_len = body.len(),
        "mail logged without smtp"
    );
    MailDelivery::LoggedOnly
}

#[cfg(test)]
mod tests {
    use super::redact_addr;

    #[test]
    fn redact_keeps_domain() {
        assert_eq!(redact_addr("owner@example.com"), "o***@example.com");
    }

    #[test]
    fn redact_does_not_echo_token_like_local_part() {
        let redacted = redact_addr("confirm-token-secret@example.com");
        assert!(!redacted.contains("confirm-token-secret"));
    }
}
