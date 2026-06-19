"""Render every monitoring config file from a single state dict.

`monitor-config.json` (edited via the web UI) is the source of truth. From it we
generate prometheus.yml, alert.rules.yml, alertmanager.yml, promtail-config.yml,
loki-config.yml, the Loki log-alert rules, dcgm-counters.csv and the .env file
that feeds docker-compose (retention / log-rotation knobs).

The generated files intentionally match docs/monitoring_setup.md so the doc
stays an accurate description of what the stack runs.
"""
from __future__ import annotations

import json
import os
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
            # Loki log retention window (Go duration, e.g. 168h = 7d)
            "loki_period": "168h",
            # Docker json-file log rotation for *this stack's* containers
            "log_max_size": "10m",
            "log_max_file": "3",
        },
        "alertmanager": {
            "resolve_timeout": "5m",
            "group_wait": "10s",
            "group_interval": "10s",
            "repeat_interval": "1h",
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
                "logs": True, "log_patterns": "(?i)error|exception|cuda|out of memory",
                "log_threshold": 0,
            },
            {
                "name": "litellm", "scrape": True, "metrics_path": "/metrics",
                "target": "litellm:4000", "cadvisor_liveness": True, "absent_alert": True,
                "logs": True, "log_patterns": "(?i)error|exception",
                "log_threshold": 0,
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
        "alerting": {"alertmanagers": [{"static_configs": [{"targets": ["alertmanager:9093"]}]}]},
        "rule_files": ["alert.rules.yml"],
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


def render_alert_rules(state: dict) -> str:
    groups = []

    am = state.get("alertmanager", {})
    if am.get("heartbeat", True):
        groups.append({
            "name": "heartbeat_rules",
            "rules": [{
                "alert": am.get("heartbeat_alert", "AlwaysFiringTest"),
                "expr": "vector(1)", "for": "10s",
                "labels": {"severity": "warning"},
                "annotations": {
                    "summary": "Notify 接続テスト用アラート",
                    "description": "常時発火。Notify アプリの接続確認に使用します。",
                },
            }],
        })

    # Liveness via /metrics up==0
    liveness = []
    for c in state.get("containers", []):
        if c.get("scrape"):
            liveness.append({
                "alert": _pascal(c["name"]) + "Down",
                "expr": 'up{job="%s"} == 0' % c["name"],
                "for": "1m",
                "labels": {"severity": "critical"},
                "annotations": {
                    "summary": "%s が応答していません" % c["name"],
                    "description": "%s (job=%s) のスクレイプに1分以上失敗しています。" % (c["name"], c["name"]),
                },
            })
    if liveness:
        groups.append({"name": "llm_liveness", "rules": liveness})

    # cAdvisor-based liveness (stalled + absent)
    cadv_rules = []
    watched = [c["name"] for c in state.get("containers", []) if c.get("cadvisor_liveness")]
    if watched:
        cadv_rules.append({
            "alert": "WatchedContainerStalled",
            "expr": 'time() - container_last_seen{name=~"%s"} > 60' % "|".join(watched),
            "labels": {"severity": "critical"},
            "annotations": {
                "summary": "コンテナ [{{ $labels.name }}] が60秒以上観測されていません",
                "description": "停止または再起動中の可能性があります。",
            },
        })
    for c in state.get("containers", []):
        if c.get("absent_alert"):
            cadv_rules.append({
                "alert": _pascal(c["name"]) + "ContainerAbsent",
                "expr": 'absent(container_last_seen{name="%s"})' % c["name"],
                "for": "1m",
                "labels": {"severity": "critical"},
                "annotations": {"summary": "%s コンテナが存在しません" % c["name"]},
            })
    if cadv_rules:
        groups.append({"name": "llm_container_liveness", "rules": cadv_rules})

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
        rule = {"alert": a["name"], "expr": a["expr"]}
        if a.get("for"):
            rule["for"] = a["for"]
        rule["labels"] = {"severity": a.get("severity", "warning")}
        ann = {}
        if a.get("summary"):
            ann["summary"] = a["summary"]
        if a.get("description"):
            ann["description"] = a["description"]
        if ann:
            rule["annotations"] = ann
        by_group[grp].append(rule)
    for grp in order:
        groups.append({"name": grp, "rules": by_group[grp]})

    header = "# AUTO-GENERATED by the Notify monitoring web admin.\n"
    return header + _dump({"groups": groups})


def render_alertmanager(state: dict) -> str:
    am = state.get("alertmanager", {})
    cfg = {
        "global": {"resolve_timeout": am.get("resolve_timeout", "5m")},
        "route": {
            "group_by": ["alertname"],
            "group_wait": am.get("group_wait", "10s"),
            "group_interval": am.get("group_interval", "10s"),
            "repeat_interval": am.get("repeat_interval", "1h"),
            "receiver": "default-receiver",
        },
        "receivers": [{"name": "default-receiver"}],
    }
    return "# AUTO-GENERATED by the Notify monitoring web admin.\n" + _dump(cfg)


def render_promtail(state: dict) -> str:
    names = [c["name"] for c in state.get("containers", []) if c.get("logs")]
    keep_regex = "/(%s)" % "|".join(names) if names else "/(__none__)"
    cfg = {
        "server": {"http_listen_port": 9080, "grpc_listen_port": 0},
        "positions": {"filename": "/tmp/positions.yaml"},
        "clients": [{"url": "http://loki:3100/loki/api/v1/push"}],
        "scrape_configs": [{
            "job_name": "docker_llm",
            "docker_sd_configs": [{"host": "unix:///var/run/docker.sock", "refresh_interval": "5s"}],
            "relabel_configs": [
                {"source_labels": ["__meta_docker_container_name"], "regex": keep_regex, "action": "keep"},
                {"source_labels": ["__meta_docker_container_name"], "regex": "/(.*)", "target_label": "container"},
            ],
        }],
    }
    return "# AUTO-GENERATED by the Notify monitoring web admin.\n" + _dump(cfg)


def render_loki_config(state: dict) -> str:
    period = state.get("retention", {}).get("loki_period", "168h")
    cfg = {
        "auth_enabled": False,
        "server": {"http_listen_port": 3100},
        "common": {
            "path_prefix": "/tmp/loki",
            "storage": {"filesystem": {
                "chunks_directory": "/tmp/loki/chunks",
                "rules_directory": "/tmp/loki/rules",
            }},
            "replication_factor": 1,
            "ring": {"kvstore": {"store": "inmemory"}},
        },
        "schema_config": {"configs": [{
            "from": "2020-10-24", "store": "tsdb", "object_store": "filesystem",
            "schema": "v13", "index": {"prefix": "index_", "period": "24h"},
        }]},
        # Retention: enabled in the compactor so old logs are deleted, capping disk.
        "limits_config": {"retention_period": period},
        "compactor": {
            "working_directory": "/tmp/loki/compactor",
            "retention_enabled": True,
            "delete_request_store": "filesystem",
        },
        "ruler": {
            "alertmanager_url": "http://alertmanager:9093",
            "rule_path": "/tmp/loki/scratch",
            "storage": {"type": "local", "local": {"directory": "/etc/loki/rules"}},
            "enable_api": True,
        },
    }
    return "# AUTO-GENERATED by the Notify monitoring web admin.\n" + _dump(cfg)


def render_loki_rules(state: dict) -> str:
    rules = []
    for c in state.get("containers", []):
        if not c.get("logs"):
            continue
        patt = c.get("log_patterns", "(?i)error|exception")
        thr = c.get("log_threshold", 0)
        rules.append({
            "alert": _pascal(c["name"]) + "ErrorLog",
            "expr": ("sum by (container) (\n"
                     "  count_over_time({container=\"%s\"} |~ `%s` [5m])\n"
                     ") > %s" % (c["name"], patt, thr)),
            "for": "0m",
            "labels": {"severity": "warning"},
            "annotations": {
                "summary": "%s コンテナでエラーログを検知しました" % c["name"],
                "description": "直近5分でエラー/例外等のログ行を %s 件超検知しました。" % thr,
            },
        })
    doc = {"groups": [{"name": "llm_log_rules", "rules": rules}]} if rules else {"groups": []}
    return "# AUTO-GENERATED by the Notify monitoring web admin.\n" + _dump(doc)


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
    "alerts": ("alert.rules.yml", render_alert_rules),
    "alertmanager": ("alertmanager.yml", render_alertmanager),
    "promtail": ("promtail-config.yml", render_promtail),
    "loki": ("loki-config.yml", render_loki_config),
    "loki_rules": ("loki-rules/fake/llm-log-rules.yml", render_loki_rules),
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
