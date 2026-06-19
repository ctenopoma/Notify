"""Notify monitoring — web admin backend.

A small FastAPI app that:
  * serves the hand-written JS single-page UI (static/),
  * reads/writes the source-of-truth monitor-config.json,
  * regenerates every monitoring config file (generator.py),
  * inspects the host on startup (running containers, exposed /metrics,
    DCGM metrics actually available) so the UI can guide config choices,
  * drives docker / docker compose (start, restart, up, reload).

Runs in its own container with /var/run/docker.sock and the monitor directory
bind-mounted at the same absolute path as on the host (so `docker compose`'s
relative bind paths resolve correctly on the daemon side).
"""
from __future__ import annotations

import json
import os
import shutil
import subprocess
import urllib.request
from pathlib import Path

from fastapi import FastAPI, HTTPException
from fastapi.responses import FileResponse, JSONResponse
from fastapi.staticfiles import StaticFiles
from pydantic import BaseModel

import catalog
import generator

MONITOR_DIR = Path(os.environ.get("MONITOR_DIR", "/opt/monitor"))   # in-container config root
# Host path of the monitor dir, used as `docker compose --project-directory` so the
# daemon resolves ./config bind mounts against the real host filesystem.
HOST_MONITOR_DIR = os.environ.get("HOST_MONITOR_DIR", str(MONITOR_DIR))
CONFIG_DIR = MONITOR_DIR / "config"
COMPOSE_FILE = MONITOR_DIR / "docker-compose.yml"   # container-readable path passed to -f
STATIC_DIR = Path(__file__).parent / "static"

# Stack services we manage (for the compose/service controls).
STACK_SERVICES = [
    "prometheus", "alertmanager", "grafana", "dcgm-exporter",
    "node-exporter", "loki", "promtail", "cadvisor", "admin",
]
# Changing these files only needs a Prometheus reload, not a restart.
RELOAD_ONLY = {"prometheus.yml", "alert.rules.yml"}

app = FastAPI(title="Notify Monitoring Admin")


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------
def run(cmd: list[str], timeout: int = 120) -> dict:
    """Run a command, capturing output. Never raises on non-zero exit."""
    try:
        p = subprocess.run(cmd, capture_output=True, text=True, timeout=timeout)
        return {"cmd": " ".join(cmd), "code": p.returncode,
                "stdout": p.stdout, "stderr": p.stderr,
                "ok": p.returncode == 0}
    except subprocess.TimeoutExpired:
        return {"cmd": " ".join(cmd), "code": -1, "stdout": "",
                "stderr": "timeout after %ss" % timeout, "ok": False}
    except FileNotFoundError as e:
        return {"cmd": " ".join(cmd), "code": -1, "stdout": "",
                "stderr": str(e), "ok": False}


def compose(args: list[str], timeout: int = 300) -> dict:
    return run(["docker", "compose", "--project-directory", HOST_MONITOR_DIR,
                "-f", str(COMPOSE_FILE)] + args, timeout=timeout)


def http_get(url: str, timeout: float = 2.0) -> str | None:
    try:
        with urllib.request.urlopen(url, timeout=timeout) as r:  # noqa: S310
            return r.read().decode("utf-8", "replace")
    except Exception:
        return None


def load_state() -> dict:
    return generator.load_state(CONFIG_DIR)


# ---------------------------------------------------------------------------
# Catalog & state
# ---------------------------------------------------------------------------
@app.get("/api/catalog")
def get_catalog():
    return {
        "dcgm_counters": catalog.DCGM_COUNTERS,
        "metric_sources": catalog.METRIC_SOURCES,
        "alert_templates": catalog.ALERT_TEMPLATES,
    }


@app.get("/api/state")
def get_state():
    return load_state()


class StatePut(BaseModel):
    state: dict
    write_files: bool = True


@app.put("/api/state")
def put_state(body: StatePut):
    state = body.state
    generator.save_state(state, CONFIG_DIR)
    written = []
    if body.write_files:
        written = generator.write_all(state, CONFIG_DIR, MONITOR_DIR)
    return {"ok": True, "written": written}


@app.get("/api/preview")
def preview():
    """Render all files in-memory from current saved state (no disk writes)."""
    return generator.render_all(load_state(), MONITOR_DIR)


