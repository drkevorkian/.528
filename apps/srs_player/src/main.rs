use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use eframe::egui;
use libsrs_app_config::SrsConfig;
use libsrs_app_services::{AppServices, MediaInspection};
use libsrs_licensing_client::{EffectiveMode, LicenseSnapshot, LicensingClient, VerificationState};
use libsrs_licensing_proto::{ClientNotification, EntitlementClaims, UnsupportedCodecTrack};
use rfd::FileDialog;

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions::default();
    eframe::run_native(
        "SRS Player",
        options,
        Box::new(|cc| {
            apply_player_theme(&cc.egui_ctx);
            Ok(Box::new(PlayerApp::bootstrap()))
        }),
    )
}

struct PlayerApp {
    services: AppServices,
    licensing: Option<LicensingClient>,
    input_path: String,
    output_path: String,
    license_key_input: String,
    status: String,
    notifications: Vec<NotificationEntry>,
    seen_server_notifications: Vec<String>,
    last_license_notice_key: Option<String>,
    recent_files: Vec<String>,
    current_media: Option<MediaInspection>,
    license_snapshot: LicenseSnapshot,
    show_license_popup: bool,
    workspace: WorkspaceTab,
    playback: PlaybackWorkspace,
    editor: EditorWorkspace,
    primary_url: String,
    backup_url: String,
    last_tick: Instant,
    next_auto_refresh_at: Instant,
    refresh_prng_state: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorkspaceTab {
    PlayOnly,
    Editor,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EditorTab {
    Pipeline,
    Timeline,
    FrameTools,
}

struct PlaybackWorkspace {
    playing: bool,
    position_ms: u64,
    duration_ms: u64,
    skip_ms: u64,
    debug_stats: String,
}

struct EditorWorkspace {
    active_tab: EditorTab,
    selection_start_ms: u64,
    selection_end_ms: u64,
    frame_cursor_ms: u64,
    last_result: String,
}

#[derive(Debug, Clone)]
struct NotificationEntry {
    message: String,
    created_at: Instant,
    expires_at: Instant,
}

impl NotificationEntry {
    fn new(message: String) -> Self {
        let created_at = Instant::now();
        Self {
            message,
            created_at,
            expires_at: created_at + Duration::from_secs(3),
        }
    }

    fn is_active(&self) -> bool {
        Instant::now() < self.expires_at
    }
}

impl PlayerApp {
    fn bootstrap() -> Self {
        Self::try_bootstrap().unwrap_or_else(|err| Self::fallback(err.to_string()))
    }

    fn try_bootstrap() -> anyhow::Result<Self> {
        let config = SrsConfig::load()?;
        let licensing = LicensingClient::new(config.client.clone())?;
        let license_key_input = licensing.current_key().unwrap_or_default();
        let mut app = Self {
            services: AppServices::default(),
            licensing: Some(licensing),
            input_path: String::new(),
            output_path: String::new(),
            license_key_input,
            status: "Idle".to_string(),
            notifications: vec![],
            seen_server_notifications: vec![],
            last_license_notice_key: None,
            recent_files: vec![],
            current_media: None,
            license_snapshot: missing_snapshot("No verification performed yet.".to_string()),
            show_license_popup: false,
            workspace: WorkspaceTab::PlayOnly,
            playback: PlaybackWorkspace {
                playing: false,
                position_ms: 0,
                duration_ms: 5_000,
                skip_ms: 5_000,
                debug_stats: "fps=n/a, dropped=0, queue=0".to_string(),
            },
            editor: EditorWorkspace {
                active_tab: EditorTab::Pipeline,
                selection_start_ms: 0,
                selection_end_ms: 0,
                frame_cursor_ms: 0,
                last_result: "Editor actions idle".to_string(),
            },
            primary_url: config.client.primary_url,
            backup_url: config.client.backup_url,
            last_tick: Instant::now(),
            next_auto_refresh_at: Instant::now() + Duration::from_secs(30),
            refresh_prng_state: seed_refresh_prng(),
        };
        app.refresh_license();
        app.schedule_next_auto_refresh();
        Ok(app)
    }

    fn fallback(message: String) -> Self {
        Self {
            services: AppServices::default(),
            licensing: None,
            input_path: String::new(),
            output_path: String::new(),
            license_key_input: String::new(),
            status: format!("Startup degraded: {message}"),
            notifications: vec![NotificationEntry::new(message)],
            seen_server_notifications: vec![],
            last_license_notice_key: None,
            recent_files: vec![],
            current_media: None,
            license_snapshot: missing_snapshot("Licensing client unavailable.".to_string()),
            show_license_popup: false,
            workspace: WorkspaceTab::PlayOnly,
            playback: PlaybackWorkspace {
                playing: false,
                position_ms: 0,
                duration_ms: 5_000,
                skip_ms: 5_000,
                debug_stats: "fps=n/a, dropped=0, queue=0".to_string(),
            },
            editor: EditorWorkspace {
                active_tab: EditorTab::Pipeline,
                selection_start_ms: 0,
                selection_end_ms: 0,
                frame_cursor_ms: 0,
                last_result: "Editor unavailable".to_string(),
            },
            primary_url: "http://localhost:3000".to_string(),
            backup_url: "http://127.0.0.1:3000".to_string(),
            last_tick: Instant::now(),
            next_auto_refresh_at: Instant::now() + Duration::from_secs(30),
            refresh_prng_state: seed_refresh_prng(),
        }
    }

    fn refresh_license(&mut self) {
        let snapshot = match &self.licensing {
            Some(client) => client.refresh_entitlement("srs-player", env!("CARGO_PKG_VERSION")),
            None => missing_snapshot("Licensing client not initialized.".to_string()),
        };
        self.license_key_input = snapshot.current_key.clone().unwrap_or_default();
        self.apply_license_snapshot(snapshot);
    }

    fn apply_license_snapshot(&mut self, snapshot: LicenseSnapshot) {
        let message = snapshot.message.clone();
        let notice_key = format!(
            "{:?}|{:?}|{}",
            snapshot.verification_state, snapshot.effective_mode, message
        );
        self.workspace = if snapshot.allows_editor() {
            WorkspaceTab::Editor
        } else {
            WorkspaceTab::PlayOnly
        };
        if self.workspace != WorkspaceTab::Editor {
            self.editor.last_result = "Editor mode is gated by a verified key.".to_string();
        }
        self.status = match snapshot.effective_mode {
            EffectiveMode::Editor => "Editor mode verified".to_string(),
            EffectiveMode::PlayOnly => "Play-only mode".to_string(),
        };
        if self.last_license_notice_key.as_deref() != Some(&notice_key) {
            self.push_notification(message);
            self.last_license_notice_key = Some(notice_key);
        }
        for notification in &snapshot.server_notifications {
            self.push_server_notification(notification);
        }
        self.license_snapshot = snapshot;
    }

    fn schedule_next_auto_refresh(&mut self) {
        let interval_secs = next_refresh_interval_secs(
            &mut self.refresh_prng_state,
            runtime_refresh_entropy(),
        );
        self.next_auto_refresh_at = Instant::now() + Duration::from_secs(interval_secs);
    }

    fn maybe_auto_refresh_license(&mut self, ctx: &egui::Context) {
        if self.licensing.is_some() && Instant::now() >= self.next_auto_refresh_at {
            self.refresh_license();
            self.schedule_next_auto_refresh();
        }

        let until_next = self
            .next_auto_refresh_at
            .saturating_duration_since(Instant::now());
        ctx.request_repaint_after(auto_refresh_repaint_delay(until_next));
    }

    fn seconds_until_auto_refresh(&self) -> u64 {
        self.next_auto_refresh_at
            .saturating_duration_since(Instant::now())
            .as_secs()
    }

    fn open_media(&mut self) {
        if self.input_path.trim().is_empty() {
            self.push_notification("Open requested with empty path.".to_string());
            return;
        }
        match self.services.inspect_media(&self.input_path) {
            Ok(inspection) => {
                self.playback.duration_ms = inspection.duration_for_ui();
                self.playback.position_ms = 0;
                self.playback.playing = false;
                self.editor.selection_start_ms = 0;
                self.editor.selection_end_ms = self.playback.duration_ms;
                self.editor.frame_cursor_ms = 0;
                self.playback.debug_stats = format!(
                    "format={} tracks={} packets={:?} frames={:?}",
                    inspection.format_name,
                    inspection.tracks.len(),
                    inspection.packet_count,
                    inspection.frame_count
                );
                self.status = format!("Opened {}", self.input_path);
                self.current_media = Some(inspection);
                self.add_recent_file(self.input_path.clone());
                if self.output_path.is_empty() {
                    self.output_path = suggest_output_path(&self.input_path);
                }
            }
            Err(err) => self.push_notification(format!("Open failed: {err}")),
        }
    }

    fn close_media(&mut self) {
        self.current_media = None;
        self.playback.playing = false;
        self.playback.position_ms = 0;
        self.status = "Closed current media".to_string();
    }

    fn save_license_key(&mut self) {
        let Some(client) = &self.licensing else {
            self.push_notification("Licensing client unavailable.".to_string());
            return;
        };
        match client.set_license_key(self.license_key_input.trim().to_string()) {
            Ok(()) => self.refresh_license(),
            Err(err) => self.push_notification(format!("Failed to store key: {err}")),
        }
    }

    fn play(&mut self) {
        if self.current_media.is_none() {
            self.push_notification("Open media before playing.".to_string());
            return;
        }
        let unsupported = self.unsupported_codec_tracks();
        if !unsupported.is_empty() {
            self.report_unsupported_playback(unsupported);
            self.push_notification(
                "Playback blocked: this file contains unsupported or license-sensitive codecs."
                    .to_string(),
            );
            self.playback.playing = false;
            self.status = "Playback blocked by codec policy".to_string();
            return;
        }
        self.playback.playing = true;
        self.status = "Playing".to_string();
    }

    fn unsupported_codec_tracks(&self) -> Vec<UnsupportedCodecTrack> {
        self.current_media
            .as_ref()
            .map(|media| {
                media
                    .tracks
                    .iter()
                    .filter(|track| !track.supported_without_license)
                    .map(|track| UnsupportedCodecTrack {
                        track_id: track.id,
                        kind: track.kind.clone(),
                        codec: track.codec.clone(),
                        detail: track.detail.clone(),
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    fn report_unsupported_playback(&mut self, tracks: Vec<UnsupportedCodecTrack>) {
        let Some(client) = &self.licensing else {
            self.push_notification("Unsupported playback report skipped: no licensing client.".to_string());
            return;
        };
        let license_id = self.license_snapshot.claims.as_ref().map(|claims| claims.license_id.as_str());
        let endpoint = self.license_snapshot.endpoint.as_deref();
        if let Err(err) = client.report_unsupported_playback(
            endpoint,
            license_id,
            &self.input_path,
            tracks,
            "srs-player",
            env!("CARGO_PKG_VERSION"),
        ) {
            self.push_notification(format!("Unsupported playback report failed: {err}"));
        }
    }

    fn pause(&mut self) {
        self.playback.playing = false;
        self.status = "Paused".to_string();
    }

    fn stop(&mut self) {
        self.playback.playing = false;
        self.playback.position_ms = 0;
        self.status = "Stopped".to_string();
    }

    fn skip_by(&mut self, delta_ms: i64) {
        let max = self.playback.duration_ms.max(1) as i64;
        let next = (self.playback.position_ms as i64 + delta_ms).clamp(0, max);
        self.playback.position_ms = next as u64;
        self.editor.frame_cursor_ms = self.playback.position_ms;
    }

    fn update_playback_tick(&mut self, ctx: &egui::Context) {
        let now = Instant::now();
        let elapsed = now.saturating_duration_since(self.last_tick);
        self.last_tick = now;
        if self.playback.playing {
            self.playback.position_ms = self
                .playback
                .position_ms
                .saturating_add(elapsed.as_millis() as u64);
            if self.playback.position_ms >= self.playback.duration_ms {
                self.playback.position_ms = self.playback.duration_ms;
                self.playback.playing = false;
                self.status = "Reached end of media".to_string();
            }
            self.editor.frame_cursor_ms = self.playback.position_ms;
            ctx.request_repaint_after(Duration::from_millis(33));
        }
    }

    fn run_editor_action(&mut self, action: EditorAction) {
        let Some(claims) = self.editor_claims() else {
            self.push_notification(self.license_snapshot.message.clone());
            return;
        };
        if self.input_path.trim().is_empty() {
            self.push_notification("Select an input path before running editor actions.".to_string());
            return;
        }
        if self.output_path.trim().is_empty() {
            self.push_notification("Set an output path before running editor actions.".to_string());
            return;
        }

        let input = PathBuf::from(self.input_path.trim());
        let output = PathBuf::from(self.output_path.trim());
        let result = match action {
            EditorAction::Encode => self.services.encode_input_to_native(&input, &output, claims),
            EditorAction::Decode => self.services.decode_native_to_raw(&input, &output, claims),
            EditorAction::Mux => self.services.mux_elementary_streams(&input, &output, claims),
            EditorAction::Demux => {
                self.services
                    .demux_container_to_elementary(&input, &output, claims)
            }
            EditorAction::Import => self
                .services
                .import_to_native(&input, &output, claims)
                .map(|_| ()),
            EditorAction::Transcode | EditorAction::Compress => self
                .services
                .transcode_to_native(&input, &output, claims)
                .map(|_| ()),
        };
        match result {
            Ok(()) => {
                let action_name = action.label();
                self.editor.last_result =
                    format!("{action_name} completed: {} -> {}", input.display(), output.display());
                self.push_notification(self.editor.last_result.clone());
            }
            Err(err) => self.push_notification(format!("{} failed: {err}", action.label())),
        }
    }

    fn editor_claims(&self) -> Option<&EntitlementClaims> {
        if self.license_snapshot.allows_editor() {
            self.license_snapshot.claims.as_ref()
        } else {
            None
        }
    }

    fn add_recent_file(&mut self, path: String) {
        self.recent_files.retain(|existing| existing != &path);
        self.recent_files.insert(0, path);
        self.recent_files.truncate(8);
    }

    fn push_notification(&mut self, message: String) {
        self.notifications.push(NotificationEntry::new(message));
        if self.notifications.len() > 50 {
            self.notifications.remove(0);
        }
    }

    fn push_server_notification(&mut self, notification: &ClientNotification) {
        if self
            .seen_server_notifications
            .iter()
            .any(|id| id == &notification.notification_id)
        {
            return;
        }
        self.seen_server_notifications
            .push(notification.notification_id.clone());
        if self.seen_server_notifications.len() > 200 {
            self.seen_server_notifications.remove(0);
        }
        self.push_notification(format!(
            "{}: {}",
            notification.subject, notification.body
        ));
    }

    fn render_top_bar(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::top("top_bar")
            .resizable(false)
            .show(ctx, |ui| {
                styled_section(ui, "SRS Player", |ui| {
                    ui.horizontal_wrapped(|ui| {
                        ui.heading(
                            egui::RichText::new("SRS Player")
                                .size(24.0)
                                .color(accent_blue()),
                                );
                        badge(
                            ui,
                            if self.license_snapshot.allows_editor() {
                                "EDITOR"
                            } else {
                                "PLAY ONLY"
                            },
                            if self.license_snapshot.allows_editor() {
                                accent_green()
                            } else {
                                accent_amber()
                            },
                        );
                        badge(
                            ui,
                            match self.license_snapshot.verification_state {
                                VerificationState::Verified => "VERIFIED",
                                VerificationState::PendingConfirmation => "PENDING",
                                VerificationState::OfflineFallback => "OFFLINE FALLBACK",
                                VerificationState::MissingKey => "NO KEY",
                                VerificationState::Revoked => "REVOKED",
                                VerificationState::ReplacementIssued => "REPLACED",
                                VerificationState::InvalidResponse => "INVALID",
                            },
                            verification_color(self.license_snapshot.verification_state),
                        );
                        ui.separator();
                        ui.label(
                            egui::RichText::new(format!("Status: {}", self.status))
                                .color(muted_text()),
                        );
                        ui.label(
                            egui::RichText::new(format!(
                                "Auto verify in {}s",
                                self.seconds_until_auto_refresh()
                            ))
                            .color(muted_text()),
                        );
                        ui.separator();
                        if secondary_button(ui, "License").clicked() {
                            self.show_license_popup = true;
                        }
                        ui.menu_button(
                            format!("Notifications ({})", self.notifications.len()),
                            |ui| {
                                ui.set_min_width(320.0);
                                if self.notifications.is_empty() {
                                    ui.label("No notifications yet.");
                                } else {
                                    for note in self.notifications.iter().rev().take(12) {
                                        egui::Frame::group(ui.style())
                                            .fill(panel_fill_alt())
                                            .stroke(egui::Stroke::new(1.0, panel_stroke()))
                                            .inner_margin(egui::Margin::same(6))
                                            .show(ui, |ui| {
                                                ui.label(
                                                    egui::RichText::new(&note.message)
                                                        .color(if note.is_active() {
                                                            accent_blue()
                                                        } else {
                                                            egui::Color32::WHITE
                                                        }),
                                                );
                                                ui.label(
                                                    egui::RichText::new(format!(
                                                        "age: {}s",
                                                        note.created_at.elapsed().as_secs()
                                                    ))
                                                    .size(11.0)
                                                    .color(muted_text()),
                                                );
                                            });
                                        ui.add_space(4.0);
                                    }
                                }
                            },
                        );
                    });

                    ui.add_space(6.0);

                    ui.horizontal_wrapped(|ui| {
                        ui.label(egui::RichText::new("Source").strong().color(muted_text()));
                        ui.add_sized(
                            [ui.available_width().max(260.0) - 520.0, 30.0],
                            egui::TextEdit::singleline(&mut self.input_path),
                        );
                        if secondary_button(ui, "Browse").clicked() {
                            if let Some(path) = FileDialog::new().pick_file() {
                                self.input_path = path.display().to_string();
                            }
                        }
                        if primary_button(ui, "Open").clicked() {
                            self.open_media();
                        }
                        if secondary_button(ui, "Close").clicked() {
                            self.close_media();
                        }
                        ui.separator();
                        if primary_button(ui, "Play").clicked() {
                            self.play();
                        }
                        if secondary_button(ui, "Pause").clicked() {
                            self.pause();
                        }
                        if destructive_button(ui, "Stop").clicked() {
                            self.stop();
                        }
                        if secondary_button(ui, "- Skip").clicked() {
                            self.skip_by(-(self.playback.skip_ms as i64));
                        }
                        if secondary_button(ui, "+ Skip").clicked() {
                            self.skip_by(self.playback.skip_ms as i64);
                        }
                    });
                });
        });
    }

    fn render_side_panel(&mut self, ctx: &egui::Context) {
        egui::SidePanel::left("side_panel")
            .resizable(true)
            .default_width(250.0)
            .show(ctx, |ui| {
                styled_section(ui, "Inspector", |ui| {
                    if let Some(media) = &self.current_media {
                        key_value_row(ui, "Format", &media.format_name);
                        key_value_row(
                            ui,
                            "Duration",
                            &media
                                .duration_ms
                                .map(|ms| format!("{ms} ms"))
                                .unwrap_or_else(|| "unknown".to_string()),
                        );
                        if let Some(packet_count) = media.packet_count {
                            key_value_row(ui, "Packets", &packet_count.to_string());
                        }
                        if let Some(frame_count) = media.frame_count {
                            key_value_row(ui, "Frames", &frame_count.to_string());
                        }
                        ui.label(egui::RichText::new(&media.summary).color(muted_text()));
                    } else {
                        ui.label("No media loaded.");
                    }
                });

                ui.add_space(8.0);

                styled_section(ui, "Recent Files", |ui| {
                    let recent_files = self.recent_files.clone();
                    if recent_files.is_empty() {
                        ui.label("No recent files yet.");
                    } else {
                        for recent in recent_files {
                            if secondary_button(ui, &recent).clicked() {
                                self.input_path = recent;
                                self.open_media();
                            }
                        }
                    }
                });

                ui.add_space(8.0);

                styled_section(ui, "Tracks", |ui| {
                    if let Some(media) = &self.current_media {
                        for track in &media.tracks {
                            egui::Frame::group(ui.style())
                                .fill(panel_fill_alt())
                                .stroke(egui::Stroke::new(1.0, panel_stroke()))
                                .inner_margin(egui::Margin::same(8))
                                .show(ui, |ui| {
                                    ui.horizontal_wrapped(|ui| {
                                        ui.label(
                                            egui::RichText::new(format!(
                                                "{}  {}",
                                                track.kind, track.codec
                                            ))
                                            .strong()
                                            .color(accent_blue()),
                                        );
                                        badge(
                                            ui,
                                            if track.supported_without_license {
                                                "NO-LICENSE OK"
                                            } else {
                                                "BLOCKED"
                                            },
                                            if track.supported_without_license {
                                                accent_green()
                                            } else {
                                                egui::Color32::from_rgb(220, 92, 92)
                                            },
                                        );
                                    });
                                    ui.label(
                                        egui::RichText::new(format!(
                                            "role={}  detail={}",
                                            track.role, track.detail
                                        ))
                                        .color(muted_text()),
                                    );
                                });
                            ui.add_space(4.0);
                        }
                    } else {
                        ui.label("No track metadata available.");
                    }
                });
            });
    }

    fn render_license_popup(&mut self, ctx: &egui::Context) {
        if !self.show_license_popup {
            return;
        }

        let mut open = self.show_license_popup;
        egui::Window::new("License & Access")
            .open(&mut open)
            .collapsible(false)
            .resizable(true)
            .default_width(420.0)
            .show(ctx, |ui| {
                styled_section(ui, "License Status", |ui| {
                    badge(
                        ui,
                        match self.license_snapshot.effective_mode {
                            EffectiveMode::Editor => "EDITOR VERIFIED",
                            EffectiveMode::PlayOnly => "PLAY ONLY",
                        },
                        match self.license_snapshot.effective_mode {
                            EffectiveMode::Editor => accent_green(),
                            EffectiveMode::PlayOnly => accent_amber(),
                        },
                    );
                    ui.add_space(6.0);
                    key_value_row(ui, "Primary", &self.primary_url);
                    key_value_row(ui, "Backup", &self.backup_url);
                    key_value_row(
                        ui,
                        "Verification",
                        &format!("{:?}", self.license_snapshot.verification_state),
                    );
                    key_value_row(
                        ui,
                        "Auto Verify",
                        &format!("{}s", self.seconds_until_auto_refresh()),
                    );
                    ui.label(
                        egui::RichText::new(&self.license_snapshot.message).color(muted_text()),
                    );
                });

                ui.add_space(8.0);

                styled_section(ui, "Key Management", |ui| {
                    ui.label(egui::RichText::new("License Key").strong().color(muted_text()));
                    ui.add_sized(
                        [ui.available_width(), 30.0],
                        egui::TextEdit::singleline(&mut self.license_key_input),
                    );
                    ui.horizontal(|ui| {
                        if primary_button(ui, "Save Key").clicked() {
                            self.save_license_key();
                        }
                        if secondary_button(ui, "Refresh").clicked() {
                            self.refresh_license();
                        }
                    });
                });

                ui.add_space(8.0);

                styled_section(ui, "Entitled Features", |ui| {
                    if let Some(claims) = &self.license_snapshot.claims {
                        if claims.features.is_empty() {
                            ui.label("No features assigned.");
                        } else {
                            for feature in &claims.features {
                                ui.label(format!("• {}", format!("{feature:?}").to_lowercase()));
                            }
                        }
                    } else {
                        ui.label("No verified entitlement loaded.");
                    }
                });
            });
        self.show_license_popup = open;
    }

    fn render_workspace(&mut self, ctx: &egui::Context) {
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.horizontal_wrapped(|ui| {
                if workspace_tab_button(ui, self.workspace == WorkspaceTab::PlayOnly, "Playback").clicked() {
                    self.workspace = WorkspaceTab::PlayOnly;
                }
                ui.add_enabled_ui(self.license_snapshot.allows_editor(), |ui| {
                    if workspace_tab_button(ui, self.workspace == WorkspaceTab::Editor, "Editor").clicked() {
                        self.workspace = WorkspaceTab::Editor;
                    }
                });
                if !self.license_snapshot.allows_editor() {
                    ui.label(
                        egui::RichText::new(
                            "Editor workspace unlocks after verified editor entitlement.",
                        )
                        .color(muted_text()),
                    );
                }
            });
            ui.separator();
            match self.workspace {
                WorkspaceTab::PlayOnly => self.render_playback_workspace(ui),
                WorkspaceTab::Editor => self.render_editor_workspace(ui),
            }
        });
    }

    fn render_playback_workspace(&mut self, ui: &mut egui::Ui) {
        ui.heading(egui::RichText::new("Playback Workspace").color(accent_blue()));
        ui.add_space(4.0);
        ui.horizontal_wrapped(|ui| {
            metric_card(
                ui,
                "State",
                if self.playback.playing { "Playing" } else { "Paused" },
                self.status.as_str(),
                if self.playback.playing {
                    accent_green()
                } else {
                    accent_amber()
                },
            );
            metric_card(
                ui,
                "Position",
                &format!("{} ms", self.playback.position_ms),
                "Current timeline position",
                accent_blue(),
            );
            metric_card(
                ui,
                "Duration",
                &format!("{} ms", self.playback.duration_ms),
                "Loaded media duration",
                accent_blue(),
            );
            metric_card(
                ui,
                "Skip Step",
                &format!("{} ms", self.playback.skip_ms),
                "Transport seek increment",
                accent_violet(),
            );
        });

        ui.add_space(8.0);

        styled_section(ui, "Timeline", |ui| {
            ui.label(
                egui::RichText::new("Use the transport controls above or drag the seek bar.")
                    .color(muted_text()),
            );
            ui.add(
                egui::Slider::new(
                    &mut self.playback.position_ms,
                    0..=self.playback.duration_ms.max(1),
                )
                .text("Seek (ms)"),
            );
        });

        ui.add_space(8.0);

        ui.columns(2, |columns| {
            styled_section(&mut columns[0], "Media Summary", |ui| {
                if let Some(media) = &self.current_media {
                    ui.label(egui::RichText::new(&media.summary).strong());
                    key_value_row(ui, "Format", &media.format_name);
                    key_value_row(
                        ui,
                        "Duration",
                        &media
                            .duration_ms
                            .map(|ms| format!("{ms} ms"))
                            .unwrap_or_else(|| "unknown".to_string()),
                    );
                    key_value_row(ui, "Tracks", &media.tracks.len().to_string());
                } else {
                    ui.label("Open media to populate metadata and playback state.");
                }
            });

            styled_section(&mut columns[1], "Debug / Queue", |ui| {
                ui.label(
                    egui::RichText::new("Player runtime diagnostics").color(muted_text()),
                );
                ui.label(&self.playback.debug_stats);
            });
        });
    }

    fn render_editor_workspace(&mut self, ui: &mut egui::Ui) {
        ui.heading(egui::RichText::new("Editor Workspace").color(accent_violet()));
        if !self.license_snapshot.allows_editor() {
            styled_section(ui, "Editor Access", |ui| {
                ui.label("Editor mode unavailable. Save a valid key and refresh verification.");
            });
            return;
        }

        ui.add_space(4.0);
        ui.horizontal_wrapped(|ui| {
            if workspace_tab_button(ui, self.editor.active_tab == EditorTab::Pipeline, "Pipeline").clicked() {
                self.editor.active_tab = EditorTab::Pipeline;
            }
            if workspace_tab_button(ui, self.editor.active_tab == EditorTab::Timeline, "Timeline").clicked() {
                self.editor.active_tab = EditorTab::Timeline;
            }
            if workspace_tab_button(ui, self.editor.active_tab == EditorTab::FrameTools, "Frame Tools").clicked() {
                self.editor.active_tab = EditorTab::FrameTools;
            }
        });
        ui.separator();

        match self.editor.active_tab {
            EditorTab::Pipeline => self.render_pipeline_tab(ui),
            EditorTab::Timeline => self.render_timeline_tab(ui),
            EditorTab::FrameTools => self.render_frame_tools_tab(ui),
        }

        ui.separator();
        styled_section(ui, "Result", |ui| {
            ui.label(format!("Last editor result: {}", self.editor.last_result));
        });
    }

    fn render_pipeline_tab(&mut self, ui: &mut egui::Ui) {
        styled_section(ui, "Output Target", |ui| {
            ui.label(
                egui::RichText::new(
                    "Use the shared application services to run editor-capable workflows.",
                )
                .color(muted_text()),
            );
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new("Output").strong().color(muted_text()));
                ui.add_sized(
                    [ui.available_width() - 130.0, 30.0],
                    egui::TextEdit::singleline(&mut self.output_path),
                );
                if secondary_button(ui, "Browse").clicked() {
                    if let Some(path) = FileDialog::new().save_file() {
                        self.output_path = path.display().to_string();
                    }
                }
            });
        });

        ui.add_space(8.0);

        styled_section(ui, "Pipeline Actions", |ui| {
            ui.horizontal_wrapped(|ui| {
                if accent_button(ui, "Encode", accent_blue()).clicked() {
                    self.run_editor_action(EditorAction::Encode);
                }
                if accent_button(ui, "Decode", accent_blue()).clicked() {
                    self.run_editor_action(EditorAction::Decode);
                }
                if accent_button(ui, "Mux", accent_green()).clicked() {
                    self.run_editor_action(EditorAction::Mux);
                }
                if accent_button(ui, "Demux", accent_green()).clicked() {
                    self.run_editor_action(EditorAction::Demux);
                }
                if accent_button(ui, "Import", accent_violet()).clicked() {
                    self.run_editor_action(EditorAction::Import);
                }
                if accent_button(ui, "Transcode", accent_violet()).clicked() {
                    self.run_editor_action(EditorAction::Transcode);
                }
                if accent_button(ui, "Compress", accent_amber()).clicked() {
                    self.run_editor_action(EditorAction::Compress);
                }
            });
        });
    }

    fn render_timeline_tab(&mut self, ui: &mut egui::Ui) {
        let duration = self.playback.duration_ms.max(1);
        styled_section(ui, "Timeline Selection", |ui| {
            ui.label(
                egui::RichText::new("Selection and frame-scoped editing scaffolding.")
                    .color(muted_text()),
            );
            ui.add(
                egui::Slider::new(&mut self.editor.selection_start_ms, 0..=duration)
                    .text("Selection Start (ms)"),
            );
            ui.add(
                egui::Slider::new(&mut self.editor.selection_end_ms, 0..=duration)
                    .text("Selection End (ms)"),
            );
            if self.editor.selection_end_ms < self.editor.selection_start_ms {
                self.editor.selection_end_ms = self.editor.selection_start_ms;
            }
            ui.horizontal_wrapped(|ui| {
                if secondary_button(ui, "Jump To Start").clicked() {
                    self.playback.position_ms = self.editor.selection_start_ms;
                }
                if secondary_button(ui, "Jump To End").clicked() {
                    self.playback.position_ms = self.editor.selection_end_ms;
                }
                if accent_button(ui, "Set To Current Position", accent_violet()).clicked() {
                    self.editor.selection_start_ms = self.playback.position_ms;
                    self.editor.selection_end_ms = self.playback.position_ms;
                    self.editor.last_result = "Selection collapsed to current position.".to_string();
                }
            });
        });
    }

    fn render_frame_tools_tab(&mut self, ui: &mut egui::Ui) {
        ui.horizontal_wrapped(|ui| {
            metric_card(
                ui,
                "Frame Cursor",
                &format!("{} ms", self.editor.frame_cursor_ms),
                "Current frame-edit mark",
                accent_violet(),
            );
            metric_card(
                ui,
                "Selection Start",
                &format!("{} ms", self.editor.selection_start_ms),
                "Current region begin",
                accent_blue(),
            );
            metric_card(
                ui,
                "Selection End",
                &format!("{} ms", self.editor.selection_end_ms),
                "Current region end",
                accent_blue(),
            );
        });

        ui.add_space(8.0);

        styled_section(ui, "Frame Tools", |ui| {
            ui.label(
                egui::RichText::new(
                    "Frame-step and frame-edit scaffolding. Native render/edit backends can plug in later.",
                )
                .color(muted_text()),
            );
            ui.horizontal_wrapped(|ui| {
                if secondary_button(ui, "Prev Frame").clicked() {
                    self.skip_by(-40);
                    self.editor.last_result =
                        format!("Moved to previous frame at {} ms.", self.playback.position_ms);
                }
                if secondary_button(ui, "Next Frame").clicked() {
                    self.skip_by(40);
                    self.editor.last_result =
                        format!("Moved to next frame at {} ms.", self.playback.position_ms);
                }
                if accent_button(ui, "Mark Frame For Edit", accent_violet()).clicked() {
                    self.editor.frame_cursor_ms = self.playback.position_ms;
                    self.editor.last_result = format!(
                        "Frame edit scaffold marked at {} ms.",
                        self.editor.frame_cursor_ms
                    );
                }
            });
        });
    }

    fn render_notification_toasts(&mut self, ctx: &egui::Context) {
        let active = self
            .notifications
            .iter()
            .filter(|note| note.is_active())
            .cloned()
            .collect::<Vec<_>>();

        if active.is_empty() {
            return;
        }

        let next_expiry = active
            .iter()
            .map(|note| note.expires_at.saturating_duration_since(Instant::now()))
            .min()
            .unwrap_or(Duration::from_millis(250));
        ctx.request_repaint_after(next_expiry.max(Duration::from_millis(100)));

        egui::Area::new("notification_toasts".into())
            .anchor(egui::Align2::RIGHT_TOP, egui::vec2(-16.0, 16.0))
            .show(ctx, |ui| {
                ui.set_width(340.0);
                for note in active.iter().rev().take(4) {
                    egui::Frame::group(ui.style())
                        .fill(panel_fill_alt())
                        .stroke(egui::Stroke::new(1.0, accent_blue()))
                        .inner_margin(egui::Margin::same(10))
                        .corner_radius(egui::CornerRadius::same(8))
                        .show(ui, |ui| {
                            ui.label(
                                egui::RichText::new(&note.message)
                                    .strong()
                                    .color(egui::Color32::WHITE),
                            );
                            ui.label(
                                egui::RichText::new("This popup will close automatically.")
                                    .size(11.0)
                                    .color(muted_text()),
                            );
                        });
                    ui.add_space(8.0);
                }
            });
    }
}

impl eframe::App for PlayerApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.update_playback_tick(ctx);
        self.maybe_auto_refresh_license(ctx);
        self.render_top_bar(ctx);
        self.render_side_panel(ctx);
        self.render_workspace(ctx);
        self.render_license_popup(ctx);
        self.render_notification_toasts(ctx);
    }
}

