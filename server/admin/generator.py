"""Render every monitoring config file from a single state dict.

`monitor-config.json` (edited via the web UI) is the source of truth. From it we
generate prometheus.yml, the Grafana unified-alerting rule provisioning
(grafana/provisioning/alerting/rules.yml), dcgm-counters.csv and the .env file
that feeds docker-compose (retention / log-rotation knobs).

Alerting lives entirely in Grafana now: Grafana evaluates the rules against the
Prometheus datasource and its embedded Alertmanager exposes them at
/api/alertmanager/grafana/api/v2/alerts — which the Notify desktop app polls.
There is no standalone Alertmanager, Loki or promtail.

The generated files intentionally match docs/monitoring_setup.md so the doc
stays an accurate description of what the stack runs.
"""
from __future__ import annotations

import hashlib
import json
import os
import re
from pathlib import Path

import yaml

import catalog


# ---------------------------------------------------------------------------
# Default state — used to seed config/monitor-config.json on first run.
# ---------------------------------------------------------------------------
def default_state() -> dict:
    return {
        "global": {"scrape_interval": "15s", "evaluation_interval": "15s"},
        "retention": {
            # Prometheus tsdb retention (time and/or size; "0" disables that limit)
            "prometheus_time": "15d",
            "prometheus_size": "0",
            # Docker json-file log rotation for *this stack's* containers
            "log_max_size": "10m",
            "log_max_file": "3",
        },
        # Only `heartbeat`/`heartbeat_alert` are still used — they drive the
        # always-firing Grafana rule the Notify app uses as a connection probe.
        # (Routing/grouping now lives in Grafana's notification policy.)
        "alertmanager": {
            "heartbeat": True,
            "heartbeat_alert": "AlwaysFiringTest",
        },
        "node_exporter": {"enabled": True},
        "cadvisor": {"enabled": True},
        "gpu": {"enabled": True, "counters": catalog.default_dcgm_counters()},
        "containers": [
            {
                "name": "vllm", "scrape": True, "metrics_path": "/metrics",
                "target": "vllm:8000", "cadvisor_liveness": True, "absent_alert": True,
            },
            {
                "name": "litellm", "scrape": True, "metrics_path": "/metrics",
                "target": "litellm:4000", "cadvisor_liveness": True, "absent_alert": True,
            },
        ],
        # Free-form extra scrape jobs (custom exporters etc.)
        "custom_jobs": [],
        # GPU + resource + user-defined alert rules. Liveness/absent/log rules are
        # generated automatically from `containers`, so they are NOT stored here.
        "alerts": _seed_alerts(),
    }


def _seed_alerts() -> list:
    """Instantiate the GPU + a couple of host-resource templates as defaults."""
    out = []
    for tpl in catalog.ALERT_TEMPLATES:
        if tpl["category"] not in ("GPU", "ホスト"):
            continue
        expr = tpl["expr"]
        for p in tpl["params"]:
            expr = expr.replace("${%s}" % p["name"], p["default"])
        out.append({
            "group": tpl["group"],
            "name": tpl["name"],
            "expr": expr,
            "for": tpl["for"],
            "severity": tpl["severity"],
            "summary": tpl["summary"],
            "description": tpl["description"],
            # Host CPU/mem/disk default to off until node-exporter data is trusted.
            "enabled": tpl["category"] == "GPU",
        })
    return out


# ---------------------------------------------------------------------------
# YAML helper: keep block style and avoid alphabetical key reordering.
# ---------------------------------------------------------------------------
class _Dumper(yaml.SafeDumper):
    pass


def _str_presenter(dumper, data):
    style = "|" if "\n" in data else None
    return dumper.represent_scalar("tag:yaml.org,2002:str", data, style=style)


_Dumper.add_representer(str, _str_presenter)


def _dump(obj) -> str:
    return yaml.dump(obj, Dumper=_Dumper, sort_keys=False, default_flow_style=False,
                     allow_unicode=True, width=4096)


