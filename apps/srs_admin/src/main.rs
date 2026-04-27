use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use eframe::egui;
use libsrs_app_config::SrsConfig;
use libsrs_licensing_proto::{
    AdminActionResponse, AdminCreateNotificationRequest, AdminPendingRequestRecord,
    AdminRecordState, AdminSnapshot, AdminUpdateKeyStatusRequest, AdminUpdateLicenseFeaturesRequest,
    AdminUpdateRecordStateRequest, LicensedFeature, NotificationDeliveryState,
};
use reqwest::blocking::Client;

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions::default();
    eframe::run_native(
        "SRS Admin",
        options,
        Box::new(|_cc| Ok(Box::new(AdminApp::bootstrap()))),
    )
}

struct AdminApp {
    base_url: String,
    client: Option<Client>,
    snapshot: Option<AdminSnapshot>,
    license_presets: BTreeMap<String, LicensePreset>,
    license_features: BTreeMap<String, Vec<LicensedFeature>>,
    pending_delete: Option<DeleteTarget>,
    active_tab: AdminTab,
    notification_license_id: String,
    notification_recipient: String,
    notification_subject: String,
    notification_body: String,
    status: String,
    notifications: Vec<String>,
    auto_refresh: bool,
    last_refresh: Instant,
}

impl AdminApp {
    fn bootstrap() -> Self {
        Self::try_bootstrap().unwrap_or_else(|err| Self::fallback(err.to_string()))
    }

    fn try_bootstrap() -> anyhow::Result<Self> {
        let config = SrsConfig::load()?;
        let client = Client::builder()
            .connect_timeout(Duration::from_millis(config.client.connect_timeout_ms))
            .timeout(Duration::from_millis(config.client.request_timeout_ms))
            .build()?;
        let mut app = Self {
            base_url: config.server.local_base_url(),
            client: Some(client),
            snapshot: None,
            license_presets: BTreeMap::new(),
            license_features: BTreeMap::new(),
            pending_delete: None,
            active_tab: AdminTab::Overview,
            notification_license_id: String::new(),
            notification_recipient: String::new(),
            notification_subject: String::new(),
            notification_body: String::new(),
            status: "Connecting to local licensing server".to_string(),
            notifications: vec![],
            auto_refresh: true,
            last_refresh: Instant::now() - Duration::from_secs(60),
        };
        app.refresh_snapshot();
        Ok(app)
    }

    fn fallback(message: String) -> Self {
        Self {
            base_url: "http://127.0.0.1:3000".to_string(),
            client: None,
            snapshot: None,
            license_presets: BTreeMap::new(),
            license_features: BTreeMap::new(),
            pending_delete: None,
            active_tab: AdminTab::Overview,
            notification_license_id: String::new(),
            notification_recipient: String::new(),
            notification_subject: String::new(),
            notification_body: String::new(),
            status: format!("Admin UI degraded: {message}"),
            notifications: vec![message],
            auto_refresh: false,
            last_refresh: Instant::now(),
        }
    }

    fn refresh_snapshot(&mut self) {
        let Some(client) = &self.client else {
            self.push_notification("HTTP client unavailable.".to_string());
            return;
        };
        let url = format!("{}/api/v1/admin/snapshot", self.base_url.trim_end_matches('/'));
        match client.get(&url).send() {
            Ok(response) => match response.error_for_status() {
                Ok(response) => match response.json::<AdminSnapshot>() {
                    Ok(snapshot) => {
                        self.status = "Admin snapshot refreshed".to_string();
                        self.last_refresh = Instant::now();
                        self.license_presets = snapshot
                            .licenses
                            .iter()
                            .map(|license| {
                                (
                                    license.license_id.clone(),
                                    LicensePreset::from_features(&license.features),
                                )
                            })
                            .collect();
                        self.license_features = snapshot
                            .licenses
                            .iter()
                            .map(|license| (license.license_id.clone(), license.features.clone()))
                            .collect();
                        if self.notification_license_id.is_empty() {
                            if let Some(first) = snapshot.licenses.first() {
                                self.notification_license_id = first.license_id.clone();
                                self.notification_recipient = first.owner_email.clone();
                            }
                        }
                        self.snapshot = Some(snapshot);
                    }
                    Err(err) => self.push_notification(format!("Failed to decode snapshot: {err}")),
                },
                Err(err) => self.push_notification(format!("Snapshot request failed: {err}")),
            },
            Err(err) => self.push_notification(format!("Local admin server unreachable: {err}")),
        }
    }

