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
let saveBtn;
let toast;
let toastMsg;

// App State (Frontend Cache)
let pollingTimer = null;
let currentInterval = 60;

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
  alertsList.innerHTML = '';
  
  if (!alerts || alerts.length === 0) {
    emptyState.classList.remove('hide');
    alertsList.classList.add('hide');
    alertCountBadge.classList.add('hide');
    alertCountBadge.textContent = '0';
    return;
  }

  emptyState.classList.add('hide');
  alertsList.classList.remove('hide');
  
  alertCountBadge.classList.remove('hide');
  alertCountBadge.textContent = alerts.length;

  // Sorting: Critical -> Warning -> Info, then by newest
  const severityWeight = { critical: 3, warning: 2, info: 1 };
  
  alerts.sort((a, b) => {
    const sevA = severityWeight[(a.labels.severity || '').toLowerCase()] || 0;
    const sevB = severityWeight[(b.labels.severity || '').toLowerCase()] || 0;
    if (sevA !== sevB) return sevB - sevA; // Descending weight
    return new Date(b.startsAt) - new Date(a.startsAt); // Newest first
  });

  alerts.forEach(alert => {
    const card = document.createElement('div');
    const severity = (alert.labels.severity || 'warning').toLowerCase();
    card.className = `alert-card severity-${severity}`;

    const alertname = alert.labels.alertname || 'Unknown Alert';
    const summary = alert.annotations.summary || alert.annotations.description || '詳細情報はありません。';
    const relativeTime = fnFormatRelativeTime(alert.startsAt);
    
    let actionBtnHtml = '';
    if (alert.generatorURL) {
      actionBtnHtml = `
        <a href="${alert.generatorURL}" target="_blank" class="alert-action-btn">
          ソース表示
          <svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.5">
            <path d="M18 13v6a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V8a2 2 0 0 1 2-2h6M15 3h6v6M10 14L21 3"/>
          </svg>
        </a>
      `;
    }

    let labelPillsHtml = '';
    Object.entries(alert.labels).forEach(([key, val]) => {
      if (key !== 'alertname' && key !== 'severity') {
        labelPillsHtml += `<span class="label-pill">${key}=${val}</span>`;
      }
    });

    card.innerHTML = `
      <div class="alert-card-header">
        <div class="alert-title-container">
          <span class="alert-name">${alertname}</span>
          <span class="severity-badge">${severity}</span>
        </div>
        <span class="alert-time">${relativeTime}</span>
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
}

// Render connection errors UI
function renderConnectionErrors(errors) {
  const headerStatus = document.querySelector('.header-status');
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

// Poll alerts from Rust cache
async function fetchAlerts() {
  try {
    const alerts = await invoke('get_active_alerts');
    renderAlerts(alerts);
    lastSyncText.textContent = new Date().toLocaleTimeString('ja-JP');

    const errors = await invoke('get_connection_errors');
    renderConnectionErrors(errors);
  } catch (e) {
    console.error('Failed to get active alerts:', e);
  }
}

// Trigger immediate sync
async function syncNow() {
  setStatus('polling', '同期中...');
  const syncIcon = syncBtn.querySelector('.sync-icon');
  syncIcon.classList.add('spinning');
  
  try {
    await invoke('trigger_poll_now');
    // Wait briefly for rust loop to fetch and update state
    setTimeout(() => {
      fetchAlerts();
      setStatus('success', '同期完了');
      setTimeout(() => setStatus('idle', '監視中'), 3000);
      syncIcon.classList.remove('spinning');
    }, 1500);
  } catch (e) {
    console.error('Manual sync failed:', e);
    setStatus('error', '同期エラー');
    showToast(`同期に失敗しました: ${e}`);
    setTimeout(() => setStatus('idle', 'エラー監視中'), 5000);
    syncIcon.classList.remove('spinning');
  }
}

// URL Inputs Management
function createUrlRow(value = '') {
  const row = document.createElement('div');
  row.className = 'url-input-row';
  
  const input = document.createElement('input');
  input.type = 'url';
  input.className = 'alertmanager-url-input';
  input.placeholder = 'http://192.168.1.100:9093';
  input.value = value;
  input.required = true;

  const removeBtn = document.createElement('button');
  removeBtn.type = 'button';
  removeBtn.className = 'btn icon-btn';
  removeBtn.innerHTML = `<svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><line x1="18" y1="6" x2="6" y2="18"></line><line x1="6" y1="6" x2="18" y2="18"></line></svg>`;
  
  removeBtn.addEventListener('click', () => {
    row.remove();
  });

  row.appendChild(input);
  row.appendChild(removeBtn);
  return row;
}

function renderUrlInputs(urls) {
  urlListContainer.innerHTML = '';
  if (!urls || urls.length === 0) {
    urlListContainer.appendChild(createUrlRow());
    return;
  }
  urls.forEach(url => {
    urlListContainer.appendChild(createUrlRow(url));
  });
}

function getUrlsFromUi() {
  const inputs = document.querySelectorAll('.alertmanager-url-input');
  const urls = [];
  inputs.forEach(input => {
    const val = input.value.trim();
    if (val) urls.push(val);
  });
  return urls;
}

// Load Application Configuration
async function loadConfig() {
  try {
    const config = await invoke('get_config');
    renderUrlInputs(config.alertmanager_urls);
    inputInterval.value = config.polling_interval_secs;
    currentInterval = config.polling_interval_secs;
    intervalVal.textContent = `${currentInterval}秒`;
    
    startPollingTimer();
  } catch (e) {
    console.error('Failed to load config:', e);
    showToast('設定の読み込みに失敗しました');
  }
}

// Save Configuration
async function saveConfig() {
  const urls = getUrlsFromUi();
  const interval = parseInt(inputInterval.value, 10);

  if (urls.length === 0) {
    showToast('少なくとも1つのURLを入力してください。');
    return;
  }

  saveBtn.disabled = true;
  
  try {
    await invoke('save_config', { urls, interval });
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

  // Listen for window focus event (when brought up from tray)
  window.__TAURI__.event.listen('tauri://focus', () => {
    // 1. Force switch to alerts view to ensure they see the most critical info immediately
    switchTab('alerts-view');
    // 2. Fetch the latest state instantly
    fetchAlerts();
  });

  loadConfig();
  setStatus('idle', '監視中');
});
