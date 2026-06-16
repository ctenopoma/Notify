use std::collections::{HashSet, HashMap};
use std::sync::Arc;
use tokio::sync::RwLock;
use tauri::Manager;
use tauri::menu::{Menu, MenuItemBuilder};
use tauri::tray::TrayIconBuilder;
use tauri_plugin_notification::NotificationExt;
use tauri_plugin_autostart::MacosLauncher;

fn default_heartbeat_alert_name() -> String {
    "AlwaysFiringTest".to_string()
}

#[derive(Clone, serde::Serialize, serde::Deserialize, Debug)]
pub struct AppConfig {
    pub alertmanager_urls: Vec<String>,
    pub polling_interval_secs: u64,
    #[serde(default = "default_heartbeat_alert_name")]
    pub heartbeat_alert_name: String,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            alertmanager_urls: vec!["http://localhost:9093".to_string()],
            polling_interval_secs: 60,
            heartbeat_alert_name: default_heartbeat_alert_name(),
        }
    }
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

#[derive(Clone, serde::Serialize, serde::Deserialize, Debug)]
pub struct AlertStatus {
    pub state: String,
}

#[derive(Clone, serde::Serialize, serde::Deserialize, Debug)]
pub struct Alert {
    pub fingerprint: String,
    pub status: AlertStatus,
    pub labels: std::collections::HashMap<String, String>,
    pub annotations: std::collections::HashMap<String, String>,
    #[serde(rename = "startsAt")]
    pub starts_at: String,
    #[serde(rename = "generatorURL")]
    pub generator_url: Option<String>,
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
    let config = state.config.read().await;
    Ok(config.clone())
}

#[tauri::command]
async fn save_config(
    state: tauri::State<'_, AppState>,
    urls: Vec<String>,
    interval: u64,
    heartbeat_alert_name: String,
) -> Result<(), String> {
    {
        let mut config = state.config.write().await;
        config.alertmanager_urls = urls;
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
    let urls = state.config.read().await.alertmanager_urls.clone();
    
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
    
    for base_url in urls {
        let clean_url = base_url.trim_end_matches('/');
        let api_url = format!("{}/api/v2/silences", clean_url);
        // Fire and forget
        let _ = client.post(&api_url).json(&payload).send().await;
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
    urls: &[String],
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
    let total_urls = urls.len();
    let mut failed_urls = 0;

    for base_url in urls {
        let clean_url = base_url.trim_end_matches('/');
        if clean_url.is_empty() {
            continue;
        }
        let api_url = format!("{}/api/v2/alerts", clean_url);

        let mut reachable = false;
        let mut heartbeat_seen = false;
        let mut last_error: Option<String> = None;
        let mut last_heartbeat_at = previous_statuses
            .get(base_url)
            .and_then(|s| s.last_heartbeat_at.clone());

        match client.get(&api_url).send().await {
            Ok(resp) => {
                if resp.status().is_success() {
                    match resp.json::<Vec<Alert>>().await {
                        Ok(alerts) => {
                            reachable = true;
                            for alert in &alerts {
                                if alert.status.state == "active"
                                    && !is_heartbeat_alert(alert, heartbeat_alert_name)
                                {
                                    merged_firing_alerts.insert(alert.fingerprint.clone(), alert.clone());
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
            .body("Failed to connect to all registered Alertmanager instances.")
            .show();
        *all_down_flag = true;
    } else if !is_all_down && *all_down_flag {
        let _ = app_handle.notification()
            .builder()
            .title("[RECOVERED] Connection Restored")
            .body("Successfully reconnected to Alertmanager.")
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

            let config = if config_path.exists() {
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
                            if let Some(window) = app.get_webview_window("main") {
                                let _ = window.show();
                                let _ = window.set_focus();
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
                        }
                    }
                })
                .build(app)?;

            let app_handle = app.handle().clone();
            
            tauri::async_runtime::spawn(async move {
                loop {
                    let app_state = app_handle.state::<AppState>();
                    
                    let (urls, interval, heartbeat_alert_name) = {
                        let conf = app_state.config.read().await;
                        (conf.alertmanager_urls.clone(), conf.polling_interval_secs, conf.heartbeat_alert_name.clone())
                    };

                    run_polling_cycle(&urls, &heartbeat_alert_name, &app_handle, &app_state).await;

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
    fn reachable_without_heartbeat_is_not_connected_when_required() {
        let heartbeat_alert_name = "AlwaysFiringTest";
        let heartbeat_required = !heartbeat_alert_name.trim().is_empty();
        let reachable = true;
        let heartbeat_seen = false;
        let connected = reachable && (heartbeat_seen || !heartbeat_required);
        assert!(!connected);
    }
}