    fn update_license_features(&mut self, license_id: &str) {
        let Some(client) = &self.client else {
            self.push_notification("HTTP client unavailable.".to_string());
            return;
        };
        let Some(preset) = self.license_presets.get(license_id).copied() else {
            self.push_notification("No license preset found for license.".to_string());
            return;
        };
        let request = AdminUpdateLicenseFeaturesRequest {
            license_id: license_id.to_string(),
            features: match preset {
                LicensePreset::Basic => LicensedFeature::basic_defaults(),
                LicensePreset::Editor => LicensedFeature::editor_defaults(),
                LicensePreset::Custom => self
                    .license_features
                    .get(license_id)
                    .cloned()
                    .unwrap_or_else(LicensedFeature::basic_defaults),
            },
        };
        let url = format!(
            "{}/api/v1/admin/licenses/features",
            self.base_url.trim_end_matches('/')
        );
        self.send_action(
            client.post(url).json(&request),
            "Updated license features.",
        );
    }

    fn update_key_status(&mut self, key_id: &str, active: bool) {
        let Some(client) = &self.client else {
            self.push_notification("HTTP client unavailable.".to_string());
            return;
        };
        let request = AdminUpdateKeyStatusRequest {
            key_id: key_id.to_string(),
            active,
        };
        let url = format!("{}/api/v1/admin/keys/status", self.base_url.trim_end_matches('/'));
        self.send_action(client.post(url).json(&request), "Updated key status.");
    }

    fn set_record_state(&mut self, target: &DeleteTarget, state: AdminRecordState) {
        let Some(client) = &self.client else {
            self.push_notification("HTTP client unavailable.".to_string());
            return;
        };
        let url = format!(
            "{}/{}",
            self.base_url.trim_end_matches('/'),
            target.state_api_path()
        );
        let request = AdminUpdateRecordStateRequest { state };
        self.send_action(
            client.post(url).json(&request),
            &format!("Updated {} to {}.", target.describe(), state.as_str()),
        );
    }

    fn delete_record(&mut self, target: DeleteTarget) {
        if self.pending_delete.as_ref() != Some(&target) {
            self.pending_delete = Some(target.clone());
            self.push_notification(format!(
                "Press '{}' again to confirm delete.",
                target.describe()
            ));
            return;
        }

        let Some(client) = &self.client else {
            self.push_notification("HTTP client unavailable.".to_string());
            return;
        };
        let url = format!(
            "{}/{}",
            self.base_url.trim_end_matches('/'),
            target.state_api_path()
        );
        self.pending_delete = None;
        self.send_action(
            client.post(url).json(&AdminUpdateRecordStateRequest {
                state: AdminRecordState::Deleted,
            }),
            &format!("Deleted {}.", target.describe()),
        );
    }

    fn approve_request(&mut self, request_id: &str) {
        let Some(client) = &self.client else {
            self.push_notification("HTTP client unavailable.".to_string());
            return;
        };
        let url = format!(
            "{}/api/v1/admin/requests/{}/approve",
            self.base_url.trim_end_matches('/'),
            request_id
        );
        self.send_action(client.post(url), "Approved verification request.");
    }

    fn create_notification(&mut self) {
        let Some(client) = &self.client else {
            self.push_notification("HTTP client unavailable.".to_string());
            return;
        };
        let request = AdminCreateNotificationRequest {
            license_id: self.notification_license_id.trim().to_string(),
            recipient: self.notification_recipient.trim().to_string(),
            subject: self.notification_subject.trim().to_string(),
            body: self.notification_body.trim().to_string(),
        };
        let url = format!(
            "{}/api/v1/admin/notifications/create",
            self.base_url.trim_end_matches('/')
        );
        self.send_action(client.post(url).json(&request), "Created notification.");
    }

    fn send_action(
        &mut self,
        request: reqwest::blocking::RequestBuilder,
        success_message: &str,
    ) {
        match request.send() {
            Ok(response) => match response.error_for_status() {
                Ok(response) => match response.json::<AdminActionResponse>() {
                    Ok(action) => {
                        self.push_notification(action.message);
                        self.status = success_message.to_string();
                        self.refresh_snapshot();
                    }
                    Err(err) => self.push_notification(format!("Action decode failed: {err}")),
                },
                Err(err) => self.push_notification(format!("Action failed: {err}")),
            },
            Err(err) => self.push_notification(format!("Action request failed: {err}")),
        }
    }

    fn push_notification(&mut self, message: String) {
        self.status = message.clone();
        self.notifications.push(message);
        if self.notifications.len() > 20 {
            self.notifications.remove(0);
        }
    }

    fn maybe_auto_refresh(&mut self, ctx: &egui::Context) {
        if self.auto_refresh && self.last_refresh.elapsed() >= Duration::from_secs(5) {
            self.refresh_snapshot();
        }
        ctx.request_repaint_after(Duration::from_secs(1));
    }

