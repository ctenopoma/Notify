// Notify monitoring web admin — hand-written SPA (no framework).
// Loads the source-of-truth state from /api/state, lets the user edit it across
// tabs, and writes it back. Config files are generated server-side by generator.py.

let STATE = null;       // the editable monitor-config object
let CATALOG = null;     // dcgm counters / metric sources / alert templates
let DISCOVER = null;    // last discovery result
let dirty = false;

const $ = (sel, root = document) => root.querySelector(sel);
const $$ = (sel, root = document) => [...root.querySelectorAll(sel)];

// --- API helpers ------------------------------------------------------------
async function api(path, opts) {
  const res = await fetch(path, opts);
  if (!res.ok) {
    const txt = await res.text();
    throw new Error(`${res.status} ${txt}`);
  }
  return res.json();
}
const getJSON = (p) => api(p);
const postJSON = (p, body) =>
  api(p, { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify(body || {}) });
const putJSON = (p, body) =>
  api(p, { method: 'PUT', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify(body) });

function toast(msg, kind = 'ok') {
  const t = $('#toast');
  t.textContent = msg;
  t.className = `toast ${kind}`;
  setTimeout(() => t.classList.add('hidden'), 4000);
}
function markDirty() { dirty = true; $('#dirty-badge').classList.remove('hidden'); }
function clearDirty() { dirty = false; $('#dirty-badge').classList.add('hidden'); }

// --- Boot -------------------------------------------------------------------
async function boot() {
  setupTabs();
  setupButtons();
  try {
    [STATE, CATALOG] = await Promise.all([getJSON('/api/state'), getJSON('/api/catalog')]);
  } catch (e) {
    toast('初期化に失敗: ' + e.message, 'bad');
    return;
  }
  renderAll();
  rediscover();
}

function setupTabs() {
  $$('#tabs button').forEach((b) =>
    b.addEventListener('click', () => {
      $$('#tabs button').forEach((x) => x.classList.remove('active'));
      $$('.tab').forEach((x) => x.classList.remove('active'));
      b.classList.add('active');
      $('#' + b.dataset.tab).classList.add('active');
    }));
}

function renderAll() {
  renderTargets();
  renderMetrics();
  renderAlerts();
  renderRetention();
  renderPreviewPicker();
}

// --- Discovery / overview ---------------------------------------------------
async function rediscover() {
  $('#discover-status').innerHTML = '<div class="card"><div class="v">スキャン中…</div></div>';
  try {
    DISCOVER = await getJSON('/api/discover');
  } catch (e) {
    $('#discover-status').innerHTML = `<div class="card"><div class="v sev-critical">スキャン失敗</div><div class="k">${e.message}</div></div>`;
    return;
  }
  renderDiscover();
  renderContainersTable();
  renderDiscoverAddOptions();
  renderMetrics(); // refresh "missing" hints on counters
}

function dot(ok) { return `<span class="dot ${ok ? 'ok' : 'bad'}"></span>`; }

function renderDiscover() {
  const d = DISCOVER;
  const cards = [
    ['Docker 接続', d.docker_ok],
    ['DCGM Exporter', d.dcgm_reachable],
    ['node-exporter', d.node_exporter_up],
    ['cAdvisor', d.cadvisor_up],
  ].map(([k, ok]) => `<div class="card"><div class="k">${k}</div><div class="v">${dot(ok)}${ok ? '応答あり' : '未応答'}</div></div>`);

  const probes = (d.metrics_probe || []).map((p) =>
    `<div class="card"><div class="k">${p.name} /metrics</div><div class="v">${dot(p.reachable)}${p.reachable ? '到達OK' : '未到達'}</div></div>`);

  $('#discover-status').innerHTML = cards.concat(probes).join('');

  const avail = d.dcgm_available || [];
  $('#dcgm-available').innerHTML = avail.length
    ? `<p class="hint">${avail.length} 個のフィールドを検出。</p><pre class="code">${avail.join('\n')}</pre>`
    : '<p class="hint">DCGM フィールドを検出できませんでした（GPU 非搭載 / dcgm-exporter 未起動 / PROF 非対応の可能性）。</p>';
}