#[derive(Debug, Clone, Copy)]
enum EditorAction {
    Encode,
    Decode,
    Mux,
    Demux,
    Import,
    Transcode,
    Compress,
}

impl EditorAction {
    fn label(self) -> &'static str {
        match self {
            Self::Encode => "Encode",
            Self::Decode => "Decode",
            Self::Mux => "Mux",
            Self::Demux => "Demux",
            Self::Import => "Import",
            Self::Transcode => "Transcode",
            Self::Compress => "Compress",
        }
    }
}

fn suggest_output_path(input: &str) -> String {
    let path = Path::new(input);
    path.with_extension("srsm").display().to_string()
}

fn missing_snapshot(message: String) -> LicenseSnapshot {
    LicenseSnapshot {
        current_key: None,
        claims: None,
        verification_state: VerificationState::MissingKey,
        effective_mode: EffectiveMode::PlayOnly,
        endpoint: None,
        message,
        server_notifications: Vec::new(),
    }
}

fn seed_refresh_prng() -> u64 {
    runtime_refresh_entropy()
        .wrapping_mul(6364136223846793005)
        .wrapping_add(1)
}

fn runtime_refresh_entropy() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64
}

fn next_refresh_interval_secs(state: &mut u64, entropy: u64) -> u64 {
    *state ^= entropy | 1;
    *state = state
        .wrapping_mul(6364136223846793005)
        .wrapping_add(1442695040888963407);
    (*state % 90) + 1
}

