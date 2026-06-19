use std::collections::{HashSet, HashMap};
use std::sync::Arc;
use tokio::sync::RwLock;
use tauri::{Manager, Emitter};
use tauri::menu::{Menu, MenuItemBuilder};
use tauri::tray::TrayIconBuilder;
use tauri_plugin_notification::NotificationExt;
use tauri_plugin_autostart::MacosLauncher;

fn default_heartbeat_alert_name() -> String {
    "AlwaysFiringTest".to_string()
}

// Grafana's unified-alerting Alertmanager is reached under this prefix; appending
// `/api/v2/alerts` or `/api/v2/silences` yields the standard Alertmanager v2 API.
const GRAFANA_AM_PREFIX: &str = "/api/alertmanager/grafana";

fn alerts_endpoint(base_url: &str) -> String {
    format!("{}{}/api/v2/alerts", base_url.trim_end_matches('/'), GRAFANA_AM_PREFIX)
}

fn silences_endpoint(base_url: &str) -> String {
    format!("{}{}/api/v2/silences", base_url.trim_end_matches('/'), GRAFANA_AM_PREFIX)
}

// One monitored Grafana instance. Each server carries its OWN service-account
// token, because separate Grafana instances issue independent tokens — a single
// shared token could not authenticate against all of them.
#[derive(Clone, serde::Serialize, serde::Deserialize, Debug, Default)]
pub struct GrafanaServer {
    pub url: String,
    #[serde(default)]
    pub token: String,
}

#[derive(Clone, serde::Serialize, serde::Deserialize, Debug)]
pub struct AppConfig {
    #[serde(default)]
    pub servers: Vec<GrafanaServer>,
    pub polling_interval_secs: u64,
    #[serde(default = "default_heartbeat_alert_name")]
    pub heartbeat_alert_name: String,

    // --- Legacy fields, read only to migrate older config.json files. They are
    // never written back (skip_serializing); `normalize()` folds them into
    // `servers` on load. Covers both the Alertmanager era (alertmanager_urls)
    // and the single-global-token era (grafana_urls + grafana_token).
    #[serde(default, alias = "grafana_urls", alias = "alertmanager_urls", skip_serializing)]
    legacy_urls: Vec<String>,
    #[serde(default, rename = "grafana_token", skip_serializing)]
    legacy_token: String,
}

impl AppConfig {
    // Fold any legacy url/token fields into `servers` so the rest of the app only
    // ever deals with the per-server shape.
    fn normalize(&mut self) {
        if self.servers.is_empty() && !self.legacy_urls.is_empty() {
            self.servers = self
                .legacy_urls
                .drain(..)
                .map(|url| GrafanaServer { url, token: self.legacy_token.clone() })
                .collect();
        }
        self.legacy_urls.clear();
        self.legacy_token.clear();
    }
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            servers: vec![GrafanaServer {
                url: "http://localhost:3000".to_string(),
                token: String::new(),
            }],
            polling_interval_secs: 60,
            heartbeat_alert_name: default_heartbeat_alert_name(),
            legacy_urls: Vec::new(),
            legacy_token: String::new(),
        }
    }
}

// Read + normalize config.json from disk. Returns None (rather than a default)
// when the file is missing or mid-write/unparsable, so callers can keep the
// last-known-good config instead of momentarily snapping back to defaults.
fn read_config_file(path: &std::path::Path) -> Option<AppConfig> {
    let content = std::fs::read_to_string(path).ok()?;
    let mut cfg: AppConfig = serde_json::from_str(&content).ok()?;
    cfg.normalize();
    Some(cfg)
}

#[derive(Clone, serde::Serialize, serde::Deserialize, Debug)]
pub struct ServerStatus {
    pub url: String,
    pub reachable: bool,
    pub heartbeat_ok: bool,
    pub connected: bool,
    pub last_heartbeat_at: Option<String>,
    pub last_error: Option<String>,
    pub checked_at: String,
}