    fn render_top_bar(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::top("admin_top").show(ctx, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.heading("SRS Admin");
                ui.label(format!("Server: {}", self.base_url));
                if ui.button("Refresh").clicked() {
                    self.refresh_snapshot();
                }
                ui.checkbox(&mut self.auto_refresh, "Auto-refresh");
                ui.label(format!("Status: {}", self.status));
            });
        });
    }

    fn render_tab_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal_wrapped(|ui| {
            for tab in AdminTab::all() {
                let label = tab.label(self.snapshot.as_ref());
                ui.selectable_value(&mut self.active_tab, tab, label);
            }
        });
    }

    fn render_overview(&self, ui: &mut egui::Ui) {
        ui.heading("Overview");
        ui.label("Quick operational summary for the local licensing server.");
        ui.separator();
        self.render_stats(ui);
        ui.separator();

        ui.heading("Current State");
        ui.label(format!("Server endpoint: {}", self.base_url));
        ui.label(format!(
            "Auto-refresh: {}",
            if self.auto_refresh { "enabled" } else { "disabled" }
        ));
        ui.label(format!(
            "Last refresh age: {}s",
            self.last_refresh.elapsed().as_secs()
        ));

        ui.separator();
        ui.heading("Recent Notifications");
        if self.notifications.is_empty() {
            ui.label("No notifications yet.");
        } else {
            for note in self.notifications.iter().rev().take(8) {
                ui.label(note);
            }
        }
    }

    fn render_stats(&self, ui: &mut egui::Ui) {
        let Some(snapshot) = &self.snapshot else {
            ui.label("No snapshot loaded yet.");
            return;
        };

        ui.horizontal_wrapped(|ui| {
            stat_card(ui, "Licenses", snapshot.stats.license_count);
            stat_card(ui, "Keys", snapshot.stats.key_count);
            stat_card(ui, "Active Keys", snapshot.stats.active_key_count);
            stat_card(ui, "Installations", snapshot.stats.installation_count);
            stat_card(ui, "Trusted", snapshot.stats.trusted_installation_count);
            stat_card(ui, "Pending", snapshot.stats.pending_request_count);
            stat_card(ui, "Audit Events", snapshot.stats.audit_event_count);
        });
    }

    fn render_licenses(&mut self, ui: &mut egui::Ui) {
        ui.heading("Licenses");
        let Some(snapshot) = &self.snapshot else {
            ui.label("No license data.");
            return;
        };
        let licenses = snapshot.licenses.clone();
        egui::ScrollArea::vertical()
            .id_salt("admin_licenses_scroll")
            .max_height(220.0)
            .show(ui, |ui| {
            egui::Grid::new("admin_licenses_grid")
                .num_columns(7)
                .striped(true)
                .show(ui, |ui| {
                    ui.strong("License");
                    ui.strong("Owner Email");
                    ui.strong("Active Keys");
                    ui.strong("State");
                    ui.strong("License Type");
                    ui.strong("Effective Features");
                    ui.strong("Actions");
                    ui.end_row();

                    for license in licenses {
                        let mut selected_preset = self
                            .license_presets
                            .get(&license.license_id)
                            .copied()
                            .unwrap_or_else(|| LicensePreset::from_features(&license.features));
                        let mut selected_features = self
                            .license_features
                            .get(&license.license_id)
                            .cloned()
                            .unwrap_or_else(|| normalize_feature_selection(license.features.clone()));
                        let mut update_clicked = false;

                        ui.push_id(("license_row", &license.license_id), |ui| {
                            let preset_before = selected_preset;
                            ui.monospace(&license.license_id);
                            ui.label(&license.owner_email);
                            ui.label(license.active_key_count.to_string());
                            ui.colored_label(
                                record_state_color(license.record_state),
                                license.record_state.as_str(),
                            );
                            ui.horizontal(|ui| {
                                egui::ComboBox::from_id_salt(("license_preset", &license.license_id))
                                    .selected_text(selected_preset.label())
                                    .show_ui(ui, |ui| {
                                        for option in LicensePreset::all() {
                                            ui.selectable_value(
                                                &mut selected_preset,
                                                option,
                                                option.label(),
                                            );
                                        }
                                    });

                                if selected_preset != preset_before {
                                    selected_features = match selected_preset {
                                        LicensePreset::Basic => LicensedFeature::basic_defaults(),
                                        LicensePreset::Editor => LicensedFeature::editor_defaults(),
                                        LicensePreset::Custom => {
                                            normalize_feature_selection(selected_features.clone())
                                        }
                                    };
                                }

                                if selected_preset == LicensePreset::Custom {
                                    ui.menu_button("Custom Features", |ui| {
                                        ui.set_min_width(240.0);
                                        edit_custom_features_ui(ui, &mut selected_features);
                                    });
                                }

                                if ui.button("Update").clicked() {
                                    update_clicked = true;
                                }
                            });
                            ui.label(format_feature_list(&selected_features));
                        });
                        let target = DeleteTarget::License(license.license_id.clone());
                        if let Some(action) = render_state_actions(
                            ui,
                            self.pending_delete.as_ref(),
                            &target,
                            license.record_state,
                        ) {
                            match action {
                                RowAction::SetState(state) => self.set_record_state(&target, state),
                                RowAction::SoftDelete => self.delete_record(target.clone()),
                            }
                        }

                        selected_features = normalize_feature_selection(selected_features);
                        self.license_presets
                            .insert(license.license_id.clone(), selected_preset);
                        self.license_features
                            .insert(license.license_id.clone(), selected_features);
                        if update_clicked {
                            self.update_license_features(&license.license_id);
                        }
                        ui.end_row();
                    }
                });
            });
    }

    fn render_keys(&mut self, ui: &mut egui::Ui) {
        ui.heading("Keys");
        let Some(snapshot) = &self.snapshot else {
            ui.label("No key data.");
            return;
        };
        let keys = snapshot.keys.clone();
        egui::ScrollArea::vertical()
            .id_salt("admin_keys_scroll")
            .max_height(220.0)
            .show(ui, |ui| {
            egui::Grid::new("admin_keys_grid")
                .num_columns(8)
                .striped(true)
                .show(ui, |ui| {
                    ui.strong("Key Id");
                    ui.strong("License");
                    ui.strong("Key");
                    ui.strong("Version");
                    ui.strong("Status");
                    ui.strong("Record State");
                    ui.strong("Created");
                    ui.strong("Actions");
                    ui.end_row();

                    for key in keys {
                        ui.push_id(("key_row", &key.key_id), |ui| {
                            ui.monospace(&key.key_id);
                            ui.monospace(&key.license_id);
                            ui.monospace(&key.key_value);
                            ui.label(key.key_version.to_string());
                            ui.colored_label(
                                if key.active {
                                    egui::Color32::LIGHT_GREEN
                                } else {
                                    egui::Color32::LIGHT_RED
                                },
                                if key.active { "active" } else { "inactive" },
                            );
                            ui.colored_label(
                                record_state_color(key.record_state),
                                key.record_state.as_str(),
                            );
                            ui.label(key.created_at_epoch_s.to_string());
                        });
                        let target = DeleteTarget::Key(key.key_id.clone());
                        ui.horizontal(|ui| {
                            if ui
                                .button(if key.active { "Deactivate" } else { "Activate" })
                                .clicked()
                            {
                                self.update_key_status(&key.key_id, !key.active);
                            }
                            if let Some(action) = render_state_actions(
                                ui,
                                self.pending_delete.as_ref(),
                                &target,
                                key.record_state,
                            ) {
                                match action {
                                    RowAction::SetState(state) => {
                                        self.set_record_state(&target, state)
                                    }
                                    RowAction::SoftDelete => self.delete_record(target.clone()),
                                }
                            }
                        });
                        ui.end_row();
                    }
                });
            });
    }

    fn render_requests(&mut self, ui: &mut egui::Ui) {
        ui.heading("Pending Verification Requests");
        let Some(snapshot) = &self.snapshot else {
            ui.label("No request data.");
            return;
        };
        let requests = snapshot.pending_requests.clone();
        egui::ScrollArea::vertical()
            .id_salt("admin_requests_scroll")
            .max_height(220.0)
            .show(ui, |ui| {
            egui::Grid::new("admin_requests_grid")
                .num_columns(9)
                .striped(true)
                .show(ui, |ui| {
                    ui.strong("Request");
                    ui.strong("License");
                    ui.strong("Device");
                    ui.strong("Requested IP");
                    ui.strong("OS");
                    ui.strong("Hostname");
                    ui.strong("Approval");
                    ui.strong("State");
                    ui.strong("Actions");
                    ui.end_row();

                    for request in requests {
                        ui.push_id(("request_row", &request.request_id), |ui| {
                            ui.monospace(&request.request_id);
                            ui.monospace(&request.license_id);
                            ui.monospace(&request.device_install_id);
                            ui.label(request.requested_ip.as_deref().unwrap_or("unknown"));
                            ui.label(format!("{}/{}", request.requested_os, request.requested_arch));
                            ui.label(request.hostname.as_deref().unwrap_or("unknown"));
                            ui.label(request_status_text(&request));
                            ui.colored_label(
                                record_state_color(request.record_state),
                                request.record_state.as_str(),
                            );
                        });
                        let target = DeleteTarget::Request(request.request_id.clone());
                        ui.horizontal(|ui| {
                            let can_approve = request.approved_at_epoch_s.is_none()
                                && request.record_state == AdminRecordState::Active;
                            ui.add_enabled_ui(can_approve, |ui| {
                                if ui.button("Approve").clicked() {
                                    self.approve_request(&request.request_id);
                                }
                            });
                            if let Some(action) = render_state_actions(
                                ui,
                                self.pending_delete.as_ref(),
                                &target,
                                request.record_state,
                            ) {
                                match action {
                                    RowAction::SetState(state) => self.set_record_state(&target, state),
                                    RowAction::SoftDelete => self.delete_record(target.clone()),
                                }
                            }
                        });
                        ui.end_row();
                    }
                });
            });
    }

    fn render_installations(&mut self, ui: &mut egui::Ui) {
        ui.heading("Connected Installations");
        let Some(snapshot) = &self.snapshot else {
            ui.label("No installation data.");
            return;
        };
        let installations = snapshot.installations.clone();
        egui::ScrollArea::vertical()
            .id_salt("admin_installations_scroll")
            .max_height(240.0)
            .show(ui, |ui| {
            egui::Grid::new("admin_installations_grid")
                .num_columns(9)
                .striped(true)
                .show(ui, |ui| {
                    ui.strong("Installation");
                    ui.strong("License");
                    ui.strong("Device");
                    ui.strong("Last IP");
                    ui.strong("First IP");
                    ui.strong("OS");
                    ui.strong("Hostname");
                    ui.strong("Status");
                    ui.strong("Actions");
                    ui.end_row();

                    for installation in installations {
                        ui.push_id(("installation_row", &installation.installation_id), |ui| {
                            ui.monospace(&installation.installation_id);
                            ui.monospace(&installation.license_id);
                            ui.monospace(&installation.device_install_id);
                            ui.label(installation.last_seen_ip.as_deref().unwrap_or("unknown"));
                            ui.label(installation.first_seen_ip.as_deref().unwrap_or("unknown"));
                            ui.label(format!(
                                "{}/{}",
                                installation.os_family, installation.os_arch
                            ));
                            ui.label(installation.hostname.as_deref().unwrap_or("unknown"));
                            ui.colored_label(
                                if installation.trusted {
                                    egui::Color32::LIGHT_GREEN
                                } else {
                                    egui::Color32::YELLOW
                                },
                                if installation.trusted { "verified" } else { "pending/untrusted" },
                            );
                        });
                        let target = DeleteTarget::Installation(installation.installation_id.clone());
                        if let Some(action) = render_state_actions(
                            ui,
                            self.pending_delete.as_ref(),
                            &target,
                            installation.record_state,
                        ) {
                            match action {
                                RowAction::SetState(state) => self.set_record_state(&target, state),
                                RowAction::SoftDelete => self.delete_record(target.clone()),
                            }
                        }
                        ui.end_row();
                    }
                });
            });
    }

    fn render_audits(&mut self, ui: &mut egui::Ui) {
        ui.heading("Recent Audit / Connection Log");
        let Some(snapshot) = &self.snapshot else {
            ui.label("No audit data.");
            return;
        };
        let audits = snapshot.audits.clone();
        egui::ScrollArea::vertical()
            .id_salt("admin_audits_scroll")
            .max_height(260.0)
            .show(ui, |ui| {
            egui::Grid::new("admin_audits_grid")
                .num_columns(7)
                .striped(true)
                .show(ui, |ui| {
                    ui.strong("Time");
                    ui.strong("License");
                    ui.strong("Event");
                    ui.strong("Key");
                    ui.strong("Installation");
                    ui.strong("Payload");
                    ui.strong("Actions");
                    ui.end_row();

                    for audit in audits {
                        ui.push_id(("audit_row", &audit.event_id), |ui| {
                            ui.label(audit.created_at_epoch_s.to_string());
                            ui.monospace(&audit.license_id);
                            ui.label(&audit.event_type);
                            ui.monospace(audit.key_id.as_deref().unwrap_or("-"));
                            ui.monospace(audit.installation_id.as_deref().unwrap_or("-"));
                            ui.label(&audit.event_payload_json);
                        });
                        let target = DeleteTarget::Audit(audit.event_id.clone());
                        if let Some(action) = render_state_actions(
                            ui,
                            self.pending_delete.as_ref(),
                            &target,
                            audit.record_state,
                        ) {
                            match action {
                                RowAction::SetState(state) => self.set_record_state(&target, state),
                                RowAction::SoftDelete => self.delete_record(target.clone()),
                            }
                        }
                        ui.end_row();
                    }
                });
            });
    }

    fn render_notifications_tab(&mut self, ui: &mut egui::Ui) {
        ui.heading("Notifications");
        let Some(snapshot) = &self.snapshot else {
            ui.label("No notification data.");
            return;
        };
        let licenses = snapshot.licenses.clone();
        let notifications = snapshot.notifications.clone();

        egui::Frame::group(ui.style())
            .fill(egui::Color32::from_rgb(28, 33, 41))
            .inner_margin(egui::Margin::same(10))
            .show(ui, |ui| {
                ui.heading("Create Notification");
                ui.horizontal(|ui| {
                    ui.label("License");
                    egui::ComboBox::from_id_salt("notification_license")
                        .selected_text(if self.notification_license_id.is_empty() {
                            "Select license".to_string()
                        } else {
                            self.notification_license_id.clone()
                        })
                        .show_ui(ui, |ui| {
                            for license in &licenses {
                                if ui
                                    .selectable_value(
                                        &mut self.notification_license_id,
                                        license.license_id.clone(),
                                        format!("{} ({})", license.license_id, license.owner_email),
                                    )
                                    .clicked()
                                {
                                    self.notification_recipient = license.owner_email.clone();
                                }
                            }
                        });
                });
                ui.horizontal(|ui| {
                    ui.label("Recipient");
                    ui.add_sized(
                        [ui.available_width(), 24.0],
                        egui::TextEdit::singleline(&mut self.notification_recipient),
                    );
                });
                ui.horizontal(|ui| {
                    ui.label("Subject");
                    ui.add_sized(
                        [ui.available_width(), 24.0],
                        egui::TextEdit::singleline(&mut self.notification_subject),
                    );
                });
                ui.label("Body");
                ui.add_sized(
                    [ui.available_width(), 90.0],
                    egui::TextEdit::multiline(&mut self.notification_body),
                );
                if ui.button("Create And Send Notification").clicked() {
                    self.create_notification();
                }
            });

        ui.separator();
        ui.heading("Notification Monitor");
        egui::ScrollArea::vertical()
            .id_salt("admin_notifications_tab_scroll")
            .max_height(360.0)
            .show(ui, |ui| {
                egui::Grid::new("admin_notifications_grid")
                    .num_columns(9)
                    .striped(true)
                    .show(ui, |ui| {
                        ui.strong("Created");
                        ui.strong("License");
                        ui.strong("Recipient");
                        ui.strong("Subject");
                        ui.strong("Delivery");
                        ui.strong("Sent");
                        ui.strong("Delivered");
                        ui.strong("Read");
                        ui.strong("Record State");
                        ui.end_row();

                        for notification in notifications {
                            ui.push_id(("notification_row", &notification.email_id), |ui| {
                                ui.label(notification.created_at_epoch_s.to_string());
                                ui.monospace(&notification.license_id);
                                ui.label(&notification.recipient);
                                ui.label(&notification.subject);
                                ui.colored_label(
                                    notification_state_color(notification.notification_state),
                                    notification.notification_state.as_str(),
                                );
                                ui.label(
                                    notification
                                        .sent_at_epoch_s
                                        .map(|value| value.to_string())
                                        .unwrap_or_else(|| "-".to_string()),
                                );
                                ui.label(
                                    notification
                                        .delivered_at_epoch_s
                                        .map(|value| value.to_string())
                                        .unwrap_or_else(|| "-".to_string()),
                                );
                                ui.label(
                                    notification
                                        .read_at_epoch_s
                                        .map(|value| value.to_string())
                                        .unwrap_or_else(|| "-".to_string()),
                                );
                                ui.colored_label(
                                    record_state_color(notification.record_state),
                                    notification.record_state.as_str(),
                                );
                            });
                            ui.end_row();
                        }
                    });
            });
    }

    fn render_playback_requests(&mut self, ui: &mut egui::Ui) {
        ui.heading("Playback Requests");
        let Some(snapshot) = &self.snapshot else {
            ui.label("No playback request data.");
            return;
        };
        let requests = snapshot.playback_requests.clone();
        egui::ScrollArea::vertical()
            .id_salt("admin_playback_requests_scroll")
            .max_height(420.0)
            .show(ui, |ui| {
                egui::Grid::new("admin_playback_requests_grid")
                    .num_columns(8)
                    .striped(true)
                    .show(ui, |ui| {
                        ui.strong("Created");
                        ui.strong("License");
                        ui.strong("Device");
                        ui.strong("Source");
                        ui.strong("App");
                        ui.strong("Unsupported Tracks");
                        ui.strong("State");
                        ui.strong("Action");
                        ui.end_row();

                        for request in requests {
                            ui.push_id(("playback_request_row", &request.playback_request_id), |ui| {
                                ui.label(request.created_at_epoch_s.to_string());
                                ui.monospace(request.license_id.as_deref().unwrap_or("-"));
                                ui.monospace(&request.device_install_id);
                                ui.label(&request.source);
                                ui.label(format!("{} {}", request.app_name, request.app_version));
                                ui.label(&request.tracks_json);
                                ui.colored_label(
                                    record_state_color(request.record_state),
                                    request.record_state.as_str(),
                                );
                                ui.label("-");
                            });
                            ui.end_row();
                        }
                    });
            });
    }

    fn render_notifications(&self, ctx: &egui::Context) {
        egui::TopBottomPanel::bottom("admin_notifications")
            .resizable(true)
            .show(ctx, |ui| {
                ui.heading("Notifications");
                for note in self.notifications.iter().rev().take(8) {
                    ui.label(note);
                }
            });
    }
}

