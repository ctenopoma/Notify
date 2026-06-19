"""Static catalogs used by the web admin UI.

These describe *what can be configured*: the DCGM metric definitions, the
node/CPU/memory & container resource metrics, and ready-made alert-rule
templates. The UI fetches these via /api/catalog so users can pick from them
rather than hand-writing YAML. Everything here mirrors docs/monitoring_setup.md.
"""

# --- DCGM Exporter counters (config/dcgm-counters.csv) -----------------------
# Each entry: field name, prometheus type, help text, and a coarse "group" so
# the UI can present them in sensible sections. `prof` marks profiling-only
# fields (DCGM_FI_PROF_*) which need cap_add: SYS_ADMIN and a DCP-capable GPU.
DCGM_COUNTERS = [
    # Clocks / temperature / power
    {"field": "DCGM_FI_DEV_GPU_TEMP", "type": "gauge", "help": "GPU temperature (C).", "group": "温度/電力", "prof": False, "default": True},
    {"field": "DCGM_FI_DEV_MEMORY_TEMP", "type": "gauge", "help": "Memory temperature (C).", "group": "温度/電力", "prof": False, "default": True},
    {"field": "DCGM_FI_DEV_POWER_USAGE", "type": "gauge", "help": "Power draw (W).", "group": "温度/電力", "prof": False, "default": True},
    {"field": "DCGM_FI_DEV_SM_CLOCK", "type": "gauge", "help": "SM clock (MHz).", "group": "温度/電力", "prof": False, "default": True},
    # Throttling (thermal/power violations)
    {"field": "DCGM_FI_DEV_THERMAL_VIOLATION", "type": "counter", "help": "Throttling duration due to thermal constraints (us).", "group": "スロットリング", "prof": False, "default": True},
    {"field": "DCGM_FI_DEV_POWER_VIOLATION", "type": "counter", "help": "Throttling duration due to power constraints (us).", "group": "スロットリング", "prof": False, "default": True},
    # Utilisation / efficiency
    {"field": "DCGM_FI_DEV_GPU_UTIL", "type": "gauge", "help": "GPU utilization (%).", "group": "使用率/効率", "prof": False, "default": True},
    {"field": "DCGM_FI_PROF_GR_ENGINE_ACTIVE", "type": "gauge", "help": "Ratio of time the graphics/compute engine is active (0-1).", "group": "使用率/効率", "prof": True, "default": True},
    {"field": "DCGM_FI_PROF_SM_ACTIVE", "type": "gauge", "help": "Ratio of cycles an SM has at least 1 warp assigned (0-1).", "group": "使用率/効率", "prof": True, "default": True},
    {"field": "DCGM_FI_PROF_SM_OCCUPANCY", "type": "gauge", "help": "Ratio of resident warps to the theoretical maximum (0-1).", "group": "使用率/効率", "prof": True, "default": True},
    {"field": "DCGM_FI_PROF_PIPE_TENSOR_ACTIVE", "type": "gauge", "help": "Ratio of cycles the tensor (HMMA) pipe is active (0-1).", "group": "使用率/効率", "prof": True, "default": True},
    {"field": "DCGM_FI_PROF_DRAM_ACTIVE", "type": "gauge", "help": "Ratio of cycles the device memory interface is active (0-1).", "group": "使用率/効率", "prof": True, "default": True},
    # VRAM / framebuffer
    {"field": "DCGM_FI_DEV_FB_TOTAL", "type": "gauge", "help": "Framebuffer memory total (MiB).", "group": "VRAM", "prof": False, "default": True},
    {"field": "DCGM_FI_DEV_FB_FREE", "type": "gauge", "help": "Framebuffer memory free (MiB).", "group": "VRAM", "prof": False, "default": True},
    {"field": "DCGM_FI_DEV_FB_USED", "type": "gauge", "help": "Framebuffer memory used (MiB).", "group": "VRAM", "prof": False, "default": True},
    {"field": "DCGM_FI_DEV_FB_RESERVED", "type": "gauge", "help": "Framebuffer memory reserved (MiB).", "group": "VRAM", "prof": False, "default": True},
    # NVLink / PCIe bandwidth
    {"field": "DCGM_FI_PROF_NVLINK_TX_BYTES", "type": "gauge", "help": "NVLink bytes transmitted (per second).", "group": "NVLink/PCIe", "prof": True, "default": True},
    {"field": "DCGM_FI_PROF_NVLINK_RX_BYTES", "type": "gauge", "help": "NVLink bytes received (per second).", "group": "NVLink/PCIe", "prof": True, "default": True},
    {"field": "DCGM_FI_PROF_PCIE_TX_BYTES", "type": "gauge", "help": "PCIe bytes transmitted (per second).", "group": "NVLink/PCIe", "prof": True, "default": True},
    {"field": "DCGM_FI_PROF_PCIE_RX_BYTES", "type": "gauge", "help": "PCIe bytes received (per second).", "group": "NVLink/PCIe", "prof": True, "default": True},
]