fn auto_refresh_repaint_delay(until_next: Duration) -> Duration {
    until_next.min(Duration::from_millis(250))
}

fn apply_player_theme(ctx: &egui::Context) {
    let mut visuals = egui::Visuals::dark();
    visuals.panel_fill = panel_fill();
    visuals.window_fill = panel_fill();
    visuals.extreme_bg_color = panel_fill_alt();
    visuals.override_text_color = Some(egui::Color32::from_rgb(230, 233, 240));
    visuals.widgets.inactive.bg_fill = egui::Color32::from_rgb(34, 38, 48);
    visuals.widgets.hovered.bg_fill = egui::Color32::from_rgb(44, 50, 63);
    visuals.widgets.active.bg_fill = egui::Color32::from_rgb(60, 72, 92);
    visuals.selection.bg_fill = accent_blue();
    ctx.set_visuals(visuals);

    let mut style = (*ctx.style()).clone();
    style.spacing.item_spacing = egui::vec2(10.0, 10.0);
    style.spacing.button_padding = egui::vec2(14.0, 8.0);
    style.spacing.indent = 14.0;
    ctx.set_style(style);
}

fn styled_section(
    ui: &mut egui::Ui,
    title: &str,
    add_contents: impl FnOnce(&mut egui::Ui),
) {
    egui::Frame::group(ui.style())
        .fill(panel_fill_alt())
        .stroke(egui::Stroke::new(1.0, panel_stroke()))
        .inner_margin(egui::Margin::same(12))
        .show(ui, |ui| {
            ui.label(
                egui::RichText::new(title)
                    .strong()
                    .size(16.0)
                    .color(accent_blue()),
            );
            ui.add_space(6.0);
            add_contents(ui);
        });
}