impl eframe::App for AdminApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.maybe_auto_refresh(ctx);
        self.render_top_bar(ctx);
        egui::CentralPanel::default().show(ctx, |ui| {
            self.render_tab_bar(ui);
            ui.separator();
            match self.active_tab {
                AdminTab::Overview => self.render_overview(ui),
                AdminTab::Licenses => self.render_licenses(ui),
                AdminTab::Keys => self.render_keys(ui),
                AdminTab::Requests => self.render_requests(ui),
                AdminTab::Installations => self.render_installations(ui),
                AdminTab::AuditLog => self.render_audits(ui),
                AdminTab::Notifications => self.render_notifications_tab(ui),
                AdminTab::PlaybackRequests => self.render_playback_requests(ui),
            }
        });
        self.render_notifications(ctx);
    }
}

fn stat_card(ui: &mut egui::Ui, label: &str, value: u64) {
    egui::Frame::group(ui.style()).show(ui, |ui| {
        ui.vertical(|ui| {
            ui.strong(label);
            ui.heading(value.to_string());
        });
    });
}

fn delete_button(
    ui: &mut egui::Ui,
    pending_delete: Option<&DeleteTarget>,
    target: &DeleteTarget,
) -> egui::Response {
    let armed = pending_delete == Some(target);
    let label = if armed { "Confirm Delete" } else { "Delete" };
    ui.add(egui::Button::new(label).fill(if armed {
        egui::Color32::from_rgb(150, 35, 35)
    } else {
        egui::Color32::from_rgb(70, 25, 25)
    }))
}