function renderContainersTable() {
  const tb = $('#containers-table tbody');
  const list = (DISCOVER && DISCOVER.containers) || [];
  if (!list.length) { tb.innerHTML = '<tr><td colspan="5">コンテナが見つかりません</td></tr>'; return; }
  tb.innerHTML = list.map((c) => {
    const running = c.state === 'running';
    return `<tr>
      <td>${dot(running)}${c.name}</td>
      <td>${c.status || c.state}</td>
      <td>${c.image}</td>
      <td>${c.ports || ''}</td>
      <td>
        <button data-c="${c.name}" data-a="start">起動</button>
        <button data-c="${c.name}" data-a="restart">再起動</button>
        <button data-c="${c.name}" data-a="stop" class="danger">停止</button>
      </td></tr>`;
  }).join('');
  $$('#containers-table button').forEach((b) =>
    b.addEventListener('click', () => containerAction(b.dataset.c, b.dataset.a)));
}

async function containerAction(name, action) {
  try {
    const r = await postJSON('/api/container/action', { name, action });
    logOps(r);
    toast(`${name}: ${action} ${r.ok ? '成功' : '失敗'}`, r.ok ? 'ok' : 'bad');
    setTimeout(rediscover, 800);
  } catch (e) { toast(e.message, 'bad'); }
}

function renderDiscoverAddOptions() {
  const sel = $('#discover-add');
  const existing = new Set(STATE.containers.map((c) => c.name));
  const opts = ((DISCOVER && DISCOVER.containers) || [])
    .filter((c) => !existing.has(c.name))
    .map((c) => `<option value="${c.name}">${c.name}</option>`);
  sel.innerHTML = '<option value="">検出から追加…</option>' + opts.join('');
}

// --- Targets (containers) ---------------------------------------------------
function renderTargets() {
  const root = $('#containers-editor');
  root.innerHTML = STATE.containers.map((c, i) => containerItem(c, i)).join('');
  bindContainerItems();
}

function containerItem(c, i) {
  return `<div class="item" data-i="${i}">
    <div class="item-head">
      <input class="name" data-f="name" value="${esc(c.name)}" placeholder="コンテナ名" />
      <button class="del" data-del-container="${i}">削除</button>
    </div>
    <div class="grid">
      <label>スクレイプ先 (host:port)<input type="text" data-f="target" value="${esc(c.target || '')}" placeholder="vllm:8000" /></label>
      <label>metrics パス<input type="text" data-f="metrics_path" value="${esc(c.metrics_path || '/metrics')}" /></label>
    </div>
    <div class="checks">
      <label><input type="checkbox" data-f="scrape" ${c.scrape ? 'checked' : ''} /> /metrics 死活</label>
      <label><input type="checkbox" data-f="cadvisor_liveness" ${c.cadvisor_liveness ? 'checked' : ''} /> cAdvisor 死活</label>
      <label><input type="checkbox" data-f="absent_alert" ${c.absent_alert ? 'checked' : ''} /> 消失アラート</label>
    </div>
  </div>`;
}

function bindContainerItems() {
  $$('#containers-editor .item').forEach((el) => {
    const i = +el.dataset.i;
    $$('[data-f]', el).forEach((inp) => {
      inp.addEventListener('change', () => {
        const f = inp.dataset.f;
        STATE.containers[i][f] = inp.type === 'checkbox' ? inp.checked : inp.value;
        markDirty();
      });
    });
  });
  $$('[data-del-container]').forEach((b) =>
    b.addEventListener('click', () => {
      STATE.containers.splice(+b.dataset.delContainer, 1);
      markDirty(); renderTargets(); renderDiscoverAddOptions();
    }));
}

function addContainer(name = '') {
  STATE.containers.push({
    name, scrape: true, metrics_path: '/metrics', target: name ? name + ':8000' : '',
    cadvisor_liveness: true, absent_alert: true,
  });
  markDirty(); renderTargets(); renderDiscoverAddOptions();
}

