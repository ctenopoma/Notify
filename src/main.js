const { invoke } = window.__TAURI__.core;

// DOM Elements
let tabAlertsBtn;
let tabSettingsBtn;
let alertsView;
let settingsView;
let alertsList;
let emptyState;
let alertCountBadge;
let syncBtn;
let statusIndicator;
let statusText;
let lastSyncText;
let urlListContainer;
let addUrlBtn;
let inputInterval;
let intervalVal;
let inputHeartbeatName;
let saveBtn;
let toast;
let toastMsg;

// App State (Frontend Cache)
let pollingTimer = null;
let currentInterval = 60;
let manualStatusUntil = 0; // suppress auto status overwrite while a manual sync message is shown
let lastAlertsSignature = null; // skip rebuilding the list when nothing changed (prevents flicker)

// Show temporary toast notification
function showToast(message, duration = 3000) {
  toastMsg.textContent = message;
  toast.classList.remove('hide');
  setTimeout(() => {
    toast.classList.add('hide');
  }, duration);
}

// Switch between Tabs
function switchTab(targetTabId) {
  document.querySelectorAll('.nav-tab').forEach(tab => {
    if (tab.dataset.tab === targetTabId) {
      tab.classList.add('active');
    } else {
      tab.classList.remove('active');
    }
  });

  document.querySelectorAll('.tab-view').forEach(view => {
    if (view.id === targetTabId) {
      view.classList.add('active');
    } else {
      view.classList.remove('active');
    }
  });
}

// Convert ISO string to relative time
function fnFormatRelativeTime(isoString) {
  try {
    const alertTime = new Date(isoString);
    const now = new Date();
    const diffMs = now - alertTime;
    const diffSec = Math.floor(diffMs / 1000);
    const diffMin = Math.floor(diffSec / 60);
    const diffHour = Math.floor(diffMin / 60);

    if (diffSec < 60) return 'たった今';
    if (diffMin < 60) return `${diffMin}分前`;
    if (diffHour < 24) return `${diffHour}時間前`;
    
    return alertTime.toLocaleString('ja-JP', { 
      month: 'numeric', day: 'numeric', hour: '2-digit', minute: '2-digit' 
    });
  } catch (e) {
    return '時刻不明';
  }
}

