use std::collections::{HashSet, HashMap};
use std::sync::Arc;
use tokio::sync::RwLock;
use tauri::Manager;
use tauri::menu::{Menu, MenuItemBuilder};
use tauri::tray::TrayIconBuilder;
use tauri_plugin_notification::NotificationExt;

#[derive(Clone, serde::Serialize, serde::Deserialize, Debug)]
pub struct AppConfig {
    pub alertmanager_urls: Vec<String>,
    pub polling_interval_secs: u64,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            alertmanager_urls: vec!["http://localhost:9093".to_string()],
            polling_interval_secs: 60,
        }
    }
}

#[derive(Clone, serde::Serialize, serde::Deserialize, Debug)]
pub struct AlertStatus {
    pub state: String, // "firing", "suppressed", "unprocessed"
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

pub struct AppState {
    pub config: Arc<RwLock<AppConfig>>,
    pub config_path: std::path::PathBuf,
    pub notified_fingerprints: Arc<RwLock<HashSet<String>>>,
    pub active_alerts: Arc<RwLock<Vec<Alert>>>,
    pub config_changed_notify: Arc<tokio::sync::Notify>,
    pub connection_errors: Arc<RwLock<HashMap<String, String>>>,
    pub all_down_notified: Arc<RwLock<bool>>,
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
) -> Result<(), String> {
    {
        let mut config = state.config.write().await;
        config.alertmanager_urls = urls;
        config.polling_interval_secs = interval;

        // Save configuration file
        if let Ok(content) = serde_json::to_string_pretty(&*config) {
            if let Err(e) = std::fs::write(&state.config_path, content) {
                return Err(format!("Failed to save config file: {}", e));
            }
        }
    }
    // Interrupt the sleep and trigger immediate poll
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
async fn trigger_poll_now(state: tauri::State<'_, AppState>) -> Result<(), String> {
    // Notify the background task to wake up immediately
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

async fn run_polling_cycle(
    urls: &[String],
    app_handle: &tauri::AppHandle,
    state: &AppState,
) {
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(8))
        .no_proxy() // Important: By-pass system proxy
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Failed to build HTTP client: {}", e);
            return;
        }
    };

    let mut merged_firing_alerts = HashMap::new();
    let mut current_errors = HashMap::new();
    let total_urls = urls.len();
    let mut failed_urls = 0;

    for base_url in urls {
        let clean_url = base_url.trim_end_matches('/');
        if clean_url.is_empty() {
            continue;
        }
        let api_url = format!("{}/api/v2/alerts", clean_url);

        match client.get(&api_url).send().await {
            Ok(resp) => {
                if resp.status().is_success() {
                    if let Ok(alerts) = resp.json::<Vec<Alert>>().await {
                        for alert in alerts {
                            if alert.status.state == "firing" {
                                merged_firing_alerts.insert(alert.fingerprint.clone(), alert);
                            }
                        }
                    } else {
                        failed_urls += 1;
                        current_errors.insert(base_url.clone(), "JSON Parsing Error".to_string());
                    }
                } else {
                    failed_urls += 1;
                    current_errors.insert(base_url.clone(), format!("HTTP Error: {}", resp.status()));
                }
            }
            Err(e) => {
                failed_urls += 1;
                current_errors.insert(base_url.clone(), format!("Connection Error: {}", e));
            }
        }
    }

    // Update connection errors state
    {
        let mut err_state = state.connection_errors.write().await;
        *err_state = current_errors;
    }

    // Check all down logic
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

    // Identify resolved alerts and process new firing notifications
    {
        let mut active_cache = state.active_alerts.write().await;
        let current_firing_fps: HashSet<String> = current_firing_alerts.iter().map(|a| a.fingerprint.clone()).collect();
        
        let mut notified = state.notified_fingerprints.write().await;

        // Process newly resolved alerts
        for prev_alert in active_cache.iter() {
            if !current_firing_fps.contains(&prev_alert.fingerprint) && notified.contains(&prev_alert.fingerprint) {
                // It was firing and notified, but now it's gone -> Resolved!
                let alertname = prev_alert.labels.get("alertname").map(|s| s.as_str()).unwrap_or("Unknown Alert");
                let title = format!("[RESOLVED] {}", alertname);
                
                let _ = app_handle.notification()
                    .builder()
                    .title(title)
                    .body("The alert is no longer firing.")
                    .show();
            }
        }

        // Clean up resolved alerts from notified history
        notified.retain(|fp| current_firing_fps.contains(fp));

        // Process newly firing alerts
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

        // Update active cache
        *active_cache = current_firing_alerts;
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
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
            };

            app.manage(state);

            let show = MenuItemBuilder::with_id("show", "設定を開く").build(app)?;
            let quit = MenuItemBuilder::with_id("quit", "終了").build(app)?;
            let menu = Menu::with_items(app, &[&show, &quit])?;

            let default_icon = app.default_window_icon().cloned().ok_or_else(|| {
                tauri::Error::AssetNotFound("Default window icon not found".to_string())
            })?;

            let _tray = TrayIconBuilder::new()
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
                    
                    let (urls, interval) = {
                        let conf = app_state.config.read().await;
                        (conf.alertmanager_urls.clone(), conf.polling_interval_secs)
                    };

                    run_polling_cycle(&urls, &app_handle, &app_state).await;

                    // Sleep for 'interval' seconds, but wake up immediately if config changes
                    tokio::select! {
                        _ = tokio::time::sleep(std::time::Duration::from_secs(interval)) => {}
                        _ = app_state.config_changed_notify.notified() => {
                            // Config changed or immediate poll requested, loop will instantly restart
                        }
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
            trigger_poll_now,
            open_config_dir
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