// --- Metrics ----------------------------------------------------------------
function renderMetrics() {
  $('#node-enabled').checked = !!(STATE.node_exporter && STATE.node_exporter.enabled);
  $('#cadvisor-enabled').checked = !!(STATE.cadvisor && STATE.cadvisor.enabled);
  $('#gpu-enabled').checked = !!(STATE.gpu && STATE.gpu.enabled);
  renderCounters();
  renderCustomJobs();
}

function renderCounters() {
  const root = $('#dcgm-counters');
  const selected = new Set((STATE.gpu.counters || []).map((c) => c.field));
  const available = new Set((DISCOVER && DISCOVER.dcgm_available) || []);
  const haveDiscovery = available.size > 0;
  const groups = {};
  CATALOG.dcgm_counters.forEach((c) => { (groups[c.group] ||= []).push(c); });

  root.innerHTML = Object.entries(groups).map(([g, items]) => `
    <div class="counter-group"><h4>${g}</h4><div class="counter-list">
      ${items.map((c) => {
        const missing = haveDiscovery && !available.has(c.field);
        return `<label class="${missing ? 'missing' : ''}" title="${esc(c.help)}">
          <input type="checkbox" data-field="${c.field}" data-type="${c.type}" data-help="${esc(c.help)}" ${selected.has(c.field) ? 'checked' : ''} />
          ${c.prof ? '⚡' : ''}${c.field}${missing ? ' (未検出)' : ''}</label>`;
      }).join('')}
    </div></div>`).join('');

  $$('#dcgm-counters input[type=checkbox]').forEach((inp) =>
    inp.addEventListener('change', () => {
      const field = inp.dataset.field;
      STATE.gpu.counters = STATE.gpu.counters || [];
      if (inp.checked) {
        if (!STATE.gpu.counters.some((c) => c.field === field))
          STATE.gpu.counters.push({ field, type: inp.dataset.type, help: inp.dataset.help });
      } else {
        STATE.gpu.counters = STATE.gpu.counters.filter((c) => c.field !== field);
      }
      markDirty();
    }));
}

function renderCustomJobs() {
  const root = $('#custom-jobs');
  STATE.custom_jobs = STATE.custom_jobs || [];
  root.innerHTML = STATE.custom_jobs.map((j, i) => `<div class="item" data-i="${i}">
    <div class="item-head">
      <input class="name" data-cf="job_name" value="${esc(j.job_name || '')}" placeholder="job 名" />
      <button class="del" data-del-job="${i}">削除</button>
    </div>
    <div class="grid">
      <label>ターゲット (カンマ区切り)<input type="text" data-cf="targets" value="${esc((j.targets || []).join(','))}" placeholder="exporter:9115" /></label>
      <label>metrics パス<input type="text" data-cf="metrics_path" value="${esc(j.metrics_path || '')}" placeholder="/metrics" /></label>
      <label>scheme<input type="text" data-cf="scheme" value="${esc(j.scheme || '')}" placeholder="http" /></label>
    </div></div>`).join('');
  $$('#custom-jobs .item').forEach((el) => {
    const i = +el.dataset.i;
    $$('[data-cf]', el).forEach((inp) => inp.addEventListener('change', () => {
      const f = inp.dataset.cf;
      STATE.custom_jobs[i][f] = f === 'targets'
        ? inp.value.split(',').map((s) => s.trim()).filter(Boolean) : inp.value;
      markDirty();
    }));
  });
  $$('[data-del-job]').forEach((b) => b.addEventListener('click', () => {
    STATE.custom_jobs.splice(+b.dataset.delJob, 1); markDirty(); renderCustomJobs();
  }));
}

// --- Alerts -----------------------------------------------------------------
function renderAlerts() {
  // template picker
  const picker = $('#template-picker');
  picker.innerHTML = '<option value="">テンプレートから追加…</option>' +
    CATALOG.alert_templates.map((t, i) => `<option value="${i}">[${t.category}] ${t.name}</option>`).join('');

  const root = $('#alerts-editor');
  STATE.alerts = STATE.alerts || [];
  root.innerHTML = STATE.alerts.map((a, i) => alertItem(a, i)).join('');
  bindAlertItems();
}