// Render active alerts list with Sorting
function renderAlerts(alerts) {
  // Sorting: Critical -> Warning -> Info, then by newest.
  // Work on a copy so the signature reflects the displayed order.
  const severityWeight = { critical: 3, warning: 2, info: 1 };
  const list = (alerts || []).slice().sort((a, b) => {
    const sevA = severityWeight[(a.labels.severity || '').toLowerCase()] || 0;
    const sevB = severityWeight[(b.labels.severity || '').toLowerCase()] || 0;
    if (sevA !== sevB) return sevB - sevA; // Descending weight
    return new Date(b.startsAt) - new Date(a.startsAt); // Newest first
  });

  // Compute a signature of the meaningful content. If it hasn't changed since
  // the last render, skip rebuilding the DOM entirely: a full innerHTML rebuild
  // every poll re-triggered each card's slideIn animation, which showed up as
  // flicker and as alerts appearing to "re-display" repeatedly.
  const signature = JSON.stringify(list.map(a => [
    a.fingerprint,
    (a.labels.severity || '').toLowerCase(),
    a.startsAt,
    a.annotations.summary || a.annotations.description || ''
  ]));

  if (signature === lastAlertsSignature) {
    // Nothing structurally changed; just keep the relative timestamps fresh
    // in place (no rebuild, so no animation replay).
    alertsList.querySelectorAll('.alert-time[data-starts-at]').forEach(el => {
      el.textContent = fnFormatRelativeTime(el.getAttribute('data-starts-at'));
    });
    return;
  }
  lastAlertsSignature = signature;

  alertsList.innerHTML = '';

  if (list.length === 0) {
    emptyState.classList.remove('hide');
    alertsList.classList.add('hide');
    alertCountBadge.classList.add('hide');
    alertCountBadge.textContent = '0';
    return;
  }

  emptyState.classList.add('hide');
  alertsList.classList.remove('hide');

  alertCountBadge.classList.remove('hide');
  alertCountBadge.textContent = list.length;

  list.forEach(alert => {
    const card = document.createElement('div');
    const severity = (alert.labels.severity || 'warning').toLowerCase();
    card.className = `alert-card severity-${severity}`;

    const alertname = alert.labels.alertname || 'Unknown Alert';
    const summary = alert.annotations.summary || alert.annotations.description || '詳細情報はありません。';
    const relativeTime = fnFormatRelativeTime(alert.startsAt);
    
    // Prefer the Grafana alert-rule detail page (a recognizable screen that
    // shows what this alert is) over the raw generatorURL, which points at the
    // underlying datasource query and is confusing. Grafana stamps each managed
    // alert with the `__alert_rule_uid__` label; combined with the source server
    // URL we can link straight to /alerting/grafana/<uid>/view. Fall back to the
    // generatorURL when either piece is missing.
    const ruleUid = alert.labels.__alert_rule_uid__;
    let sourceLink = '';
    if (alert.sourceURL && ruleUid) {
      sourceLink = `${alert.sourceURL.replace(/\/+$/, '')}/alerting/grafana/${encodeURIComponent(ruleUid)}/view`;
    } else if (alert.generatorURL) {
      sourceLink = alert.generatorURL;
    }

    let actionBtnHtml = '';
    if (sourceLink) {
      actionBtnHtml = `
        <a href="${sourceLink}" target="_blank" class="alert-action-btn">
          ソース表示
          <svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.5">
            <path d="M18 13v6a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V8a2 2 0 0 1 2-2h6M15 3h6v6M10 14L21 3"/>
          </svg>
        </a>
      `;
    }

    actionBtnHtml += `
      <button type="button" class="btn outline-btn small-btn mute-btn" data-alertname="${alertname}" title="1時間ミュートする">
        🔕 ミュート
      </button>
    `;

    let labelPillsHtml = '';
    Object.entries(alert.labels).forEach(([key, val]) => {
      // Skip alertname/severity (shown elsewhere) and Grafana's internal
      // __double_underscore__ labels (e.g. __alert_rule_uid__), which are noise.
      if (key !== 'alertname' && key !== 'severity' && !key.startsWith('__')) {
        labelPillsHtml += `<span class="label-pill">${key}=${val}</span>`;
      }
    });

    card.innerHTML = `
      <div class="alert-card-header">
        <div class="alert-title-container">
          <span class="alert-name">${alertname}</span>
          <span class="severity-badge">${severity}</span>
        </div>
        <span class="alert-time" data-starts-at="${alert.startsAt}">${relativeTime}</span>
      </div>
      <div class="alert-body">${summary}</div>
      <div class="alert-footer">
        <div class="alert-labels">
          ${labelPillsHtml}
        </div>
        ${actionBtnHtml}
      </div>
    `;

    alertsList.appendChild(card);
  });

  // Bind mute button events
  document.querySelectorAll('.mute-btn').forEach(btn => {
    btn.addEventListener('click', async (e) => {
      const alertname = e.currentTarget.getAttribute('data-alertname');
      e.currentTarget.disabled = true;
      try {
        await invoke('create_silence', { alertname: alertname, durationHours: 1 });
        showToast(`「${alertname}」を1時間ミュートしました`);
        setTimeout(fetchAlerts, 1000);
      } catch (err) {
        console.error(err);
        showToast('ミュート設定に失敗しました');
        e.currentTarget.disabled = false;
      }
    });
  });
}