# --- Metric "sources" the UI can suggest in the alert/expr builder -----------
# Helps users compose conditions without memorising metric names.
METRIC_SOURCES = {
    "gpu": [c["field"] for c in DCGM_COUNTERS],
    "host_cpu": [
        "node_cpu_seconds_total",
        "node_load1",
        "node_load5",
    ],
    "host_memory": [
        "node_memory_MemTotal_bytes",
        "node_memory_MemAvailable_bytes",
        "node_memory_SwapFree_bytes",
    ],
    "host_disk": [
        "node_filesystem_avail_bytes",
        "node_filesystem_size_bytes",
        "node_disk_io_time_seconds_total",
    ],
    "container": [
        "container_cpu_usage_seconds_total",
        "container_memory_usage_bytes",
        "container_memory_working_set_bytes",
        "container_spec_memory_limit_bytes",
        "container_last_seen",
        "container_network_receive_bytes_total",
        "container_network_transmit_bytes_total",
    ],
    "scrape": ["up"],
}


# --- Alert-rule templates ----------------------------------------------------
# `{{...}}` Go-template label refs are kept verbatim; `${name}` placeholders are
# substituted by the UI with user-supplied threshold values before saving.
ALERT_TEMPLATES = [
    # GPU --------------------------------------------------------------------
    {
        "key": "GPUMemoryHigh", "group": "gpu_health", "category": "GPU",
        "name": "GPUMemoryHigh", "severity": "warning", "for": "5m",
        "params": [{"name": "threshold", "label": "FB使用率(%)", "default": "95"}],
        "expr": "(DCGM_FI_DEV_FB_USED / DCGM_FI_DEV_FB_TOTAL) * 100 > ${threshold}",
        "summary": "GPU{{ $labels.gpu }} の VRAM 使用率が高い ({{ $value | printf \"%.0f\" }}%)",
        "description": "FB 使用率が閾値超で5分継続。OOM/断片化のリスク。vLLM の gpu_cache_usage_perc も併せて確認してください。",
    },
    {
        "key": "GPUNVLinkSaturated", "group": "gpu_health", "category": "GPU",
        "name": "GPUNVLinkSaturated", "severity": "warning", "for": "5m",
        "params": [{"name": "threshold", "label": "TX+RX バイト/秒", "default": "20e9"}],
        "expr": "(DCGM_FI_PROF_NVLINK_TX_BYTES + DCGM_FI_PROF_NVLINK_RX_BYTES) > ${threshold}",
        "summary": "GPU{{ $labels.gpu }} の NVLink 帯域が飽和気味",
        "description": "NVLink TX+RX が閾値超で5分継続。テンソル並列の通信ボトルネックの可能性。閾値は GPU のリンク帯域に合わせて調整してください。",
    },
    {
        "key": "GPUThermalThrottling", "group": "gpu_health", "category": "GPU",
        "name": "GPUThermalThrottling", "severity": "critical", "for": "1m",
        "params": [],
        "expr": "rate(DCGM_FI_DEV_THERMAL_VIOLATION[5m]) > 0",
        "summary": "GPU{{ $labels.gpu }} がサーマルスロットリング中",
        "description": "温度制約による周波数低下が継続的に発生しています。冷却/設置/電力設定を確認してください。",
    },
    {
        "key": "GPUTemperatureHigh", "group": "gpu_health", "category": "GPU",
        "name": "GPUTemperatureHigh", "severity": "warning", "for": "2m",
        "params": [{"name": "threshold", "label": "温度(C)", "default": "85"}],
        "expr": "DCGM_FI_DEV_GPU_TEMP > ${threshold}",
        "summary": "GPU{{ $labels.gpu }} の温度が高い ({{ $value }}C)",
        "description": "閾値超が2分継続。サーマルスロットリングに至る前の予兆です。",
    },
    {
        "key": "GPULowEfficiency", "group": "gpu_health", "category": "GPU",
        "name": "GPULowEfficiency", "severity": "warning", "for": "15m",
        "params": [
            {"name": "active", "label": "engine稼働率(下限)", "default": "0.2"},
            {"name": "power", "label": "電力(W,下限)", "default": "100"},
        ],
        "expr": "(avg_over_time(DCGM_FI_PROF_GR_ENGINE_ACTIVE[15m]) < ${active})\nand\n(avg_over_time(DCGM_FI_DEV_POWER_USAGE[15m]) > ${power})",
        "summary": "GPU{{ $labels.gpu }} の利用効率が低い",
        "description": "電力消費は高い一方で計算エンジン稼働率が15分平均で閾値未満。確保したまま遊んでいる/通信待ち等で効率が悪い可能性。Grafana で SM・Tensor 稼働率の内訳を確認してください。",
    },
    # Host CPU / memory / disk (node-exporter) -------------------------------
    {
        "key": "HostHighCPU", "group": "resource_health", "category": "ホスト",
        "name": "HostHighCPU", "severity": "warning", "for": "5m",
        "params": [{"name": "threshold", "label": "CPU使用率(%)", "default": "90"}],
        "expr": "100 - (avg by (instance) (rate(node_cpu_seconds_total{mode=\"idle\"}[5m])) * 100) > ${threshold}",
        "summary": "ホスト {{ $labels.instance }} の CPU 使用率が高い ({{ $value | printf \"%.0f\" }}%)",
        "description": "CPU 使用率が閾値超で5分継続。",
    },
    {
        "key": "HostHighMemory", "group": "resource_health", "category": "ホスト",
        "name": "HostHighMemory", "severity": "warning", "for": "5m",
        "params": [{"name": "threshold", "label": "メモリ使用率(%)", "default": "90"}],
        "expr": "(1 - (node_memory_MemAvailable_bytes / node_memory_MemTotal_bytes)) * 100 > ${threshold}",
        "summary": "ホスト {{ $labels.instance }} のメモリ使用率が高い ({{ $value | printf \"%.0f\" }}%)",
        "description": "メモリ使用率が閾値超で5分継続。",
    },
    {
        "key": "HostLowDisk", "group": "resource_health", "category": "ホスト",
        "name": "HostLowDisk", "severity": "critical", "for": "5m",
        "params": [{"name": "threshold", "label": "空き(%)を下回ったら", "default": "10"}],
        "expr": "(node_filesystem_avail_bytes{fstype!~\"tmpfs|overlay\"} / node_filesystem_size_bytes{fstype!~\"tmpfs|overlay\"}) * 100 < ${threshold}",
        "summary": "ホスト {{ $labels.instance }} のディスク空きが少ない ({{ $labels.mountpoint }})",
        "description": "ファイルシステム空き容量が閾値未満です。ログ/メトリクスの保持期間を見直してください。",
    },
    # Container CPU / memory (cAdvisor) --------------------------------------
    {
        "key": "ContainerHighCPU", "group": "resource_health", "category": "コンテナ",
        "name": "ContainerHighCPU", "severity": "warning", "for": "5m",
        "params": [
            {"name": "container", "label": "コンテナ名(正規表現)", "default": "vllm|litellm"},
            {"name": "threshold", "label": "コア使用数(>)", "default": "0.9"},
        ],
        "expr": "sum by (name) (rate(container_cpu_usage_seconds_total{name=~\"${container}\"}[5m])) > ${threshold}",
        "summary": "コンテナ {{ $labels.name }} の CPU 使用が高い ({{ $value | printf \"%.2f\" }} cores)",
        "description": "コンテナの CPU 使用が閾値超で5分継続。",
    },
    {
        "key": "ContainerHighMemory", "group": "resource_health", "category": "コンテナ",
        "name": "ContainerHighMemory", "severity": "warning", "for": "5m",
        "params": [
            {"name": "container", "label": "コンテナ名(正規表現)", "default": "vllm|litellm"},
            {"name": "threshold", "label": "対上限の使用率(%)", "default": "90"},
        ],
        "expr": "(container_memory_working_set_bytes{name=~\"${container}\"} / container_spec_memory_limit_bytes{name=~\"${container}\"}) * 100 > ${threshold}",
        "summary": "コンテナ {{ $labels.name }} のメモリ使用率が高い ({{ $value | printf \"%.0f\" }}%)",
        "description": "メモリ上限に対する使用率が閾値超で5分継続。memory limit 未設定のコンテナでは評価されません。",
    },
]


def default_dcgm_counters():
    """Counters selected by default in a fresh config."""
    return [
        {"field": c["field"], "type": c["type"], "help": c["help"]}
        for c in DCGM_COUNTERS if c["default"]
    ]