fn record_state_color(state: AdminRecordState) -> egui::Color32 {
    match state {
        AdminRecordState::Active => egui::Color32::LIGHT_GREEN,
        AdminRecordState::Archived => egui::Color32::YELLOW,
        AdminRecordState::Deleted => egui::Color32::LIGHT_RED,
    }
}

fn notification_state_color(state: NotificationDeliveryState) -> egui::Color32 {
    match state {
        NotificationDeliveryState::Queued => egui::Color32::from_rgb(170, 170, 170),
        NotificationDeliveryState::Sent => egui::Color32::from_rgb(114, 179, 255),
        NotificationDeliveryState::Delivered => egui::Color32::from_rgb(114, 212, 139),
        NotificationDeliveryState::Read => egui::Color32::from_rgb(196, 132, 255),
    }
}

fn render_state_actions(
    ui: &mut egui::Ui,
    pending_delete: Option<&DeleteTarget>,
    target: &DeleteTarget,
    state: AdminRecordState,
)-> Option<RowAction> {
    let mut action = None;
    ui.horizontal_wrapped(|ui| match state {
        AdminRecordState::Active => {
            if ui.button("Archive").clicked() {
                action = Some(RowAction::SetState(AdminRecordState::Archived));
            }
            if delete_button(ui, pending_delete, target).clicked() {
                action = Some(RowAction::SoftDelete);
            }
        }
        AdminRecordState::Archived => {
            if ui.button("Restore").clicked() {
                action = Some(RowAction::SetState(AdminRecordState::Active));
            }
            if delete_button(ui, pending_delete, target).clicked() {
                action = Some(RowAction::SoftDelete);
            }
        }
        AdminRecordState::Deleted => {
            if ui.button("Restore").clicked() {
                action = Some(RowAction::SetState(AdminRecordState::Active));
            }
        }
    });
    action
}