# ---------------------------------------------------------------------------
# Individual file renderers
# ---------------------------------------------------------------------------
def render_prometheus(state: dict) -> str:
    g = state.get("global", {})
    cfg = {
        "global": {
            "scrape_interval": g.get("scrape_interval", "15s"),
            "evaluation_interval": g.get("evaluation_interval", "15s"),
        },
        # No `alerting`/`rule_files`: rule evaluation and routing moved to Grafana
        # unified alerting. Prometheus is now purely a metrics store/query engine.
        "scrape_configs": [
            {"job_name": "prometheus", "static_configs": [{"targets": ["localhost:9090"]}]},
        ],
    }
    sc = cfg["scrape_configs"]
    if state.get("cadvisor", {}).get("enabled", True):
        sc.append({"job_name": "cadvisor", "static_configs": [{"targets": ["cadvisor:8080"]}]})
    if state.get("node_exporter", {}).get("enabled", True):
        sc.append({"job_name": "node", "static_configs": [{"targets": ["node-exporter:9100"]}]})
    if state.get("gpu", {}).get("enabled", True):
        sc.append({"job_name": "dcgm", "static_configs": [{"targets": ["dcgm-exporter:9400"]}]})
    for c in state.get("containers", []):
        if c.get("scrape"):
            sc.append({
                "job_name": c["name"],
                "metrics_path": c.get("metrics_path", "/metrics"),
                "static_configs": [{"targets": [c["target"]]}],
            })
    for j in state.get("custom_jobs", []):
        job = {"job_name": j["job_name"]}
        if j.get("metrics_path"):
            job["metrics_path"] = j["metrics_path"]
        if j.get("scheme"):
            job["scheme"] = j["scheme"]
        job["static_configs"] = [{"targets": j.get("targets", [])}]
        sc.append(job)
    header = ("# AUTO-GENERATED by the Notify monitoring web admin.\n"
              "# Edit via the web UI (http://<host>:8088), not by hand.\n")
    return header + _dump(cfg)


# Datasource UID that the Prometheus datasource is provisioned with
# (see grafana/provisioning/datasources/datasources.yml).
_PROM_DS_UID = "prometheus"
_ALERT_FOLDER = "Notify Alerts"


def _collect_alert_rules(state: dict) -> list[tuple[str, list[dict]]]:
    """Gather every alert as (group_name, [intermediate rule dicts]).

    Each intermediate rule is {name, expr (PromQL), for, severity, summary,
    description}. This is the provider-neutral shape that both the heartbeat /
    liveness / cAdvisor auto-rules and the user/GPU/resource rules share; the
    Grafana renderer below turns it into the unified-alerting graph format.
    """
    groups: list[tuple[str, list[dict]]] = []

    am = state.get("alertmanager", {})
    if am.get("heartbeat", True):
        groups.append(("heartbeat_rules", [{
            "name": am.get("heartbeat_alert", "AlwaysFiringTest"),
            "expr": "vector(1)", "for": "10s", "severity": "warning",
            "summary": "Notify 接続テスト用アラート",
            "description": "常時発火。Notify アプリの接続確認に使用します。",
        }]))

    # Liveness via /metrics up==0
    liveness = []
    for c in state.get("containers", []):
        if c.get("scrape"):
            liveness.append({
                "name": _pascal(c["name"]) + "Down",
                "expr": 'up{job="%s"} == 0' % c["name"],
                "for": "1m", "severity": "critical",
                "summary": "%s が応答していません" % c["name"],
                "description": "%s (job=%s) のスクレイプに1分以上失敗しています。" % (c["name"], c["name"]),
            })
    if liveness:
        groups.append(("llm_liveness", liveness))

    # cAdvisor-based liveness (stalled + absent)
    cadv_rules = []
    watched = [c["name"] for c in state.get("containers", []) if c.get("cadvisor_liveness")]
    if watched:
        cadv_rules.append({
            "name": "WatchedContainerStalled",
            "expr": 'time() - container_last_seen{name=~"%s"} > 60' % "|".join(watched),
            "for": "0s", "severity": "critical",
            "summary": "コンテナ [{{ $labels.name }}] が60秒以上観測されていません",
            "description": "停止または再起動中の可能性があります。",
        })
    for c in state.get("containers", []):
        if c.get("absent_alert"):
            cadv_rules.append({
                "name": _pascal(c["name"]) + "ContainerAbsent",
                "expr": 'absent(container_last_seen{name="%s"})' % c["name"],
                "for": "1m", "severity": "critical",
                "summary": "%s コンテナが存在しません" % c["name"],
                "description": "",
            })
    if cadv_rules:
        groups.append(("llm_container_liveness", cadv_rules))

    # User/GPU/resource alerts, grouped by their `group` field, order preserved.
    by_group: dict[str, list] = {}
    order: list[str] = []
    for a in state.get("alerts", []):
        if not a.get("enabled", True):
            continue
        grp = a.get("group", "custom")
        if grp not in by_group:
            by_group[grp] = []
            order.append(grp)
        by_group[grp].append({
            "name": a["name"],
            "expr": a["expr"],
            "for": a.get("for") or "0s",
            "severity": a.get("severity", "warning"),
            "summary": a.get("summary", ""),
            "description": a.get("description", ""),
        })
    for grp in order:
        groups.append((grp, by_group[grp]))

    return groups