// Render connection errors UI
function renderConnectionErrors(errors) {
  const headerStatus = document.querySelector('.header-status');
  // Guard against a missing container so a render error here can never abort the
  // rest of the status refresh (server badges, overall status, etc.).
  if (!headerStatus) return;
  // Remove existing error badge
  const existingBadge = headerStatus.querySelector('.error-badge');
  if (existingBadge) existingBadge.remove();
  
  const existingTooltip = headerStatus.querySelector('.error-tooltip');
  if (existingTooltip) existingTooltip.remove();

  const errorUrls = Object.keys(errors);
  if (errorUrls.length === 0) {
    return; // No errors
  }

  // Create Error Badge
  const badge = document.createElement('div');
  badge.className = 'error-badge';
  badge.innerHTML = `
    <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2">
      <circle cx="12" cy="12" r="10"></circle>
      <line x1="12" y1="8" x2="12" y2="12"></line>
      <line x1="12" y1="16" x2="12.01" y2="16"></line>
    </svg>
    ${errorUrls.length} Errors
  `;

  // Create Tooltip
  const tooltip = document.createElement('div');
  tooltip.className = 'error-tooltip';
  let tooltipHtml = '';
  errorUrls.forEach(url => {
    tooltipHtml += `<div class="error-tooltip-item"><strong>${url}</strong><br>${errors[url]}</div>`;
  });
  tooltip.innerHTML = tooltipHtml;

  headerStatus.appendChild(badge);
  headerStatus.appendChild(tooltip);
}

// Update Status Badge UI
function setStatus(state, text) {
  statusIndicator.className = `status-badge state-${state}`;
  statusText.textContent = text;
}

// Render per-server connection badges in the settings screen,
// based on whether the heartbeat (test) alert was actually received.
function renderServerStatuses(statuses) {
  const rows = urlListContainer.querySelectorAll('.url-input-row');
  rows.forEach(row => {
    const input = row.querySelector('.alertmanager-url-input');
    const badge = row.querySelector('.server-status-badge');
    if (!input || !badge) return;

    const url = input.value.trim();
    const status = statuses[url];

    if (!status) {
      badge.className = 'server-status-badge unknown';
      badge.textContent = '未確認';
      badge.title = '設定を保存すると接続状態を確認します';
      return;
    }

    if (status.connected) {
      badge.className = 'server-status-badge connected';
      badge.textContent = '接続中';
      const seen = status.last_heartbeat_at ? fnFormatRelativeTime(status.last_heartbeat_at) : 'たった今';
      badge.title = `テストアラートを受信しました（${seen}）`;
    } else if (status.reachable) {
      badge.className = 'server-status-badge degraded';
      badge.textContent = 'テスト未到達';
      const lastSeen = status.last_heartbeat_at
        ? `最終受信: ${fnFormatRelativeTime(status.last_heartbeat_at)}`
        : '受信履歴なし';
      badge.title = `APIへの接続は成功していますが、テストアラートが届いていません（${lastSeen}）。Grafanaのアラートルール設定を確認してください。`;
    } else {
      badge.className = 'server-status-badge disconnected';
      badge.textContent = '未接続';
      badge.title = status.last_error || '接続エラー';
    }
  });
}

// Compute and reflect an aggregate connection state in the header badge,
// so "監視中" only shows once at least one server is verified connected.
function updateOverallStatus(statuses) {
  if (Date.now() < manualStatusUntil) return; // don't fight a just-triggered manual sync message

  const entries = Object.values(statuses || {});
  if (entries.length === 0) {
    setStatus('idle', '監視中');
    return;
  }

  const connectedCount = entries.filter(s => s.connected).length;

  if (connectedCount === entries.length) {
    setStatus('success', '監視中（接続確認済）');
  } else if (connectedCount > 0) {
    setStatus('warning', `一部未確認 (${connectedCount}/${entries.length})`);
  } else {
    setStatus('error', '未接続');
  }
}

// Poll alerts from Rust cache
async function fetchAlerts() {
  try {
    const alerts = await invoke('get_active_alerts');
    renderAlerts(alerts);
    lastSyncText.textContent = new Date().toLocaleTimeString('ja-JP');

    const errors = await invoke('get_connection_errors');
    renderConnectionErrors(errors);

    const statuses = await invoke('get_server_statuses');
    renderServerStatuses(statuses);
    updateOverallStatus(statuses);
  } catch (e) {
    console.error('Failed to get active alerts:', e);
  }
}