fn edit_custom_features_ui(ui: &mut egui::Ui, features: &mut Vec<LicensedFeature>) {
    let mut normalized = normalize_feature_selection(features.clone());
    for feature in LicensedFeature::all() {
        let mut enabled = normalized.contains(&feature);
        let label = if feature == LicensedFeature::Basic {
            format!("{} (required)", feature.as_str())
        } else {
            feature.as_str().to_string()
        };
        let response = ui.add_enabled(
            feature != LicensedFeature::Basic,
            egui::Checkbox::new(&mut enabled, label),
        );
        if feature != LicensedFeature::Basic && response.changed() {
            if enabled {
                normalized.push(feature);
            } else {
                normalized.retain(|existing| *existing != feature);
            }
            normalized = normalize_feature_selection(normalized);
        }
    }
    *features = normalize_feature_selection(normalized);
}

fn normalize_feature_selection(mut features: Vec<LicensedFeature>) -> Vec<LicensedFeature> {
    if !features.contains(&LicensedFeature::Basic) {
        features.push(LicensedFeature::Basic);
    }
    features.sort_by_key(|feature| feature.as_str());
    features.dedup();
    features
}

fn format_feature_list(features: &[LicensedFeature]) -> String {
    normalize_feature_selection(features.to_vec())
        .iter()
        .map(|feature| feature.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

fn request_status_text(request: &AdminPendingRequestRecord) -> String {
    if let Some(approved_at) = request.approved_at_epoch_s {
        format!("approved at {}", approved_at)
    } else {
        format!("pending until {}", request.expires_at_epoch_s)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LicensePreset {
    Basic,
    Editor,
    Custom,
}

impl LicensePreset {
    fn all() -> [Self; 3] {
        [Self::Basic, Self::Editor, Self::Custom]
    }

    fn from_features(features: &[LicensedFeature]) -> Self {
        if normalize_feature_selection(features.to_vec())
            == normalize_feature_selection(LicensedFeature::basic_defaults())
        {
            Self::Basic
        } else if normalize_feature_selection(features.to_vec())
            == normalize_feature_selection(LicensedFeature::editor_defaults())
        {
            Self::Editor
        } else {
            Self::Custom
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Basic => "Basic",
            Self::Editor => "Editor",
            Self::Custom => "Custom",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AdminTab {
    Overview,
    Licenses,
    Keys,
    Requests,
    Installations,
    AuditLog,
    Notifications,
    PlaybackRequests,
}

impl AdminTab {
    fn all() -> [Self; 8] {
        [
            Self::Overview,
            Self::Licenses,
            Self::Keys,
            Self::Requests,
            Self::Installations,
            Self::AuditLog,
            Self::Notifications,
            Self::PlaybackRequests,
        ]
    }

    fn label(self, snapshot: Option<&AdminSnapshot>) -> String {
        match self {
            Self::Overview => "Overview".to_string(),
            Self::Licenses => format!(
                "Licenses ({})",
                snapshot.map(|snapshot| snapshot.licenses.len()).unwrap_or(0)
            ),
            Self::Keys => format!(
                "Keys ({})",
                snapshot.map(|snapshot| snapshot.keys.len()).unwrap_or(0)
            ),
            Self::Requests => format!(
                "Requests ({})",
                snapshot
                    .map(|snapshot| snapshot.pending_requests.len())
                    .unwrap_or(0)
            ),
            Self::Installations => format!(
                "Installations ({})",
                snapshot
                    .map(|snapshot| snapshot.installations.len())
                    .unwrap_or(0)
            ),
            Self::AuditLog => format!(
                "Audit Log ({})",
                snapshot.map(|snapshot| snapshot.audits.len()).unwrap_or(0)
            ),
            Self::Notifications => format!(
                "Notifications ({})",
                snapshot
                    .map(|snapshot| snapshot.notifications.len())
                    .unwrap_or(0)
            ),
            Self::PlaybackRequests => format!(
                "Playback Requests ({})",
                snapshot
                    .map(|snapshot| snapshot.playback_requests.len())
                    .unwrap_or(0)
            ),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum DeleteTarget {
    License(String),
    Key(String),
    Installation(String),
    Request(String),
    Audit(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RowAction {
    SetState(AdminRecordState),
    SoftDelete,
}

impl DeleteTarget {
    fn state_api_path(&self) -> String {
        match self {
            Self::License(id) => format!("api/v1/admin/licenses/{id}/state"),
            Self::Key(id) => format!("api/v1/admin/keys/{id}/state"),
            Self::Installation(id) => format!("api/v1/admin/installations/{id}/state"),
            Self::Request(id) => format!("api/v1/admin/requests/{id}/state"),
            Self::Audit(id) => format!("api/v1/admin/audits/{id}/state"),
        }
    }

    fn describe(&self) -> String {
        match self {
            Self::License(id) => format!("license {id}"),
            Self::Key(id) => format!("key {id}"),
            Self::Installation(id) => format!("installation {id}"),
            Self::Request(id) => format!("verification request {id}"),
            Self::Audit(id) => format!("audit event {id}"),
        }
    }
}