#[derive(Clone, serde::Serialize, serde::Deserialize, Debug, Default)]
pub struct AlertStatus {
    #[serde(default)]
    pub state: String,
}

#[derive(Clone, serde::Serialize, serde::Deserialize, Debug)]
pub struct Alert {
    pub fingerprint: String,
    // Alertmanager always sends these, but we default them so that a single
    // field omitted by an upstream (or a future schema tweak) can't make an
    // otherwise-valid alert undeserializable.
    #[serde(default)]
    pub status: AlertStatus,
    #[serde(default)]
    pub labels: std::collections::HashMap<String, String>,
    #[serde(default)]
    pub annotations: std::collections::HashMap<String, String>,
    #[serde(rename = "startsAt")]
    pub starts_at: String,
    #[serde(rename = "generatorURL")]
    pub generator_url: Option<String>,
    // Not part of the Alertmanager payload: we stamp this with the base URL of the
    // Grafana instance the alert came from, so the UI can build a link to that
    // instance's alert-rule detail page (the merge step otherwise loses which
    // server an alert originated from).
    #[serde(default, rename = "sourceURL")]
    pub source_url: Option<String>,
}

// Deserialize the /api/v2/alerts array element-by-element so that one
// non-conforming alert can't discard the entire response. Previously a single
// unexpected alert shape (e.g. a missing field) made `resp.json::<Vec<Alert>>()`
// fail wholesale, so even a perfectly valid heartbeat alert went unseen and the
// server was reported as "テスト未到達" despite the test alert being present.
fn parse_alerts_lenient(body: &str) -> Result<Vec<Alert>, serde_json::Error> {
    let raw: Vec<serde_json::Value> = serde_json::from_str(body)?;
    let alerts = raw
        .into_iter()
        .filter_map(|value| match serde_json::from_value::<Alert>(value) {
            Ok(alert) => Some(alert),
            Err(e) => {
                eprintln!("Skipping unparsable alert in response: {}", e);
                None
            }
        })
        .collect();
    Ok(alerts)
}

#[derive(serde::Serialize)]
#[allow(non_snake_case)]
struct SilenceMatcher {
    name: String,
    value: String,
    isRegex: bool,
    isEqual: bool,
}

#[derive(serde::Serialize)]
#[allow(non_snake_case)]
struct SilencePayload {
    matchers: Vec<SilenceMatcher>,
    startsAt: String,
    endsAt: String,
    createdBy: String,
    comment: String,
}

pub struct AppState {
    pub config: Arc<RwLock<AppConfig>>,
    pub config_path: std::path::PathBuf,
    pub notified_fingerprints: Arc<RwLock<HashSet<String>>>,
    pub active_alerts: Arc<RwLock<Vec<Alert>>>,
    pub config_changed_notify: Arc<tokio::sync::Notify>,
    pub connection_errors: Arc<RwLock<HashMap<String, String>>>,
    pub all_down_notified: Arc<RwLock<bool>>,
    pub server_statuses: Arc<RwLock<HashMap<String, ServerStatus>>>,
}

#[tauri::command]
async fn get_config(state: tauri::State<'_, AppState>) -> Result<AppConfig, String> {
    // Pick up any out-of-band edits to config.json when the settings screen opens.
    if let Some(fresh) = read_config_file(&state.config_path) {
        *state.config.write().await = fresh;
    }
    let config = state.config.read().await;
    Ok(config.clone())
}

#[tauri::command]
async fn save_config(
    state: tauri::State<'_, AppState>,
    servers: Vec<GrafanaServer>,
    interval: u64,
    heartbeat_alert_name: String,
) -> Result<(), String> {
    {
        let mut config = state.config.write().await;
        config.servers = servers;
        config.polling_interval_secs = interval;
        config.heartbeat_alert_name = heartbeat_alert_name;

        if let Ok(content) = serde_json::to_string_pretty(&*config) {
            if let Err(e) = std::fs::write(&state.config_path, content) {
                return Err(format!("Failed to save config file: {}", e));
            }
        }
    }
    state.config_changed_notify.notify_one();
    Ok(())
}