# ---------------------------------------------------------------------------
# Discovery — what CAN be monitored, inspected live on demand / at UI startup.
# ---------------------------------------------------------------------------
@app.get("/api/discover")
def discover():
    containers = _list_containers()

    # Which monitored targets currently expose a parseable /metrics endpoint.
    state = load_state()
    metrics_probe = []
    for c in state.get("containers", []):
        if not c.get("scrape"):
            continue
        url = "http://%s%s" % (c["target"], c.get("metrics_path", "/metrics"))
        body = http_get(url)
        metrics_probe.append({
            "name": c["name"], "url": url,
            "reachable": body is not None,
            "sample_lines": _metric_names(body)[:8] if body else [],
        })

    # DCGM fields actually present (so the UI can warn about PROF metrics that
    # this GPU/driver does not provide — see docs §9).
    dcgm_body = http_get("http://dcgm-exporter:9400/metrics")
    dcgm_available = sorted({n for n in _metric_names(dcgm_body) if n.startswith("DCGM_")}) if dcgm_body else []

    node_up = http_get("http://node-exporter:9100/metrics") is not None
    cadvisor_up = http_get("http://cadvisor:8080/healthz") is not None

    return {
        "containers": containers,
        "metrics_probe": metrics_probe,
        "dcgm_available": dcgm_available,
        "dcgm_reachable": dcgm_body is not None,
        "node_exporter_up": node_up,
        "cadvisor_up": cadvisor_up,
        "docker_ok": shutil.which("docker") is not None,
    }


def _metric_names(body: str | None) -> list[str]:
    if not body:
        return []
    names = []
    seen = set()
    for line in body.splitlines():
        line = line.strip()
        if not line or line.startswith("#"):
            continue
        name = line.split("{", 1)[0].split(" ", 1)[0]
        if name and name not in seen:
            seen.add(name)
            names.append(name)
    return names


def _list_containers() -> list[dict]:
    res = run(["docker", "ps", "-a", "--format", "{{json .}}"])
    if not res["ok"]:
        return []
    out = []
    for line in res["stdout"].splitlines():
        line = line.strip()
        if not line:
            continue
        try:
            d = json.loads(line)
        except json.JSONDecodeError:
            continue
        out.append({
            "name": d.get("Names", ""),
            "image": d.get("Image", ""),
            "state": d.get("State", ""),
            "status": d.get("Status", ""),
            "ports": d.get("Ports", ""),
        })
    return out


@app.get("/api/containers")
def containers():
    return {"containers": _list_containers()}


# ---------------------------------------------------------------------------
# Actions: container & compose control, config apply.
# ---------------------------------------------------------------------------
class ContainerAction(BaseModel):
    name: str
    action: str  # start | stop | restart


@app.post("/api/container/action")
def container_action(body: ContainerAction):
    if body.action not in ("start", "stop", "restart"):
        raise HTTPException(400, "invalid action")
    if not body.name or "/" in body.name or " " in body.name:
        raise HTTPException(400, "invalid container name")
    return run(["docker", body.action, body.name])


class ComposeAction(BaseModel):
    action: str           # up | down | restart | pull | reload
    services: list[str] = []


@app.post("/api/compose/action")
def compose_action(body: ComposeAction):
    services = [s for s in body.services if s in STACK_SERVICES]
    if body.action == "up":
        return compose(["up", "-d"] + services)
    if body.action == "down":
        return compose(["down"])
    if body.action == "restart":
        return compose(["restart"] + services)
    if body.action == "pull":
        return compose(["pull"] + services)
    if body.action == "recreate":
        return compose(["up", "-d", "--force-recreate"] + services)
    raise HTTPException(400, "invalid action")


@app.post("/api/apply")
def apply():
    """Regenerate every config file, then reload/restart only what's needed."""
    state = load_state()
    written = generator.write_all(state, CONFIG_DIR, MONITOR_DIR)

    steps = []
    # 1) Prometheus: hot-reload via POST (no restart needed for prometheus.yml/alerts).
    steps.append(_post("http://prometheus:9090/-/reload", "prometheus reload"))

    # 2) Loki ruler picks up rule files via its API reload.
    steps.append(_post("http://loki:3100/loki/api/v1/rules", "loki rules check", method="GET"))

    # 3) Restart services whose *config files* changed and have no hot reload.
    restart = compose(["restart", "promtail", "loki", "dcgm-exporter"])
    steps.append({"step": "restart promtail/loki/dcgm", **restart})

    return {"ok": all(s.get("ok", True) for s in steps), "written": written, "steps": steps}


@app.post("/api/reload-prometheus")
def reload_prometheus():
    return _post("http://prometheus:9090/-/reload", "prometheus reload")


def _post(url: str, label: str, method: str = "POST") -> dict:
    try:
        req = urllib.request.Request(url, method=method)  # noqa: S310
        with urllib.request.urlopen(req, timeout=5) as r:  # noqa: S310
            return {"step": label, "ok": True, "code": r.status}
    except Exception as e:  # noqa: BLE001
        return {"step": label, "ok": False, "error": str(e)}


@app.get("/api/logs/{service}")
def service_logs(service: str, tail: int = 200):
    if service not in STACK_SERVICES:
        raise HTTPException(400, "unknown service")
    return compose(["logs", "--no-color", "--tail", str(tail), service], timeout=30)


# ---------------------------------------------------------------------------
# Static UI
# ---------------------------------------------------------------------------
@app.get("/")
def index():
    return FileResponse(STATIC_DIR / "index.html")


@app.get("/healthz")
def healthz():
    return JSONResponse({"ok": True})


app.mount("/static", StaticFiles(directory=str(STATIC_DIR)), name="static")