fn metric_card(
    ui: &mut egui::Ui,
    title: &str,
    value: &str,
    subtitle: &str,
    accent: egui::Color32,
) {
    egui::Frame::group(ui.style())
        .fill(panel_fill_alt())
        .stroke(egui::Stroke::new(1.0, panel_stroke()))
        .inner_margin(egui::Margin::same(12))
        .show(ui, |ui| {
            ui.set_min_width(150.0);
            ui.label(egui::RichText::new(title).strong().color(muted_text()));
            ui.label(egui::RichText::new(value).size(22.0).strong().color(accent));
            ui.label(egui::RichText::new(subtitle).color(muted_text()));
        });
}

fn key_value_row(ui: &mut egui::Ui, key: &str, value: &str) {
    ui.horizontal_wrapped(|ui| {
        ui.label(
            egui::RichText::new(format!("{key}:"))
                .strong()
                .color(muted_text()),
        );
        ui.label(value);
    });
}

fn badge(ui: &mut egui::Ui, text: &str, color: egui::Color32) {
    egui::Frame::group(ui.style())
        .fill(color.gamma_multiply(0.18))
        .stroke(egui::Stroke::new(1.0, color.gamma_multiply(0.7)))
        .inner_margin(egui::Margin::symmetric(8, 4))
        .show(ui, |ui| {
            ui.label(egui::RichText::new(text).strong().color(color));
        });
}