# Grafana annotation templating uses `{{ $values.X }}`, not Prometheus's
# `{{ $value }}`. Our condition query always evaluates to 1 (see _to_grafana_rule),
# so `$value` carries no meaning here — strip those mustaches to avoid template
# errors while preserving `{{ $labels.X }}`, which Grafana renders natively.
_VALUE_MUSTACHE = re.compile(r"\{\{-?\s*\$value.*?\}\}")


def _sanitize_annotation(text: str) -> str:
    return _VALUE_MUSTACHE.sub("", text or "").strip()


def _rule_uid(group: str, name: str) -> str:
    digest = hashlib.sha1(("%s/%s" % (group, name)).encode("utf-8")).hexdigest()
    return "notify-" + digest[:20]


def _to_grafana_rule(group: str, r: dict) -> dict:
    """Convert an intermediate rule into a Grafana unified-alerting rule.

    The PromQL expr `E` already encodes the firing condition by *filtering*
    (e.g. `temp > 85` returns the value only when true; `up == 0` returns 0 when
    firing). We wrap it as `(E) * 0 + 1` so every series that passes the filter
    yields exactly 1 — making a single `> 0` threshold a correct, uniform firing
    test even for `== 0` style rules. When `E` matches nothing the query returns
    no data, and `noDataState: OK` keeps the rule quiet.
    """
    annotations = {}
    summary = _sanitize_annotation(r.get("summary", ""))
    description = _sanitize_annotation(r.get("description", ""))
    if summary:
        annotations["summary"] = summary
    if description:
        annotations["description"] = description

    return {
        "uid": _rule_uid(group, r["name"]),
        "title": r["name"],
        "condition": "C",
        "for": r.get("for", "0s"),
        "data": [
            {
                "refId": "A",
                "relativeTimeRange": {"from": 600, "to": 0},
                "datasourceUid": _PROM_DS_UID,
                "model": {
                    "refId": "A",
                    "editorMode": "code",
                    "expr": "(%s) * 0 + 1" % r["expr"],
                    "instant": True,
                    "range": False,
                    "intervalMs": 1000,
                    "maxDataPoints": 43200,
                    "legendFormat": "__auto",
                },
            },
            {
                "refId": "C",
                "relativeTimeRange": {"from": 600, "to": 0},
                "datasourceUid": "__expr__",
                "model": {
                    "refId": "C",
                    "type": "threshold",
                    "expression": "A",
                    "conditions": [{"evaluator": {"type": "gt", "params": [0]}}],
                    "intervalMs": 1000,
                    "maxDataPoints": 43200,
                },
            },
        ],
        "noDataState": "OK",
        "execErrState": "Error",
        "labels": {"severity": r.get("severity", "warning")},
        "annotations": annotations,
    }


def render_grafana_alerting(state: dict) -> str:
    """Grafana unified-alerting provisioning (folder + rule groups).

    Grafana evaluates these against the Prometheus datasource and surfaces firing
    instances on its embedded Alertmanager API, which the Notify desktop app polls.
    """
    groups = []
    for group_name, rules in _collect_alert_rules(state):
        if not rules:
            continue
        groups.append({
            "orgId": 1,
            "name": group_name,
            "folder": _ALERT_FOLDER,
            "interval": "1m",
            "rules": [_to_grafana_rule(group_name, r) for r in rules],
        })

    header = "# AUTO-GENERATED by the Notify monitoring web admin. Edit via http://<host>:8088.\n"
    return header + _dump({"apiVersion": 1, "groups": groups})


def render_dcgm_csv(state: dict) -> str:
    # NOTE: dcgm-exporter parses this with a strict CSV reader that does NOT skip
    # comment lines — every record must have exactly 3 comma-separated fields, or
    # it dies with "wrong number of fields". So emit data lines only (no comments).
    lines = []
    for c in state.get("gpu", {}).get("counters", []):
        lines.append("%s, %s, %s" % (c["field"], c["type"], c["help"]))
    return "\n".join(lines) + "\n"