#[tauri::command]
async fn get_active_alerts(state: tauri::State<'_, AppState>) -> Result<Vec<Alert>, String> {
    let alerts = state.active_alerts.read().await;
    Ok(alerts.clone())
}

#[tauri::command]
async fn get_connection_errors(state: tauri::State<'_, AppState>) -> Result<HashMap<String, String>, String> {
    let errors = state.connection_errors.read().await;
    Ok(errors.clone())
}

#[tauri::command]
async fn get_server_statuses(state: tauri::State<'_, AppState>) -> Result<HashMap<String, ServerStatus>, String> {
    let statuses = state.server_statuses.read().await;
    Ok(statuses.clone())
}

#[tauri::command]
async fn trigger_poll_now(state: tauri::State<'_, AppState>) -> Result<(), String> {
    state.config_changed_notify.notify_one();
    Ok(())
}

#[tauri::command]
async fn open_config_dir(state: tauri::State<'_, AppState>, app_handle: tauri::AppHandle) -> Result<(), String> {
    use tauri_plugin_opener::OpenerExt;
    if let Some(parent) = state.config_path.parent() {
        app_handle.opener().reveal_item_in_dir(parent)
            .map_err(|e| format!("Failed to open config directory: {}", e))?;
    }
    Ok(())
}

#[tauri::command]
async fn create_silence(state: tauri::State<'_, AppState>, alertname: String, duration_hours: u64) -> Result<(), String> {
    let servers = state.config.read().await.servers.clone();

    let now = chrono::Utc::now();
    let ends_at = now + chrono::Duration::hours(duration_hours as i64);
    
    let payload = SilencePayload {
        matchers: vec![SilenceMatcher {
            name: "alertname".to_string(),
            value: alertname,
            isRegex: false,
            isEqual: true,
        }],
        startsAt: now.to_rfc3339(),
        endsAt: ends_at.to_rfc3339(),
        createdBy: "Notify App".to_string(),
        comment: "Silenced via Notify desktop app".to_string(),
    };

    let client = reqwest::Client::builder().no_proxy().build().map_err(|e| e.to_string())?;

    for server in servers {
        if server.url.trim().is_empty() {
            continue;
        }
        let mut req = client.post(silences_endpoint(&server.url)).json(&payload);
        if !server.token.trim().is_empty() {
            req = req.bearer_auth(&server.token);
        }
        // Fire and forget
        let _ = req.send().await;
    }
    
    // Trigger poll to fetch updated alerts
    state.config_changed_notify.notify_one();
    Ok(())
}

fn alerts_contain_firing_heartbeat(alerts: &[Alert], heartbeat_alert_name: &str) -> bool {
    alerts.iter().any(|alert| {
        alert.status.state == "active"
            && alert.labels.get("alertname").map(|s| s.as_str()) == Some(heartbeat_alert_name)
    })
}

// The heartbeat/test alert (e.g. "AlwaysFiringTest") exists purely to verify the
// delivery path. It must never be surfaced to the user as a real alert, neither in
// the alert list nor as a desktop notification.
fn is_heartbeat_alert(alert: &Alert, heartbeat_alert_name: &str) -> bool {
    !heartbeat_alert_name.trim().is_empty()
        && alert.labels.get("alertname").map(|s| s.as_str()) == Some(heartbeat_alert_name)
}

fn update_tray_icon(app_handle: &tauri::AppHandle, has_error: bool) {
    if let Some(tray) = app_handle.tray_by_id("main") {
        if has_error {
            // Generate a simple 16x16 red square as error icon
            let rgba = vec![255, 0, 0, 255].repeat(16 * 16);
            let img = tauri::image::Image::new(&rgba, 16, 16);
            let _ = tray.set_icon(Some(img));
        } else {
            // Restore default icon
            if let Some(default_icon) = app_handle.default_window_icon().cloned() {
                let _ = tray.set_icon(Some(default_icon));
            }
        }
    }
}