function alertItem(a, i) {
  return `<div class="item" data-i="${i}">
    <div class="item-head">
      <input class="name" data-af="name" value="${esc(a.name || '')}" placeholder="アラート名" />
      <label class="sev-${a.severity}"><input type="checkbox" data-af="enabled" ${a.enabled !== false ? 'checked' : ''} /> 有効</label>
      <button class="del" data-del-alert="${i}">削除</button>
    </div>
    <div class="grid">
      <label>グループ<input type="text" data-af="group" value="${esc(a.group || 'custom')}" /></label>
      <label>severity
        <select data-af="severity">
          <option ${a.severity === 'warning' ? 'selected' : ''}>warning</option>
          <option ${a.severity === 'critical' ? 'selected' : ''}>critical</option>
          <option ${a.severity === 'info' ? 'selected' : ''}>info</option>
        </select></label>
      <label>for (継続)<input type="text" data-af="for" value="${esc(a.for || '')}" placeholder="5m" /></label>
    </div>
    <label class="grid-full">PromQL 条件 (expr)
      <textarea data-af="expr" rows="3">${esc(a.expr || '')}</textarea></label>
    <label class="grid-full">summary<input type="text" data-af="summary" value="${esc(a.summary || '')}" /></label>
    <label class="grid-full">description<input type="text" data-af="description" value="${esc(a.description || '')}" /></label>
  </div>`;
}

function bindAlertItems() {
  $$('#alerts-editor .item').forEach((el) => {
    const i = +el.dataset.i;
    $$('[data-af]', el).forEach((inp) => inp.addEventListener('change', () => {
      const f = inp.dataset.af;
      STATE.alerts[i][f] = inp.type === 'checkbox' ? inp.checked : inp.value;
      markDirty();
    }));
  });
  $$('[data-del-alert]').forEach((b) => b.addEventListener('click', () => {
    STATE.alerts.splice(+b.dataset.delAlert, 1); markDirty(); renderAlerts();
  }));
}

function addTemplateAlert(idx) {
  const tpl = CATALOG.alert_templates[+idx];
  if (!tpl) return;
  let expr = tpl.expr;
  let name = tpl.name;
  (tpl.params || []).forEach((p) => {
    const val = prompt(`${tpl.name}: ${p.label}`, p.default);
    if (val === null) return;
    expr = expr.split('${' + p.name + '}').join(val);
  });
  // leftover placeholders -> defaults
  (tpl.params || []).forEach((p) => { expr = expr.split('${' + p.name + '}').join(p.default); });
  STATE.alerts.push({
    group: tpl.group, name, expr, for: tpl.for, severity: tpl.severity,
    summary: tpl.summary, description: tpl.description, enabled: true,
  });
  markDirty(); renderAlerts();
}

function addCustomAlert() {
  STATE.alerts.push({
    group: 'custom', name: 'MyAlert', expr: '', for: '5m',
    severity: 'warning', summary: '', description: '', enabled: true,
  });
  markDirty(); renderAlerts();
}

// --- Retention --------------------------------------------------------------
function renderRetention() {
  const r = STATE.retention || (STATE.retention = {});
  $('#prom-time').value = r.prometheus_time || '';
  $('#prom-size').value = r.prometheus_size || '';
  $('#log-max-size').value = r.log_max_size || '';
  $('#log-max-file').value = r.log_max_file || '';
  const map = {
    '#prom-time': 'prometheus_time', '#prom-size': 'prometheus_size',
    '#log-max-size': 'log_max_size', '#log-max-file': 'log_max_file',
  };
  Object.entries(map).forEach(([sel, key]) =>
    $(sel).addEventListener('change', () => { r[key] = $(sel).value; markDirty(); }));
}

// --- Preview & ops ----------------------------------------------------------
function renderPreviewPicker() {
  const files = {
    prometheus: 'prometheus.yml', grafana_alerts: 'Grafana アラートルール',
    dcgm: 'dcgm-counters.csv', env: '.env',
  };
  $('#preview-picker').innerHTML = Object.entries(files)
    .map(([k, v]) => `<option value="${k}">${v}</option>`).join('');
}