def render_env(state: dict, existing: str | None = None) -> str:
    """Render .env, preserving any keys the UI does not manage (e.g. *_PORT).

    Only the retention/disk/credential keys below are owned by the generator.
    User-set keys (published host ports, custom vars) in the existing .env are
    kept verbatim so the admin doesn't clobber them when saving.
    """
    r = state.get("retention", {})
    g = state.get("grafana", {})
    managed = {
        # .env's MONITOR_DIR must be the HOST path (used by compose on the CLI/daemon),
        # not the in-container path the app uses for file IO.
        "MONITOR_DIR": os.environ.get("HOST_MONITOR_DIR") or os.environ.get("MONITOR_DIR", "/opt/monitor"),
        "PROM_RETENTION_TIME": r.get("prometheus_time", "15d"),
        "PROM_RETENTION_SIZE": r.get("prometheus_size", "0"),
        "LOG_MAX_SIZE": r.get("log_max_size", "10m"),
        "LOG_MAX_FILE": r.get("log_max_file", "3"),
        "GF_ADMIN_USER": g.get("admin_user", "admin"),
        "GF_ADMIN_PASSWORD": g.get("admin_password", "admin"),
    }

    out_lines: list[str] = []
    seen: set[str] = set()
    if existing:
        for line in existing.splitlines():
            stripped = line.strip()
            if "=" in stripped and not stripped.startswith("#"):
                key = stripped.split("=", 1)[0].strip()
                if key in managed:
                    out_lines.append("%s=%s" % (key, managed[key]))
                    seen.add(key)
                    continue
            out_lines.append(line)  # comment or unmanaged key — keep as-is
    else:
        out_lines = [
            "# AUTO-GENERATED by the Notify monitoring web admin (retention / disk knobs).",
            "# Published host ports live here too; change *_PORT to resolve conflicts.",
        ]
    # Append any managed keys not already present.
    for key, val in managed.items():
        if key not in seen:
            out_lines.append("%s=%s" % (key, val))
    return "\n".join(out_lines) + "\n"


def _pascal(name: str) -> str:
    """vllm -> Vllm, lite-llm -> LiteLlm, my_app -> MyApp (for alert names)."""
    parts = []
    for chunk in name.replace("-", "_").split("_"):
        parts.append(chunk[:1].upper() + chunk[1:] if chunk else "")
    return "".join(parts) or "Service"


# ---------------------------------------------------------------------------
# Write everything to disk.
# ---------------------------------------------------------------------------
# Maps logical file -> (relative path under config/, renderer)
FILES = {
    "prometheus": ("prometheus.yml", render_prometheus),
    "grafana_alerts": ("grafana/provisioning/alerting/rules.yml", render_grafana_alerting),
    "dcgm": ("dcgm-counters.csv", render_dcgm_csv),
}


def render_all(state: dict, monitor_dir: Path | None = None) -> dict:
    """Return {logical_name: text} for preview without touching disk."""
    out = {name: fn(state) for name, (_, fn) in FILES.items()}
    existing = None
    if monitor_dir is not None:
        env_path = monitor_dir / ".env"
        if env_path.exists():
            existing = env_path.read_text(encoding="utf-8")
    out["env"] = render_env(state, existing)
    return out


def write_all(state: dict, config_dir: Path, monitor_dir: Path) -> list[str]:
    """Write all generated files. Returns the list of paths written."""
    written = []
    for _, (rel, fn) in FILES.items():
        path = config_dir / rel
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(fn(state), encoding="utf-8")
        written.append(str(path))
    # .env lives next to docker-compose.yml (monitor_dir), not under config/.
    # Preserve unmanaged keys (host ports etc.) already present in .env.
    env_path = monitor_dir / ".env"
    existing = env_path.read_text(encoding="utf-8") if env_path.exists() else None
    env_path.write_text(render_env(state, existing), encoding="utf-8")
    written.append(str(env_path))
    return written


def load_state(config_dir: Path) -> dict:
    path = config_dir / "monitor-config.json"
    if path.exists():
        return json.loads(path.read_text(encoding="utf-8"))
    state = default_state()
    save_state(state, config_dir)
    return state


def save_state(state: dict, config_dir: Path) -> None:
    config_dir.mkdir(parents=True, exist_ok=True)
    path = config_dir / "monitor-config.json"
    path.write_text(json.dumps(state, ensure_ascii=False, indent=2), encoding="utf-8")