fn workspace_tab_button(ui: &mut egui::Ui, selected: bool, label: &str) -> egui::Response {
    ui.add_sized(
        [120.0, 32.0],
        egui::Button::new(
            egui::RichText::new(label)
                .strong()
                .color(if selected {
                    egui::Color32::BLACK
                } else {
                    egui::Color32::WHITE
                }),
        )
        .fill(if selected { accent_blue() } else { panel_fill_alt() }),
    )
}

fn primary_button(ui: &mut egui::Ui, label: &str) -> egui::Response {
    accent_button(ui, label, accent_blue())
}

fn secondary_button(ui: &mut egui::Ui, label: &str) -> egui::Response {
    ui.add_sized(
        [92.0, 30.0],
        egui::Button::new(egui::RichText::new(label).strong()).fill(panel_fill_alt()),
    )
}

fn destructive_button(ui: &mut egui::Ui, label: &str) -> egui::Response {
    accent_button(ui, label, egui::Color32::from_rgb(170, 70, 70))
}

fn accent_button(ui: &mut egui::Ui, label: &str, fill: egui::Color32) -> egui::Response {
    ui.add_sized(
        [110.0, 32.0],
        egui::Button::new(
            egui::RichText::new(label)
                .strong()
                .color(egui::Color32::BLACK),
        )
        .fill(fill),
    )
}