async fn run_polling_cycle(
    servers: &[GrafanaServer],
    heartbeat_alert_name: &str,
    app_handle: &tauri::AppHandle,
    state: &AppState,
) {
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(8))
        .no_proxy()
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Failed to build HTTP client: {}", e);
            return;
        }
    };

    let heartbeat_required = !heartbeat_alert_name.trim().is_empty();
    let previous_statuses = state.server_statuses.read().await.clone();
    let now_iso = chrono::Utc::now().to_rfc3339();

    let mut merged_firing_alerts = HashMap::new();
    let mut current_errors = HashMap::new();
    let mut new_statuses: HashMap<String, ServerStatus> = HashMap::new();
    let total_urls = servers.len();
    let mut failed_urls = 0;

    for server in servers {
        let base_url = &server.url;
        let clean_url = base_url.trim_end_matches('/');
        if clean_url.is_empty() {
            continue;
        }
        let api_url = alerts_endpoint(clean_url);

        let mut reachable = false;
        let mut heartbeat_seen = false;
        let mut last_error: Option<String> = None;
        let mut last_heartbeat_at = previous_statuses
            .get(base_url)
            .and_then(|s| s.last_heartbeat_at.clone());

        let mut request = client.get(&api_url);
        if !server.token.trim().is_empty() {
            request = request.bearer_auth(&server.token);
        }
        match request.send().await {
            Ok(resp) => {
                if resp.status().is_success() {
                    match resp.text().await.map_err(|e| e.to_string())
                        .and_then(|body| parse_alerts_lenient(&body).map_err(|e| e.to_string()))
                    {
                        Ok(alerts) => {
                            reachable = true;
                            for alert in &alerts {
                                if alert.status.state == "active"
                                    && !is_heartbeat_alert(alert, heartbeat_alert_name)
                                {
                                    let mut alert = alert.clone();
                                    alert.source_url = Some(clean_url.to_string());
                                    merged_firing_alerts.insert(alert.fingerprint.clone(), alert);
                                }
                            }
                            heartbeat_seen = alerts_contain_firing_heartbeat(&alerts, heartbeat_alert_name);
                            if heartbeat_seen {
                                last_heartbeat_at = Some(now_iso.clone());
                            }
                        }
                        Err(_) => {
                            failed_urls += 1;
                            last_error = Some("JSON Parsing Error".to_string());
                            current_errors.insert(base_url.clone(), "JSON Parsing Error".to_string());
                        }
                    }
                } else {
                    failed_urls += 1;
                    let msg = format!("HTTP Error: {}", resp.status());
                    last_error = Some(msg.clone());
                    current_errors.insert(base_url.clone(), msg);
                }
            }
            Err(e) => {
                failed_urls += 1;
                let msg = format!("Connection Error: {}", e);
                last_error = Some(msg.clone());
                current_errors.insert(base_url.clone(), msg);
            }
        }

        let connected = reachable && (heartbeat_seen || !heartbeat_required);

        new_statuses.insert(
            base_url.clone(),
            ServerStatus {
                url: base_url.clone(),
                reachable,
                heartbeat_ok: heartbeat_seen,
                connected,
                last_heartbeat_at,
                last_error,
                checked_at: now_iso.clone(),
            },
        );
    }

    {
        let mut err_state = state.connection_errors.write().await;
        *err_state = current_errors;
    }

    {
        let mut status_state = state.server_statuses.write().await;
        *status_state = new_statuses;
    }

    let is_all_down = total_urls > 0 && failed_urls == total_urls;
    let mut all_down_flag = state.all_down_notified.write().await;
    if is_all_down && !*all_down_flag {
        let _ = app_handle.notification()
            .builder()
            .title("[CRITICAL] Connection Lost")
            .body("Failed to connect to all registered Grafana instances.")
            .show();
        *all_down_flag = true;
    } else if !is_all_down && *all_down_flag {
        let _ = app_handle.notification()
            .builder()
            .title("[RECOVERED] Connection Restored")
            .body("Successfully reconnected to Grafana.")
            .show();
        *all_down_flag = false;
    }

    let current_firing_alerts: Vec<Alert> = merged_firing_alerts.into_values().collect();
    
    // Update tray icon status based on errors or critical alerts
    let has_critical = current_firing_alerts.iter().any(|a| a.labels.get("severity").map(|s| s.as_str()) == Some("critical"));
    let has_error = failed_urls > 0 || has_critical;
    update_tray_icon(app_handle, has_error);

    {
        let mut active_cache = state.active_alerts.write().await;
        let current_firing_fps: HashSet<String> = current_firing_alerts.iter().map(|a| a.fingerprint.clone()).collect();
        
        let mut notified = state.notified_fingerprints.write().await;

        for prev_alert in active_cache.iter() {
            if !current_firing_fps.contains(&prev_alert.fingerprint) && notified.contains(&prev_alert.fingerprint) {
                let alertname = prev_alert.labels.get("alertname").map(|s| s.as_str()).unwrap_or("Unknown Alert");
                let title = format!("[RESOLVED] {}", alertname);
                
                let _ = app_handle.notification()
                    .builder()
                    .title(title)
                    .body("The alert is no longer firing.")
                    .show();
            }
        }

        notified.retain(|fp| current_firing_fps.contains(fp));

        for alert in &current_firing_alerts {
            if !notified.contains(&alert.fingerprint) {
                let alertname = alert.labels.get("alertname").map(|s| s.as_str()).unwrap_or("Unknown Alert");
                let severity = alert.labels.get("severity").map(|s| s.as_str()).unwrap_or("warning");
                let summary = alert.annotations.get("summary")
                    .or_else(|| alert.annotations.get("description"))
                    .map(|s| s.as_str())
                    .unwrap_or("No description provided.");

                let title = format!("[{}] {}", severity.to_uppercase(), alertname);
                
                let _ = app_handle.notification()
                    .builder()
                    .title(title)
                    .body(summary.to_string())
                    .show();

                notified.insert(alert.fingerprint.clone());
            }
        }

        *active_cache = current_firing_alerts;
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_autostart::init(MacosLauncher::LaunchAgent, Some(vec!["--minimized"])))
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_notification::init())
        .setup(|app| {
            let config_dir = app.path().app_config_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
            let _ = std::fs::create_dir_all(&config_dir);
            let config_path = config_dir.join("config.json");

            let mut config = if config_path.exists() {
                if let Ok(file_content) = std::fs::read_to_string(&config_path) {
                    serde_json::from_str(&file_content).unwrap_or_default()
                } else {
                    AppConfig::default()
                }
            } else {
                let default_config = AppConfig::default();
                if let Ok(content) = serde_json::to_string_pretty(&default_config) {
                    let _ = std::fs::write(&config_path, content);
                }
                default_config
            };
            // Migrate any legacy alertmanager_urls / grafana_urls+grafana_token
            // shape into the per-server list before anything reads it.
            config.normalize();

            let state = AppState {
                config: Arc::new(RwLock::new(config)),
                config_path: config_path.clone(),
                notified_fingerprints: Arc::new(RwLock::new(HashSet::new())),
                active_alerts: Arc::new(RwLock::new(Vec::new())),
                config_changed_notify: Arc::new(tokio::sync::Notify::new()),
                connection_errors: Arc::new(RwLock::new(HashMap::new())),
                all_down_notified: Arc::new(RwLock::new(false)),
                server_statuses: Arc::new(RwLock::new(HashMap::new())),
            };

            app.manage(state);

            let show = MenuItemBuilder::with_id("show", "設定を開く").build(app)?;
            let quit = MenuItemBuilder::with_id("quit", "終了").build(app)?;
            let menu = Menu::with_items(app, &[&show, &quit])?;

            let default_icon = app.default_window_icon().cloned().ok_or_else(|| {
                tauri::Error::AssetNotFound("Default window icon not found".to_string())
            })?;

            let _tray = TrayIconBuilder::with_id("main")
                .icon(default_icon)
                .menu(&menu)
                .on_menu_event(|app: &tauri::AppHandle, event| {
                    match event.id.as_ref() {
                        "show" => {
                            // "設定を開く": surface the window on the settings tab.
                            if let Some(window) = app.get_webview_window("main") {
                                let _ = window.show();
                                let _ = window.set_focus();
                                let _ = window.emit("navigate-view", "settings-view");
                            }
                        }
                        "quit" => {
                            app.exit(0);
                        }
                        _ => {}
                    }
                })
                .on_tray_icon_event(|tray, event| {
                    if let tauri::tray::TrayIconEvent::Click {
                        button: tauri::tray::MouseButton::Left,
                        button_state: tauri::tray::MouseButtonState::Up,
                        ..
                    } = event {
                        let app = tray.app_handle();
                        if let Some(window) = app.get_webview_window("main") {
                            let _ = window.show();
                            let _ = window.set_focus();
                            // Quick-look: a left click jumps straight to the live alert list.
                            let _ = window.emit("navigate-view", "alerts-view");
                        }
                    }
                })
                .build(app)?;

            let app_handle = app.handle().clone();
            
            tauri::async_runtime::spawn(async move {
                loop {
                    let app_state = app_handle.state::<AppState>();

                    // Re-read config.json from disk each cycle so external edits
                    // (or a hand-fixed URL/token) take effect without a restart.
                    if let Some(fresh) = read_config_file(&app_state.config_path) {
                        *app_state.config.write().await = fresh;
                    }

                    let (servers, interval, heartbeat_alert_name) = {
                        let conf = app_state.config.read().await;
                        (conf.servers.clone(), conf.polling_interval_secs, conf.heartbeat_alert_name.clone())
                    };

                    run_polling_cycle(&servers, &heartbeat_alert_name, &app_handle, &app_state).await;

                    tokio::select! {
                        _ = tokio::time::sleep(std::time::Duration::from_secs(interval)) => {}
                        _ = app_state.config_changed_notify.notified() => {}
                    }
                }
            });

            Ok(())
        })
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = window.hide();
            }
        })
        .invoke_handler(tauri::generate_handler![
            get_config,
            save_config,
            get_active_alerts,
            get_connection_errors,
            get_server_statuses,
            trigger_poll_now,
            open_config_dir,
            create_silence
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn alert_with(alertname: &str, state: &str) -> Alert {
        let mut labels = HashMap::new();
        labels.insert("alertname".to_string(), alertname.to_string());
        Alert {
            fingerprint: format!("{}-{}", alertname, state),
            status: AlertStatus { state: state.to_string() },
            labels,
            annotations: HashMap::new(),
            starts_at: "2024-01-01T00:00:00Z".to_string(),
            generator_url: None,
            source_url: None,
        }
    }

    #[test]
    fn heartbeat_detected_when_firing_and_named_match() {
        let alerts = vec![
            alert_with("SomeOtherAlert", "active"),
            alert_with("AlwaysFiringTest", "active"),
        ];
        assert!(alerts_contain_firing_heartbeat(&alerts, "AlwaysFiringTest"));
    }

    #[test]
    fn heartbeat_not_detected_when_absent() {
        let alerts = vec![alert_with("SomeOtherAlert", "active")];
        assert!(!alerts_contain_firing_heartbeat(&alerts, "AlwaysFiringTest"));
    }

    #[test]
    fn heartbeat_not_detected_when_resolved() {
        // A test alert that is silenced/suppressed must not count as a live connection check.
        let alerts = vec![alert_with("AlwaysFiringTest", "suppressed")];
        assert!(!alerts_contain_firing_heartbeat(&alerts, "AlwaysFiringTest"));
    }

    #[test]
    fn heartbeat_check_is_disabled_by_empty_name() {
        // Mirrors the `connected` computation in run_polling_cycle: an empty
        // heartbeat name means the operator opted out, so reachability alone
        // should be sufficient to mark a server as connected.
        let heartbeat_alert_name = "";
        let heartbeat_required = !heartbeat_alert_name.trim().is_empty();
        let reachable = true;
        let heartbeat_seen = false;
        let connected = reachable && (heartbeat_seen || !heartbeat_required);
        assert!(connected);
    }

    #[test]
    fn heartbeat_alert_is_identified_for_exclusion() {
        let heartbeat = alert_with("AlwaysFiringTest", "active");
        assert!(is_heartbeat_alert(&heartbeat, "AlwaysFiringTest"));
    }

    #[test]
    fn non_heartbeat_alert_is_not_excluded() {
        let real = alert_with("InstanceDown", "active");
        assert!(!is_heartbeat_alert(&real, "AlwaysFiringTest"));
    }

    #[test]
    fn nothing_is_excluded_when_heartbeat_name_is_empty() {
        // With the heartbeat feature disabled, no alert should be treated as a probe.
        let probe = alert_with("AlwaysFiringTest", "active");
        assert!(!is_heartbeat_alert(&probe, ""));
    }

    #[test]
    fn lenient_parse_keeps_valid_alerts_when_one_is_malformed() {
        // The heartbeat is well-formed; a sibling alert carries a non-string
        // label value. The old `Vec<Alert>` parse would reject the whole batch,
        // hiding the heartbeat. Lenient parsing must keep the good alert.
        let body = r#"[
            {
                "fingerprint": "good",
                "status": {"state": "active"},
                "labels": {"alertname": "AlwaysFiringTest", "severity": "warning"},
                "annotations": {"summary": "ok"},
                "startsAt": "2024-01-01T00:00:00Z"
            },
            {
                "fingerprint": "bad",
                "status": {"state": "active"},
                "labels": {"alertname": "Weird", "port": 9093},
                "annotations": {},
                "startsAt": "2024-01-01T00:00:00Z"
            }
        ]"#;
        let alerts = parse_alerts_lenient(body).expect("array should parse");
        assert_eq!(alerts.len(), 1);
        assert!(alerts_contain_firing_heartbeat(&alerts, "AlwaysFiringTest"));
    }

    #[test]
    fn lenient_parse_tolerates_missing_annotations() {
        // An alert without an `annotations` field must still deserialize.
        let body = r#"[
            {
                "fingerprint": "noann",
                "status": {"state": "active"},
                "labels": {"alertname": "AlwaysFiringTest"},
                "startsAt": "2024-01-01T00:00:00Z"
            }
        ]"#;
        let alerts = parse_alerts_lenient(body).expect("array should parse");
        assert_eq!(alerts.len(), 1);
        assert!(alerts[0].annotations.is_empty());
        assert!(alerts_contain_firing_heartbeat(&alerts, "AlwaysFiringTest"));
    }

    #[test]
    fn lenient_parse_errors_only_when_not_an_array() {
        // A non-array body is a genuine protocol error and must still surface.
        assert!(parse_alerts_lenient("{\"not\":\"an array\"}").is_err());
    }

    #[test]
    fn reachable_without_heartbeat_is_not_connected_when_required() {
        let heartbeat_alert_name = "AlwaysFiringTest";
        let heartbeat_required = !heartbeat_alert_name.trim().is_empty();
        let reachable = true;
        let heartbeat_seen = false;
        let connected = reachable && (heartbeat_seen || !heartbeat_required);
        assert!(!connected);
    }
}