// Trigger immediate sync
async function syncNow() {
  manualStatusUntil = Date.now() + 1500;
  setStatus('polling', '同期中...');
  const syncIcon = syncBtn.querySelector('.sync-icon');
  syncIcon.classList.add('spinning');

  try {
    await invoke('trigger_poll_now');
    // Wait briefly for rust loop to fetch and update state
    setTimeout(() => {
      manualStatusUntil = 0;
      fetchAlerts();
      syncIcon.classList.remove('spinning');
    }, 1500);
  } catch (e) {
    console.error('Manual sync failed:', e);
    manualStatusUntil = Date.now() + 5000;
    setStatus('error', '同期エラー');
    showToast(`同期に失敗しました: ${e}`);
    setTimeout(() => { manualStatusUntil = 0; fetchAlerts(); }, 5000);
    syncIcon.classList.remove('spinning');
  }
}

// URL + token Inputs Management. Each row pairs a Grafana base URL with the
// service-account token issued by THAT Grafana, so multiple distinct servers
// can each authenticate with their own token.
function createUrlRow(url = '', token = '') {
  const row = document.createElement('div');
  row.className = 'url-input-row';

  const input = document.createElement('input');
  input.type = 'url';
  input.className = 'alertmanager-url-input';
  input.placeholder = 'http://192.168.1.100:3000';
  input.value = url;
  input.required = true;

  const tokenInput = document.createElement('input');
  tokenInput.type = 'password';
  tokenInput.className = 'grafana-token-input';
  tokenInput.placeholder = 'glsa_... (このGrafanaのSAトークン)';
  tokenInput.value = token;
  tokenInput.autocomplete = 'off';

  const statusBadge = document.createElement('span');
  statusBadge.className = 'server-status-badge unknown';
  statusBadge.textContent = '未確認';
  statusBadge.title = '設定を保存すると接続状態を確認します';

  const removeBtn = document.createElement('button');
  removeBtn.type = 'button';
  removeBtn.className = 'btn icon-btn';
  removeBtn.innerHTML = `<svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><line x1="18" y1="6" x2="6" y2="18"></line><line x1="6" y1="6" x2="18" y2="18"></line></svg>`;

  removeBtn.addEventListener('click', () => {
    row.remove();
  });

  row.appendChild(input);
  row.appendChild(tokenInput);
  row.appendChild(statusBadge);
  row.appendChild(removeBtn);
  return row;
}

function renderUrlInputs(servers) {
  urlListContainer.innerHTML = '';
  if (!servers || servers.length === 0) {
    urlListContainer.appendChild(createUrlRow());
    return;
  }
  servers.forEach(s => {
    urlListContainer.appendChild(createUrlRow(s.url || '', s.token || ''));
  });
}

function getServersFromUi() {
  const rows = urlListContainer.querySelectorAll('.url-input-row');
  const servers = [];
  rows.forEach(row => {
    const url = row.querySelector('.alertmanager-url-input').value.trim();
    const token = row.querySelector('.grafana-token-input').value.trim();
    if (url) servers.push({ url, token });
  });
  return servers;
}

// Load Application Configuration
async function loadConfig() {
  try {
    const config = await invoke('get_config');
    renderUrlInputs(config.servers);
    inputInterval.value = config.polling_interval_secs;
    currentInterval = config.polling_interval_secs;
    intervalVal.textContent = `${currentInterval}秒`;
    inputHeartbeatName.value = config.heartbeat_alert_name || '';

    startPollingTimer();
  } catch (e) {
    console.error('Failed to load config:', e);
    showToast('設定の読み込みに失敗しました');
  }
}