fn panel_fill() -> egui::Color32 {
    egui::Color32::from_rgb(20, 24, 30)
}

fn panel_fill_alt() -> egui::Color32 {
    egui::Color32::from_rgb(28, 33, 41)
}

fn panel_stroke() -> egui::Color32 {
    egui::Color32::from_rgb(52, 61, 76)
}

fn accent_blue() -> egui::Color32 {
    egui::Color32::from_rgb(102, 174, 255)
}

fn accent_green() -> egui::Color32 {
    egui::Color32::from_rgb(114, 212, 139)
}

fn accent_amber() -> egui::Color32 {
    egui::Color32::from_rgb(234, 191, 95)
}

fn accent_violet() -> egui::Color32 {
    egui::Color32::from_rgb(182, 132, 255)
}

fn muted_text() -> egui::Color32 {
    egui::Color32::from_rgb(168, 177, 193)
}

fn verification_color(state: VerificationState) -> egui::Color32 {
    match state {
        VerificationState::Verified => accent_green(),
        VerificationState::PendingConfirmation => accent_amber(),
        VerificationState::OfflineFallback => egui::Color32::from_rgb(255, 148, 102),
        VerificationState::MissingKey => muted_text(),
        VerificationState::Revoked => egui::Color32::from_rgb(220, 92, 92),
        VerificationState::ReplacementIssued => egui::Color32::from_rgb(255, 148, 102),
        VerificationState::InvalidResponse => egui::Color32::from_rgb(220, 92, 92),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use libsrs_licensing_proto::{EntitlementStatus, LicensedFeature};

    fn editor_snapshot() -> LicenseSnapshot {
        LicenseSnapshot {
            current_key: Some("key-1".to_string()),
            claims: Some(EntitlementClaims {
                license_id: "lic-1".to_string(),
                key_id: "key-1".to_string(),
                features: vec![LicensedFeature::Basic, LicensedFeature::EditorWorkspace],
                status: EntitlementStatus::Active,
                issued_at_epoch_s: 1,
                expires_at_epoch_s: 2,
                device_install_id: "install-1".to_string(),
                message: "verified".to_string(),
                replacement_key: None,
            }),
            verification_state: VerificationState::Verified,
            effective_mode: EffectiveMode::Editor,
            endpoint: Some("http://localhost:3000".to_string()),
            message: "verified".to_string(),
            server_notifications: Vec::new(),
        }
    }

    #[test]
    fn verified_editor_snapshot_switches_workspace() {
        let mut app = PlayerApp::fallback("test".to_string());
        app.apply_license_snapshot(editor_snapshot());
        assert_eq!(app.workspace, WorkspaceTab::Editor);
        assert_eq!(app.status, "Editor mode verified");
    }

    #[test]
    fn missing_snapshot_forces_play_only() {
        let mut app = PlayerApp::fallback("test".to_string());
        app.apply_license_snapshot(missing_snapshot("offline".to_string()));
        assert_eq!(app.workspace, WorkspaceTab::PlayOnly);
        assert_eq!(app.status, "Play-only mode");
    }

    #[test]
    fn auto_refresh_interval_stays_in_expected_bounds() {
        let mut state = 7_u64;
        let secs = next_refresh_interval_secs(&mut state, 11_u64);
        assert!((1..=90).contains(&secs));
    }

    #[test]
    fn schedule_next_auto_refresh_sets_future_deadline() {
        let mut app = PlayerApp::fallback("test".to_string());
        app.refresh_prng_state = 17;
        let start = Instant::now();
        app.schedule_next_auto_refresh();
        let wait = app.next_auto_refresh_at.duration_since(start);
        assert!(wait >= Duration::from_secs(1));
        assert!(wait <= Duration::from_secs(90));
    }

    #[test]
    fn manual_refresh_does_not_reschedule_auto_refresh_deadline() {
        let mut app = PlayerApp::fallback("test".to_string());
        let before = Instant::now() + Duration::from_secs(15);
        app.next_auto_refresh_at = before;
        app.refresh_license();
        assert_eq!(app.next_auto_refresh_at, before);
    }

    #[test]
    fn auto_refresh_repaint_delay_caps_sleep_for_countdown_updates() {
        assert_eq!(
            auto_refresh_repaint_delay(Duration::from_secs(10)),
            Duration::from_millis(250)
        );
        assert_eq!(
            auto_refresh_repaint_delay(Duration::from_millis(120)),
            Duration::from_millis(120)
        );
    }

    #[test]
    fn repeated_license_snapshot_does_not_duplicate_notification() {
        let mut app = PlayerApp::fallback("test".to_string());
        app.notifications.clear();
        let snapshot = editor_snapshot();
        app.apply_license_snapshot(snapshot.clone());
        app.apply_license_snapshot(snapshot);
        assert_eq!(app.notifications.len(), 1);
    }

    #[test]
    fn server_notification_is_displayed_once() {
        let mut app = PlayerApp::fallback("test".to_string());
        app.notifications.clear();
        let notification = ClientNotification {
            notification_id: "note-1".to_string(),
            subject: "Hello".to_string(),
            body: "World".to_string(),
            created_at_epoch_s: 1,
        };
        app.push_server_notification(&notification);
        app.push_server_notification(&notification);
        assert_eq!(app.notifications.len(), 1);
        assert_eq!(app.notifications[0].message, "Hello: World");
    }
}