async function doPreview() {
  try {
    const data = await getJSON('/api/preview');
    const key = $('#preview-picker').value;
    $('#preview-out').textContent = data[key] || '(空)';
  } catch (e) { toast(e.message, 'bad'); }
}

function logOps(r) {
  const el = $('#op-log');
  const stamp = new Date().toLocaleTimeString();
  const body = typeof r === 'string' ? r : JSON.stringify(r, null, 2);
  el.textContent = `[${stamp}]\n${body}\n\n` + el.textContent;
}

async function save(writeFiles = true) {
  try {
    const r = await putJSON('/api/state', { state: STATE, write_files: writeFiles });
    clearDirty();
    toast('保存しました' + (writeFiles ? '（設定ファイル生成済み）' : ''), 'ok');
    return r;
  } catch (e) { toast('保存失敗: ' + e.message, 'bad'); throw e; }
}

async function apply() {
  await save(true);
  try {
    const r = await postJSON('/api/apply', {});
    logOps(r);
    toast('反映 ' + (r.ok ? '完了' : '一部失敗（ログ確認）'), r.ok ? 'ok' : 'bad');
  } catch (e) { toast('反映失敗: ' + e.message, 'bad'); }
}

async function compose(action, services = []) {
  try {
    toast(`docker compose ${action} 実行中…`);
    const r = await postJSON('/api/compose/action', { action, services });
    logOps(r);
    toast(`compose ${action} ${r.ok ? '成功' : '失敗'}`, r.ok ? 'ok' : 'bad');
    setTimeout(rediscover, 1000);
  } catch (e) { toast(e.message, 'bad'); }
}

// --- Buttons ----------------------------------------------------------------
function setupButtons() {
  $('#btn-save').addEventListener('click', () => save(true));
  $('#btn-apply').addEventListener('click', apply);
  $('#btn-rediscover').addEventListener('click', rediscover);
  $('#btn-add-container').addEventListener('click', () => addContainer());
  $('#discover-add').addEventListener('change', (e) => { if (e.target.value) addContainer(e.target.value); });
  $('#node-enabled').addEventListener('change', (e) => { STATE.node_exporter.enabled = e.target.checked; markDirty(); });
  $('#cadvisor-enabled').addEventListener('change', (e) => { STATE.cadvisor.enabled = e.target.checked; markDirty(); });
  $('#gpu-enabled').addEventListener('change', (e) => { STATE.gpu.enabled = e.target.checked; markDirty(); });
  $('#btn-add-job').addEventListener('click', () => { STATE.custom_jobs.push({ job_name: '', targets: [], metrics_path: '/metrics' }); markDirty(); renderCustomJobs(); });
  $('#template-picker').addEventListener('change', (e) => { if (e.target.value !== '') { addTemplateAlert(e.target.value); e.target.value = ''; } });
  $('#btn-add-custom-alert').addEventListener('click', addCustomAlert);
  $('#btn-preview').addEventListener('click', doPreview);
  $('#preview-picker').addEventListener('change', doPreview);
  $('#op-apply').addEventListener('click', apply);
  $('#op-reload').addEventListener('click', async () => { logOps(await postJSON('/api/reload-prometheus', {})); });
  $('#op-up').addEventListener('click', () => compose('up'));
  $('#op-restart').addEventListener('click', () => compose('restart'));
  $('#op-recreate').addEventListener('click', () => compose('recreate'));
  $('#op-pull').addEventListener('click', () => compose('pull'));
  $('#op-down').addEventListener('click', () => { if (confirm('監視スタックを停止します。よろしいですか?')) compose('down'); });
  window.addEventListener('beforeunload', (e) => { if (dirty) { e.preventDefault(); e.returnValue = ''; } });
}

function esc(s) {
  return String(s == null ? '' : s)
    .replaceAll('&', '&amp;').replaceAll('<', '&lt;').replaceAll('>', '&gt;')
    .replaceAll('"', '&quot;');
}

boot();