// Save Configuration
async function saveConfig() {
  const servers = getServersFromUi();
  const interval = parseInt(inputInterval.value, 10);
  const heartbeatAlertName = inputHeartbeatName.value.trim();

  if (servers.length === 0) {
    showToast('少なくとも1つのURLを入力してください。');
    return;
  }

  saveBtn.disabled = true;

  try {
    await invoke('save_config', { servers, interval, heartbeatAlertName });
    currentInterval = interval;
    showToast('設定を保存しました。');
    
    startPollingTimer();
    syncNow();
  } catch (e) {
    console.error('Failed to save config:', e);
    showToast(`設定の保存に失敗しました: ${e}`);
  } finally {
    saveBtn.disabled = false;
  }
}

// Start polling timer to refresh UI
function startPollingTimer() {
  if (pollingTimer) {
    clearInterval(pollingTimer);
  }
  fetchAlerts();
  pollingTimer = setInterval(() => {
    fetchAlerts();
  }, 5000);
}

// Initialize Application
window.addEventListener('DOMContentLoaded', () => {
  tabAlertsBtn = document.getElementById('tab-alerts');
  tabSettingsBtn = document.getElementById('tab-settings');
  alertsView = document.getElementById('alerts-view');
  settingsView = document.getElementById('settings-view');
  alertsList = document.getElementById('alerts-list');
  emptyState = document.getElementById('empty-state');
  alertCountBadge = document.getElementById('alert-count-badge');
  syncBtn = document.getElementById('sync-btn');
  statusIndicator = document.getElementById('status-indicator');
  statusText = document.getElementById('status-text');
  lastSyncText = document.getElementById('last-sync');
  urlListContainer = document.getElementById('url-list-container');
  addUrlBtn = document.getElementById('add-url-btn');
  inputInterval = document.getElementById('input-interval');
  intervalVal = document.getElementById('interval-val');
  inputHeartbeatName = document.getElementById('input-heartbeat-name');
  saveBtn = document.getElementById('save-btn');
  toast = document.getElementById('toast');
  toastMsg = document.getElementById('toast-msg');

  tabAlertsBtn.addEventListener('click', () => switchTab('alerts-view'));
  tabSettingsBtn.addEventListener('click', () => switchTab('settings-view'));

  syncBtn.addEventListener('click', syncNow);

  addUrlBtn.addEventListener('click', () => {
    urlListContainer.appendChild(createUrlRow());
  });

  inputInterval.addEventListener('input', (e) => {
    intervalVal.textContent = `${e.target.value}秒`;
  });

  saveBtn.addEventListener('click', saveConfig);

  document.getElementById('open-config-folder').addEventListener('click', (e) => {
    e.preventDefault();
    invoke('open_config_dir').catch(err => {
      console.error(err);
      showToast('フォルダを開くのに失敗しました');
    });
  });

  const autostartToggle = document.getElementById('autostart-toggle');
  
  // Initialize autostart toggle state
  invoke('plugin:autostart|is_enabled')
    .then(enabled => { autostartToggle.checked = enabled; })
    .catch(console.error);

  autostartToggle.addEventListener('change', async (e) => {
    try {
      if (e.target.checked) {
        await invoke('plugin:autostart|enable');
      } else {
        await invoke('plugin:autostart|disable');
      }
    } catch (err) {
      console.error('Failed to change autostart setting', err);
      showToast('自動起動の設定変更に失敗しました');
      e.target.checked = !e.target.checked; // Revert
    }
  });

  // Refresh data whenever the window regains focus, but DON'T change the active
  // tab — previously this forced the alerts view on every focus gain, so simply
  // clicking back into the settings screen yanked the user away to the alert list.
  window.__TAURI__.event.listen('tauri://focus', () => {
    fetchAlerts();
  });

  // Navigation is driven explicitly by the tray: a left click on the icon asks
  // for the alert list (quick look), while the "設定を開く" menu asks for settings.
  window.__TAURI__.event.listen('navigate-view', (event) => {
    const target = event.payload;
    if (target === 'alerts-view' || target === 'settings-view') {
      switchTab(target);
    }
    fetchAlerts();
  });

  loadConfig();
  setStatus('idle', '監視中');
});
