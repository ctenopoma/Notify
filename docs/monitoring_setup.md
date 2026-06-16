# vLLM / LiteLLM コンテナ & GPU 監視基盤 セットアップ手順（Prometheus / Loki / Grafana / Alertmanager + Notify）

本ドキュメントは、Ubuntu サーバー上の Docker 環境で動く **特定のコンテナ（例: vLLM と LiteLLM）** と、それらが載る **サーバーの GPU** を対象に、

- **死活監視**（コンテナが落ちた / 応答しなくなった）
- **エラーログ監視**（コンテナの標準出力にエラーが出た）
- **GPU 負荷・健全性監視**（VRAM 使用・圧迫、NVLink 帯域、サーマルスロットリング、GPU 効率）

の3軸で監視し、

- **コンテナ系のアラート**は Tauri 製デスクトップアプリ **Notify** にプッシュ通知として届け、
- **GPU 系のメトリクス**は **Grafana** で時系列に常時観察しつつ、**閾値を超えたものだけ**を同じく Notify に通知する

ための構築手順をまとめたものです。

> ⚠️ コンテナ監視（死活・ログ）は「全コンテナを一括監視する」ものではありません。**監視したいコンテナを名前で指定**し、それ以外は収集・通知の対象から外す設計です（対象の増減は最終章「10. 監視対象の追加・変更」を参照）。
> 一方、**GPU 監視は物理 GPU 単位（`gpu="0"`, `gpu="1"`, …）で行う、サーバー全体に対する監視**です。コンテナ名による絞り込みは行いません。

---

## 1. 監視の方針（この文書の前提）

### 1.1 監視するもの

| 軸 | 何を見るか | 仕組み | 通知/可視化 |
|---|---|---|---|
| 死活（推奨） | アプリが応答しているか | 各コンテナの `/metrics` を Prometheus が直接スクレイプ → `up` メトリクス | Notify |
| 死活（汎用・予備） | コンテナが存在し動いているか | cAdvisor の `container_last_seen` を**コンテナ名で絞る** | Notify |
| エラーログ | 標準出力のエラー行 | Promtail が**対象コンテナのログだけ**収集 → Loki Ruler が評価 | Notify |
| **GPU 負荷・健全性** | **VRAM / NVLink / 温度・スロットリング / 効率** | **DCGM Exporter を Prometheus がスクレイプ** | **Grafana（時系列）＋ 閾値超過時に Notify** |

### 1.2 GPU で具体的に見る3項目（ご要望の①②③）

| 監視項目 | 主に見る DCGM メトリクス | アラートの考え方 |
|---|---|---|
| ① VRAM 圧迫 / NVLink 帯域飽和 | `DCGM_FI_DEV_FB_USED/FREE/TOTAL`、`DCGM_FI_PROF_NVLINK_TX/RX_BYTES` | FB 使用率が高止まり、NVLink スループットがリンク上限に張り付く |
| ② サーマルスロットリング | `DCGM_FI_DEV_THERMAL_VIOLATION`、`DCGM_FI_DEV_GPU_TEMP` | スロットリング時間が増加（= 実際に絞られている）、温度が高い |
| ③ GPU 効率 | `DCGM_FI_PROF_GR_ENGINE_ACTIVE` / `SM_ACTIVE` / `SM_OCCUPANCY` / `PIPE_TENSOR_ACTIVE`、`DCGM_FI_DEV_POWER_USAGE` | 電力は食っているのに計算エンジンが遊んでいる（= 効率が悪い） |

> **VRAM「断片化」について（重要）**：DCGM は *断片化そのもの* を表す直接の指標を持ちません。本構成では FB（フレームバッファ）の used / free / reserved の推移で **メモリ圧迫**を捉え、加えて **すでにスクレイプしている vLLM 自身のメトリクス**（KV キャッシュ使用率 `vllm:gpu_cache_usage_perc` など）を Grafana に並べて確認します。「FB 上は空きがあるのに確保に失敗する／reserved と used の乖離が大きい」状況が、実運用上の断片化・圧迫のサインになります。

### 1.3 コンテナ監視の「指定コンテナだけ」を実現する3つの絞り込み点

死活・ログ監視で対象を限定するために編集するのは、実質この3か所だけです（GPU 監視はここには含まれません＝GPU 全体が対象）。

1. **死活（直接スクレイプ）**: `prometheus.yml` の `scrape_configs` に**対象コンテナを個別に**列挙する。
2. **ログ収集**: `promtail-config.yml` の `relabel_configs` で `action: keep` を使い、**対象コンテナ名に一致するログだけ**残す。
3. **アラート条件**: 各ルールの label セレクタ（`up{job="vllm"}` / `{container="vllm"}` 等）で**対象コンテナだけ**を評価する。

### 1.4 データの流れ

```
[ サーバーの GPU(s) ]                 [ vLLM / LiteLLM コンテナ ]
        │ NVML/DCGM                      │ /metrics(死活)   │ stdout(エラー)
        ▼                                ▼                   ▼
 [DCGM Exporter]                    [Prometheus] ← cAdvisor   [Promtail]
        │                                │  alert.rules 評価      │
        │  GPU メトリクス                  │  (死活 + GPU 閾値)      ▼
        └───────────────► [Prometheus] ◄─┘                  [Loki + Ruler]
                              │   │                               │ LogQL 評価
                  ┌───────────┘   └───────────┬───────────────────┘
                  ▼                            ▼
            [Grafana]                   [Alertmanager] ◄─ Notify が /api/v2/alerts をポーリング
   （GPU 時系列ダッシュボード）                  ▼
                                          [Notify (Windows トレイ常駐アプリ)]
```

- **Grafana**：GPU メトリクスを時系列で「眺める」場所。閾値到達前の傾向把握・事後分析に使う。
- **Alertmanager → Notify**：「閾値を超えた瞬間」を手元に届けるプッシュ通知経路。死活・ログ・GPU 閾値超過のすべてがここに集約される。

### 1.5 接続確認用ハートビート

Notify アプリは「API が応答しただけ」では接続中と判定せず、テスト用アラート **`AlwaysFiringTest`**（常時発火）を実際に受信できたときのみ「接続中」とみなします（デッドマンズスイッチ）。本構成にも最初からこのアラートを含めています（6.2 参照）。

---

## 2. 前提条件とインストール

Ubuntu サーバーに Docker および Docker Compose が必要です。

```bash
docker --version
docker compose version
```

未インストールの場合：
```bash
sudo apt update
sudo apt install -y docker.io docker-compose-v2
sudo systemctl enable --now docker
```

### 2.1 GPU 監視のための NVIDIA Container Toolkit

DCGM Exporter コンテナから GPU を参照するには、ホストに **NVIDIA ドライバ**と **NVIDIA Container Toolkit** が必要です。

```bash
# ドライバが入っていることを確認（GPU 一覧が出ればOK）
nvidia-smi

# NVIDIA Container Toolkit（未導入の場合）
curl -fsSL https://nvidia.github.io/libnvidia-container/gpgkey | \
  sudo gpg --dearmor -o /usr/share/keyrings/nvidia-container-toolkit-keyring.gpg
curl -s -L https://nvidia.github.io/libnvidia-container/stable/deb/nvidia-container-toolkit.list | \
  sed 's#deb https://#deb [signed-by=/usr/share/keyrings/nvidia-container-toolkit-keyring.gpg] https://#g' | \
  sudo tee /etc/apt/sources.list.d/nvidia-container-toolkit.list
sudo apt update && sudo apt install -y nvidia-container-toolkit

# Docker から GPU を使えるよう設定して再起動
sudo nvidia-ctk runtime configure --runtime=docker
sudo systemctl restart docker

# 動作確認（コンテナ内から GPU が見える）
docker run --rm --gpus all nvidia/cuda:12.4.1-base-ubuntu22.04 nvidia-smi
```

> 本書では監視対象の **vLLM / LiteLLM コンテナはすでに稼働している**前提です。監視スタックからそれらを `vllm:8000` のような名前で参照（スクレイプ）できるよう、**同じ Docker ネットワークに所属させる**必要があります（4.2 参照）。

---

## 3. ディレクトリ構成

作業ディレクトリ（例: `/opt/monitor`）に以下を作成します。

```text
/opt/monitor/
├── docker-compose.yml
└── config/
    ├── prometheus.yml
    ├── alert.rules.yml
    ├── alertmanager.yml
    ├── loki-config.yml
    ├── promtail-config.yml
    ├── dcgm-counters.csv          # DCGM Exporter が出力するメトリクスの定義
    ├── grafana/
    │   └── provisioning/
    │       ├── datasources/
    │       │   └── datasources.yml
    │       └── dashboards/
    │           └── dashboards.yml
    └── loki-rules/
        └── fake/                  # "fake" は Loki のデフォルトテナント
            └── llm-log-rules.yml
```

---

## 4. docker-compose.yml

死活（cAdvisor）・ログ（Promtail）・**GPU（DCGM Exporter）**・**可視化（Grafana）** を**最初から含んだ完成形**です。

```yaml
services:
  prometheus:
    image: prom/prometheus:latest
    container_name: prometheus
    volumes:
      - ./config/prometheus.yml:/etc/prometheus/prometheus.yml
      - ./config/alert.rules.yml:/etc/prometheus/alert.rules.yml
      - prometheus-data:/prometheus
    command:
      - '--config.file=/etc/prometheus/prometheus.yml'
      - '--storage.tsdb.path=/prometheus'
      - '--web.enable-lifecycle'      # /-/reload で設定リロードを許可
    ports:
      - "9090:9090"
    restart: unless-stopped
    networks: [monitor]

  alertmanager:
    image: prom/alertmanager:latest
    container_name: alertmanager
    volumes:
      - ./config/alertmanager.yml:/etc/alertmanager/alertmanager.yml
    command:
      - '--config.file=/etc/alertmanager/alertmanager.yml'
    ports:
      - "9093:9093"          # Notify アプリがここをポーリングする
    restart: unless-stopped
    networks: [monitor]

  grafana:
    image: grafana/grafana:latest
    container_name: grafana
    volumes:
      - grafana-data:/var/lib/grafana
      - ./config/grafana/provisioning:/etc/grafana/provisioning
    environment:
      - GF_SECURITY_ADMIN_USER=admin
      - GF_SECURITY_ADMIN_PASSWORD=admin   # 本番では必ず変更すること
      - GF_USERS_ALLOW_SIGN_UP=false
    ports:
      - "3000:3000"          # GPU 時系列ダッシュボードの閲覧用
    restart: unless-stopped
    networks: [monitor]

  dcgm-exporter:
    image: nvidia/dcgm-exporter:latest
    container_name: dcgm-exporter
    runtime: nvidia
    cap_add:
      - SYS_ADMIN            # NVLink/Tensor 等のプロファイリング系メトリクスに必須
    environment:
      - NVIDIA_VISIBLE_DEVICES=all
    volumes:
      - ./config/dcgm-counters.csv:/etc/dcgm-exporter/custom-counters.csv
    command: ["-f", "/etc/dcgm-exporter/custom-counters.csv"]
    ports:
      - "9400:9400"          # Prometheus がスクレイプする GPU メトリクス
    restart: unless-stopped
    networks: [monitor]

  loki:
    image: grafana/loki:latest
    container_name: loki
    volumes:
      - ./config/loki-config.yml:/etc/loki/local-config.yaml
      - ./config/loki-rules:/etc/loki/rules
    ports:
      - "3100:3100"
    command: -config.file=/etc/loki/local-config.yaml
    restart: unless-stopped
    networks: [monitor]

  promtail:
    image: grafana/promtail:latest
    container_name: promtail
    volumes:
      - ./config/promtail-config.yml:/etc/promtail/config.yml
      - /var/run/docker.sock:/var/run/docker.sock   # コンテナログの自動検出に必須
    command: -config.file=/etc/promtail/config.yml
    restart: unless-stopped
    networks: [monitor]

  cadvisor:
    image: gcr.io/cadvisor/cadvisor:latest
    container_name: cadvisor
    privileged: true
    volumes:
      - /:/rootfs:ro
      - /var/run:/var/run:ro
      - /sys:/sys:ro
      - /var/lib/docker/:/var/lib/docker:ro
      - /dev/disk/:/dev/disk:ro
    ports:
      - "8080:8080"
    restart: unless-stopped
    networks: [monitor]

volumes:
  prometheus-data:
  grafana-data:

networks:
  monitor:
    name: monitor
```

> `runtime: nvidia` が使えない環境（Compose のバージョン差異など）では、`dcgm-exporter` サービスに代わりに以下を付けても同等です。
> ```yaml
>     deploy:
>       resources:
>         reservations:
>           devices:
>             - capabilities: [gpu]
> ```

### 4.2 監視対象（vLLM / LiteLLM）をネットワークに参加させる

Prometheus が `vllm:8000` / `litellm:4000` という名前でスクレイプできるよう、**監視対象コンテナを同じ `monitor` ネットワークに所属**させます。対象側の compose（または `docker run`）に以下を追加してください。

```yaml
# vLLM / LiteLLM 側の docker-compose.yml（抜粋）
services:
  vllm:
    # ... 既存の vLLM 定義 ...
    container_name: vllm
    networks: [monitor]

  litellm:
    # ... 既存の LiteLLM 定義 ...
    container_name: litellm
    networks: [monitor]

networks:
  monitor:
    external: true        # 監視スタック側で作成した monitor ネットワークを共有
```

> `container_name` を固定しておくと、cAdvisor の `name` ラベルや Promtail の `container` ラベルが安定し、ルールのセレクタがブレません。
> なお GPU メトリクス（DCGM）は物理 GPU 単位（`gpu` ラベル）で取得され、vLLM/LiteLLM コンテナをこのネットワークに入れるかどうかとは無関係に収集されます。

---

## 5. 設定ファイル

### 5.1 `config/prometheus.yml`

Prometheus 自身・cAdvisor・**DCGM Exporter** に加え、**監視対象コンテナを個別ジョブとして列挙**します（＝絞り込み点①）。

```yaml
global:
  scrape_interval: 15s
  evaluation_interval: 15s

alerting:
  alertmanagers:
    - static_configs:
        - targets: ['alertmanager:9093']

rule_files:
  - 'alert.rules.yml'

scrape_configs:
  - job_name: 'prometheus'
    static_configs:
      - targets: ['localhost:9090']

  # コンテナ死活/リソースの汎用収集（/metrics を持たないコンテナの予備監視用）
  - job_name: 'cadvisor'
    static_configs:
      - targets: ['cadvisor:8080']

  # GPU 負荷・健全性（VRAM / NVLink / 温度・スロットリング / 効率）
  - job_name: 'dcgm'
    static_configs:
      - targets: ['dcgm-exporter:9400']

  # ▼ 監視したいコンテナを個別に列挙（ここが死活監視の主役）
  - job_name: 'vllm'
    metrics_path: /metrics
    static_configs:
      - targets: ['vllm:8000']      # vLLM の API ポート（環境に合わせて変更）

  - job_name: 'litellm'
    metrics_path: /metrics
    static_configs:
      - targets: ['litellm:4000']   # LiteLLM proxy のポート（環境に合わせて変更）
```

> - **vLLM** は `/metrics`（Prometheus 形式）を標準で公開します。KV キャッシュ使用率 `vllm:gpu_cache_usage_perc` 等は GPU メモリ圧迫の確認に有用です（Grafana に DCGM の FB メトリクスと並べて表示すると断片化・圧迫の判断がしやすくなります）。
> - **LiteLLM** は Prometheus 連携を有効化（`litellm_settings.callbacks: ["prometheus"]` 等）すると `/metrics` を公開します。出せない構成の場合は、LiteLLM の死活は 6.2 の cAdvisor 方式（`container_last_seen` / `absent`）で代替してください。

### 5.2 `config/dcgm-counters.csv`

DCGM Exporter が公開するメトリクスを明示的に定義します。NVLink・Tensor 等のプロファイリング系を確実に出すため、カスタム定義を使います（フォーマットは `フィールド名, 型, ヘルプ文`）。

```csv
# Clocks / 温度 / 電力
DCGM_FI_DEV_GPU_TEMP,            gauge, GPU temperature (C).
DCGM_FI_DEV_MEMORY_TEMP,        gauge, Memory temperature (C).
DCGM_FI_DEV_POWER_USAGE,        gauge, Power draw (W).
DCGM_FI_DEV_SM_CLOCK,           gauge, SM clock (MHz).

# スロットリング（②サーマルスロットリング）
DCGM_FI_DEV_THERMAL_VIOLATION,  counter, Throttling duration due to thermal constraints (us).
DCGM_FI_DEV_POWER_VIOLATION,    counter, Throttling duration due to power constraints (us).

# 使用率 / 効率（③GPU 効率）
DCGM_FI_DEV_GPU_UTIL,           gauge, GPU utilization (%).
DCGM_FI_PROF_GR_ENGINE_ACTIVE,  gauge, Ratio of time the graphics/compute engine is active (0-1).
DCGM_FI_PROF_SM_ACTIVE,         gauge, Ratio of cycles an SM has at least 1 warp assigned (0-1).
DCGM_FI_PROF_SM_OCCUPANCY,      gauge, Ratio of resident warps to the theoretical maximum (0-1).
DCGM_FI_PROF_PIPE_TENSOR_ACTIVE,gauge, Ratio of cycles the tensor (HMMA) pipe is active (0-1).
DCGM_FI_PROF_DRAM_ACTIVE,       gauge, Ratio of cycles the device memory interface is active (0-1).

# VRAM（①VRAM 圧迫）
DCGM_FI_DEV_FB_TOTAL,           gauge, Framebuffer memory total (MiB).
DCGM_FI_DEV_FB_FREE,            gauge, Framebuffer memory free (MiB).
DCGM_FI_DEV_FB_USED,            gauge, Framebuffer memory used (MiB).
DCGM_FI_DEV_FB_RESERVED,        gauge, Framebuffer memory reserved (MiB).

# NVLink / PCIe 帯域（①NVLink 帯域飽和）
DCGM_FI_PROF_NVLINK_TX_BYTES,   gauge, NVLink bytes transmitted (per second).
DCGM_FI_PROF_NVLINK_RX_BYTES,   gauge, NVLink bytes received (per second).
DCGM_FI_PROF_PCIE_TX_BYTES,     gauge, PCIe bytes transmitted (per second).
DCGM_FI_PROF_PCIE_RX_BYTES,     gauge, PCIe bytes received (per second).
```

> プロファイリング系（`DCGM_FI_PROF_*`）は GPU アーキテクチャや MIG 構成によって取得可否が変わります。出ない項目がある場合は CSV から外してください（その行を消すだけ）。各メトリクスには `gpu`（GPU 番号）, `Hostname`, `modelName` 等のラベルが自動付与されます。

### 5.3 `config/alert.rules.yml`

死活アラート・ハートビートに加え、**GPU 閾値アラート**を定義します。`up`/`{container=...}` で**対象コンテナだけ**を、DCGM 系は **GPU 単位（`gpu` ラベル付き）** で評価します。

```yaml
groups:
  # --- 接続確認用ハートビート（Notify アプリの「接続中」判定に使用） ---
  - name: heartbeat_rules
    rules:
      - alert: AlwaysFiringTest
        expr: vector(1)
        for: 10s
        labels:
          severity: warning
        annotations:
          summary: "Notify 接続テスト用アラート"
          description: "常時発火。Notify アプリの接続確認に使用します。"

  # --- 死活監視（推奨）: /metrics スクレイプの up で判定 ---
  - name: llm_liveness
    rules:
      - alert: VLLMDown
        expr: up{job="vllm"} == 0
        for: 1m
        labels:
          severity: critical
        annotations:
          summary: "vLLM が応答していません"
          description: "vLLM (job=vllm) のスクレイプに1分以上失敗しています。"

      - alert: LiteLLMDown
        expr: up{job="litellm"} == 0
        for: 1m
        labels:
          severity: critical
        annotations:
          summary: "LiteLLM が応答していません"
          description: "LiteLLM (job=litellm) のスクレイプに1分以上失敗しています。"

  # --- 死活監視（予備）: cAdvisor をコンテナ名で絞って判定 ---
  #     /metrics を持たないコンテナや、二重チェックしたい場合に使用
  - name: llm_container_liveness
    rules:
      # 観測が途絶えた = 停止/再起動中
      - alert: WatchedContainerStalled
        expr: time() - container_last_seen{name=~"vllm|litellm"} > 60
        labels:
          severity: critical
        annotations:
          summary: "コンテナ [{{ $labels.name }}] が60秒以上観測されていません"
          description: "停止または再起動中の可能性があります。"

      # 系列ごと消失 = 確実に停止/削除された（常時稼働すべきものに）
      - alert: VLLMContainerAbsent
        expr: absent(container_last_seen{name="vllm"})
        for: 1m
        labels: { severity: critical }
        annotations:
          summary: "vllm コンテナが存在しません"
      - alert: LiteLLMContainerAbsent
        expr: absent(container_last_seen{name="litellm"})
        for: 1m
        labels: { severity: critical }
        annotations:
          summary: "litellm コンテナが存在しません"

  # --- GPU 監視（DCGM Exporter）: 閾値超過のみ通知。傾向は Grafana で観察 ---
  - name: gpu_health
    rules:
      # ① VRAM 圧迫: FB 使用率が高止まり
      - alert: GPUMemoryHigh
        expr: (DCGM_FI_DEV_FB_USED / DCGM_FI_DEV_FB_TOTAL) * 100 > 95
        for: 5m
        labels: { severity: warning }
        annotations:
          summary: "GPU{{ $labels.gpu }} の VRAM 使用率が高い ({{ $value | printf \"%.0f\" }}%)"
          description: "FB 使用率が95%超で5分継続。OOM/断片化のリスク。vLLM の gpu_cache_usage_perc も併せて確認してください。"

      # ① NVLink 帯域飽和: TX+RX のスループットが閾値超え（環境の上限に合わせて要調整）
      #    例: 1リンクあたりの上限が25GB/s級なら、飽和の目安として 20GB/s(=20e9) を初期値に
      - alert: GPUNVLinkSaturated
        expr: (DCGM_FI_PROF_NVLINK_TX_BYTES + DCGM_FI_PROF_NVLINK_RX_BYTES) > 20e9
        for: 5m
        labels: { severity: warning }
        annotations:
          summary: "GPU{{ $labels.gpu }} の NVLink 帯域が飽和気味"
          description: "NVLink TX+RX が閾値超で5分継続。テンソル並列の通信ボトルネックの可能性。閾値はお使いの GPU のリンク帯域に合わせて調整してください。"

      # ② サーマルスロットリング: 実際に温度起因で絞られている
      - alert: GPUThermalThrottling
        expr: rate(DCGM_FI_DEV_THERMAL_VIOLATION[5m]) > 0
        for: 1m
        labels: { severity: critical }
        annotations:
          summary: "GPU{{ $labels.gpu }} がサーマルスロットリング中"
          description: "温度制約による周波数低下が継続的に発生しています。冷却/設置/電力設定を確認してください。"

      # ② 温度高（スロットリング手前の予兆）
      - alert: GPUTemperatureHigh
        expr: DCGM_FI_DEV_GPU_TEMP > 85
        for: 2m
        labels: { severity: warning }
        annotations:
          summary: "GPU{{ $labels.gpu }} の温度が高い ({{ $value }}C)"
          description: "85C超が2分継続。サーマルスロットリングに至る前の予兆です。"

      # ③ GPU 効率低下: 電力は食っているのに計算エンジンが遊んでいる
      - alert: GPULowEfficiency
        expr: |
          (avg_over_time(DCGM_FI_PROF_GR_ENGINE_ACTIVE[15m]) < 0.2)
          and
          (avg_over_time(DCGM_FI_DEV_POWER_USAGE[15m]) > 100)
        for: 15m
        labels: { severity: warning }
        annotations:
          summary: "GPU{{ $labels.gpu }} の利用効率が低い"
          description: "電力消費は高い一方で計算エンジン稼働率が15分平均20%未満。確保したまま遊んでいる/通信待ち等で効率が悪い可能性。Grafana で SM・Tensor 稼働率の内訳を確認してください。"
```

> **しきい値はあくまで初期値です。** GPU の世代・モデル・ワークロードで適正値が変わります。まず Grafana で平常時の分布を1〜2週間眺めてから、`GPUNVLinkSaturated` の `20e9`（バイト/秒）や `GPUTemperatureHigh` の `85`、`GPULowEfficiency` の `0.2` / `100W` を調整してください（調整手順は 10 章）。
>
> **`absent()` の注意点（再掲）**：cAdvisor のコンテナ系列は停止後しばらくで消えるため、常時稼働すべきコンテナには `absent()` を併用します。`absent()` は正規表現と相性が悪いので**コンテナごとに1ルール**ずつ書きます。

### 5.4 `config/alertmanager.yml`

Notify アプリが API から Pull するため、外部送信は行わない最小構成です。

```yaml
global:
  resolve_timeout: 5m

route:
  group_by: ['alertname']
  group_wait: 10s
  group_interval: 10s
  repeat_interval: 1h
  receiver: 'default-receiver'

receivers:
  - name: 'default-receiver'
```

### 5.5 `config/grafana/provisioning/datasources/datasources.yml`

Grafana 起動時に Prometheus（GPU メトリクス）と Loki（ログ）を自動でデータソース登録します。

```yaml
apiVersion: 1

datasources:
  - name: Prometheus
    type: prometheus
    access: proxy
    url: http://prometheus:9090
    isDefault: true

  - name: Loki
    type: loki
    access: proxy
    url: http://loki:3100
```

### 5.6 `config/grafana/provisioning/dashboards/dashboards.yml`

このフォルダに置いた JSON ダッシュボードを自動ロードします。

```yaml
apiVersion: 1

providers:
  - name: 'default'
    orgId: 1
    folder: ''
    type: file
    disableDeletion: false
    editable: true
    options:
      path: /etc/grafana/provisioning/dashboards
```

> 既製の GPU ダッシュボードをそのまま使うのが手軽です。Grafana 公式の **「NVIDIA DCGM Exporter Dashboard」(ID: 12239)** を、起動後に Web UI から **Dashboards → New → Import → `12239` を入力 → データソースに Prometheus を選択**でインポートできます。GPU 使用率・温度・電力・FB 使用・NVLink 等が一通り揃います。自前 JSON を恒久運用したい場合は、エクスポートした JSON をこの `dashboards/` フォルダに置けば次回以降は自動ロードされます。

### 5.7 `config/loki-config.yml`

ログ集約と、LogQL ルールを評価する Ruler を有効化します。

```yaml
auth_enabled: false

server:
  http_listen_port: 3100

common:
  path_prefix: /tmp/loki
  storage:
    filesystem:
      chunks_directory: /tmp/loki/chunks
      rules_directory: /tmp/loki/rules
  replication_factor: 1
  ring:
    kvstore:
      store: inmemory

schema_config:
  configs:
    - from: 2020-10-24
      store: tsdb
      object_store: filesystem
      schema: v11
      index:
        prefix: index_
        period: 24h

ruler:
  alertmanager_url: http://alertmanager:9093
  rule_path: /etc/loki/rules
  storage:
    type: local
    local:
      directory: /etc/loki/rules
```

### 5.8 `config/promtail-config.yml`

Docker SD で全コンテナを発見しつつ、**vLLM / LiteLLM のログだけを残す**（＝絞り込み点②）。

```yaml
server:
  http_listen_port: 9080
  grpc_listen_port: 0

positions:
  filename: /tmp/positions.yaml

clients:
  - url: http://loki:3100/loki/api/v1/push

scrape_configs:
  - job_name: docker_llm
    docker_sd_configs:
      - host: unix:///var/run/docker.sock
        refresh_interval: 5s
    relabel_configs:
      # vllm / litellm 以外のコンテナはここで捨てる（= これらだけ収集）
      - source_labels: ['__meta_docker_container_name']
        regex: '/(vllm|litellm)'
        action: keep
      # 残ったものに container ラベルを付与（先頭スラッシュを除去）
      - source_labels: ['__meta_docker_container_name']
        regex: '/(.*)'
        target_label: 'container'
```

> Docker SD のメタラベル `__meta_docker_container_name` は `/vllm` のように**先頭スラッシュ付き**です。`keep` の正規表現は `/(vllm|litellm)` とします。

### 5.9 `config/loki-rules/fake/llm-log-rules.yml`

対象コンテナのログに対するエラー検知ルール（＝絞り込み点③）。

```yaml
groups:
  - name: llm_log_rules
    rules:
      - alert: VLLMErrorLog
        expr: |
          sum by (container) (
            count_over_time({container="vllm"} |~ `(?i)error|exception|cuda|out of memory` [5m])
          ) > 0
        for: 0m
        labels: { severity: warning }
        annotations:
          summary: "vLLM コンテナでエラーログを検知しました"
          description: "直近5分でエラー/例外/CUDA OOM 等のログ行を検知しました。"

      - alert: LiteLLMErrorLog
        expr: |
          sum by (container) (
            count_over_time({container="litellm"} |~ `(?i)error|exception` [5m])
          ) > 0
        for: 0m
        labels: { severity: warning }
        annotations:
          summary: "LiteLLM コンテナでエラーログを検知しました"
          description: "直近5分でエラー/例外のログ行を検知しました。"
```

> - `{container="vllm"}` のように**ラベルセレクタで対象だけ**を見ます。
> - `|~ \`(?i)error|...\`` は大文字小文字を無視した正規表現マッチ。vLLM では `cuda` / `out of memory` を加えて GPU OOM を拾えます（DCGM の VRAM メトリクスと合わせると、メモリ圧迫→OOM の流れを前後で確認できます）。
> - `for: 0m`（1行でも即発火）はノイズが多めです。運用では `> 5`（5分で5行以上）など閾値を上げると安定します。

---

## 6. 起動と動作確認

```bash
cd /opt/monitor
docker compose up -d
```

各エンドポイントを確認します。

- **Prometheus**: `http://<サーバーIP>:9090` → Status > Targets で `vllm` / `litellm` / `cadvisor` / **`dcgm`** が **UP** か確認
- **Grafana**: `http://<サーバーIP>:3000`（初期 `admin` / `admin`、初回ログインで変更）→ データソース Prometheus / Loki が登録済みか、DCGM ダッシュボード（5.6 でインポート）に GPU が表示されるか
- **Alertmanager**: `http://<サーバーIP>:9093`
- **Alertmanager API**（Notify が読む）:
  ```bash
  curl http://localhost:9093/api/v2/alerts
  ```
  正常なら、まず `AlwaysFiringTest` を含む JSON 配列が返ります。

絞り込み・GPU 収集の動作確認：
```bash
# DCGM Exporter が GPU メトリクスを出しているか（温度・FB・NVLink が見えるか）
curl -s http://localhost:9400/metrics | grep -E 'DCGM_FI_DEV_GPU_TEMP|DCGM_FI_DEV_FB_USED|DCGM_FI_PROF_NVLINK'

# cAdvisor が対象コンテナを観測しているか
curl -s http://localhost:8080/metrics | grep 'container_last_seen' | grep -E 'name="(vllm|litellm)"'

# Loki のルールが読み込まれているか
curl -s http://localhost:3100/loki/api/v1/rules

# 死活テスト: 対象コンテナを止めて 1〜2 分待つ
docker stop vllm     # → VLLMDown / VLLMContainerAbsent が発火し Notify に届く
docker start vllm    # → 復旧（Notify に [RESOLVED] 通知）

# エラーログテスト: 対象コンテナの stdout に ERROR を出す
docker exec vllm sh -c 'echo "ERROR test from vllm" 1>&2'   # → VLLMErrorLog が発火

# GPU 閾値テスト: 負荷をかけて温度/使用率を上げ、Grafana で推移を確認
#   （閾値超過時に GPUTemperatureHigh / GPUMemoryHigh 等が Notify に届く）
nvidia-smi -l 1      # 別端末で温度・使用率の上昇をリアルタイム確認
```

---

## 7. Notify アプリ側の設定

1. Notify の「設定」タブを開く。
2. **Alertmanager API URL** に `http://<UbuntuサーバーIP>:9093` を入力（複数の Alertmanager があれば「URLを追加」で並列監視・重複排除）。
3. **テスト用アラート名（ハートビート）** が `AlwaysFiringTest` になっていることを確認（5.3 のルール名と一致させる）。
4. 「設定を保存」。各サーバーの状態が「接続中」になれば、`/metrics` 死活・cAdvisor 死活・ログ検知・**GPU 閾値超過**の各アラートがそのまま Notify に届きます。

> **役割分担の整理**：日々の GPU 負荷の「傾向」は **Grafana** で眺め、「閾値を超えた異常」だけが **Notify** にプッシュされます。Grafana を常時開いておく必要はなく、Notify の通知を起点に Grafana を開いて原因を深掘りする運用が基本です。

---

## 8. ファイアウォール (UFW)

Notify が動く Windows PC から Alertmanager のポート `9093` へ、ダッシュボードを見る PC から Grafana のポート `3000` へアクセスを許可します。

```bash
# Notify 用（Alertmanager API）
sudo ufw allow from 192.168.1.50 to any port 9093 proto tcp
# Grafana 閲覧用
sudo ufw allow from 192.168.1.0/24 to any port 3000 proto tcp

sudo ufw enable
sudo ufw status
```

> Prometheus(9090) / DCGM(9400) / Loki(3100) / cAdvisor(8080) は基本的に**監視スタック内部からの参照のみ**で十分です。外部から直接見る必要がなければ、ファイアウォールで開けないでください（必要な場合のみ管理 PC に限定して許可）。

---

## 9. トラブルシューティング（GPU 監視）

| 症状 | 主な原因と対処 |
|---|---|
| `dcgm` ターゲットが DOWN | `runtime: nvidia` または NVIDIA Container Toolkit 未設定。`docker run --rm --gpus all nvidia/cuda:... nvidia-smi` が通るか確認（2.1）。 |
| `DCGM_FI_PROF_*`（NVLink/Tensor 等）が出ない | プロファイリング系は `cap_add: SYS_ADMIN` が必須。また GPU 世代/MIG 構成で非対応のことがある。出ない行は `dcgm-counters.csv` から削除。 |
| NVLink メトリクスが常に 0 | そもそも NVLink を持たない構成（単一 GPU / PCIe のみ）。その場合は `DCGM_FI_PROF_PCIE_*` で PCIe 帯域を見る。 |
| 温度は高いのにスロットリングが出ない | `DCGM_FI_DEV_THERMAL_VIOLATION` は累積カウンタ。`rate(...[5m]) > 0` で「いま絞られているか」を判定している。閾値手前の予兆は `GPUTemperatureHigh` で拾う。 |
| Grafana にデータが出ない | データソース URL（`http://prometheus:9090` / `http://loki:3100`）は **compose 内のサービス名**で解決される。同じ `monitor` ネットワークにいるか確認。 |

### 9.1 `dcgm-exporter` のログに `skipping ... (DCGM_FI_PROF_xxx): dcp metrics not enabled` と出る場合

PROF 系（`DCGM_FI_PROF_*`）は DCGM の **DCP（プロファイリング）モジュール**経由で取得します。このログは DCP モジュールが有効化できておらず、PROF 系メトリクスだけが収集対象から外れている状態を示します（DEV 系のクロック・温度・FB などは通常そのまま出ます）。原因は主に次の5つです。上から順に確認してください。

1. **`SYS_ADMIN` capability が無い**
   プロファイリング系は `cap_add: SYS_ADMIN` が無いと DCP モジュールを初期化できません。4章の `docker-compose.yml` の `dcgm-exporter` サービスに `cap_add: [SYS_ADMIN]` が入っているか確認し、無ければ追加して再起動します。
   ```bash
   docker compose up -d --force-recreate dcgm-exporter
   docker logs dcgm-exporter | grep -i -E 'profil|dcp'
   ```

2. **GPU がプロファイリング（DCP）非対応**
   DCP はデータセンタ向け GPU（Tesla/Volta 以降の T4・V100・A100・H100・**H200** 等）が対象で、**GeForce/RTX などコンシューマ向け GPU は基本的に非対応**です。コンテナ内から確認できます。
   ```bash
   docker exec dcgm-exporter dcgmi discovery -l   # GPU 一覧とモデル名を確認
   docker exec dcgm-exporter dcgmi profile --list  # 対応していなければエラーまたは空で返る
   ```
   非対応の GPU であれば PROF 系の収集は不可能なので、`dcgm-counters.csv` から `DCGM_FI_PROF_*` の行を削除し、DEV 系メトリクス（`DCGM_FI_DEV_GPU_UTIL` など）で代替してください。
   **H200 自体は DCP に対応しているため、このケースには該当しません。**H200 では次の3・4を先に疑ってください。

3. **MIG（Multi-Instance GPU）構成によりプロファイリングが使えない**
   MIG を有効化している場合、GPU インスタンスの分割サイズによってはプロファイリングが提供されません。`dcgmi discovery -c` で MIG 構成を確認し、必要であれば該当 GPU を MIG 無効（フルGPU）に戻すか、対応プロファイルに変更してください。
   ```bash
   nvidia-smi -q -d MIG | grep "MIG Mode"   # ホスト側で MIG が Disabled か確認
   ```

4. **（H200 / NVSwitch 構成）NVIDIA Fabric Manager が起動していない**
   **H200 を含む NVSwitch 搭載のマルチGPUサーバー（HGX 系の8GPU構成など）では、ホスト側で `nv-fabricmanager` デーモンが必須**です。これが起動していないと NVSwitch ファブリックが初期化されず、DCGM の Profiling モジュール自体が読み込めずに PROF 系が丸ごと skip されることがあります。コンテナの `cap_add: SYS_ADMIN` とは別物（ホスト側のサービス）なので、まずホストで確認してください。
   ```bash
   systemctl status nvidia-fabricmanager     # ホスト上で active (running) か確認
   nvidia-smi -q | grep -A2 "Fabric"         # State が "Completed" になっているか確認
   ```
   未導入の場合は、ドライバと同バージョンの `nvidia-fabricmanager` パッケージをホストに入れて起動してください（単体の H200 で NVLink/NVSwitch を使わない構成では対象外です）。

5. **イメージ内にプロファイリング用ライブラリ（`libdcgmmoduleprofiling.so.4`）が同梱されていない**
   Docker Hub の `nvidia/dcgm-exporter` イメージは、ビルド時に `datacenter-gpu-manager-4-core` を `--no-install-recommends` でインストールしており、推奨パッケージ扱いの `datacenter-gpu-manager-4-proprietary`（プロファイリングモジュール本体）が一部のビルドで欠落していたことが報告されています（[NVIDIA/dcgm-exporter#449](https://github.com/NVIDIA/dcgm-exporter/issues/449)、PR #456 で修正）。`cap_add: SYS_ADMIN` あり・GPU は DCP 対応・MIG 無効・NVSwitch なし（Fabric Manager 不要）の条件を満たしていても `DCP metrics not enabled` が出る場合は、まずこれを疑ってください。
   ```bash
   # コンテナ内にプロファイリングモジュールの .so が存在するか確認
   docker exec dcgm-exporter find / -iname "*moduleprofiling*" 2>/dev/null
   # 何も出力されなければ欠落が濃厚
   ```
   対処は次のいずれかです。
   ```bash
   # A: イメージを強制再取得して修正済みビルドに更新
   docker compose pull dcgm-exporter
   docker compose up -d --force-recreate dcgm-exporter

   # B（より確実）: NVIDIA NGC の公式イメージに切り替える
   #   docker-compose.yml の dcgm-exporter.image を変更
   #   image: nvcr.io/nvidia/k8s/dcgm-exporter:3.3.9-3.6.1-ubuntu22.04
   docker compose up -d --force-recreate dcgm-exporter
   ```

> 上記のいずれにも該当しないにもかかわらず解決しない場合は、ホストの NVIDIA ドライバと `dcgm-exporter` イメージ内の DCGM ライブラリのバージョン不整合が疑われます。H200 は比較的新しい GPU のため、古い `dcgm-exporter` イメージタグや古いドライバでは Profiling モジュールが対応していないことがあります。`nvidia-smi` のドライババージョンと、`docker exec dcgm-exporter dcgmi -v` のバージョンを確認し、`dcgm-exporter` を最新タグ（または使用ドライバに対応したタグ）に更新してください。

---

## 10. 監視対象・しきい値の追加・変更

### 10.1 コンテナを増やす／減らす

新しいコンテナ（例: `myapp`）を死活・ログ監視に加えるときは、**前述の3つの絞り込み点だけ**を編集します。

| 監視内容 | 編集ファイル | 追記内容 |
|---|---|---|
| 死活（推奨） | `prometheus.yml` ＋ `alert.rules.yml` | `job_name: 'myapp'` を `scrape_configs` に追加し、`up{job="myapp"} == 0` ルールを追加 |
| 死活（予備） | `alert.rules.yml` | `container_last_seen` の正規表現に追加（`name=~"vllm\|litellm\|myapp"`）＋ `absent(... name="myapp")` ルール |
| エラーログ | `promtail-config.yml` ＋ Loki ルール | `keep` の正規表現に追加（`/(vllm\|litellm\|myapp)`）＋ `{container="myapp"}` ルールを追加 |

### 10.2 GPU のしきい値を調整する

GPU 監視は GPU 全体が対象のため、対象の増減ではなく**しきい値の調整**が中心になります。編集するのは `alert.rules.yml` の `gpu_health` グループだけです。

| 調整したいこと | 編集箇所 | 目安 |
|---|---|---|
| VRAM 圧迫の感度 | `GPUMemoryHigh` の `> 95` | OOM 余裕を持たせたいなら 90 などに下げる |
| NVLink 飽和の閾値 | `GPUNVLinkSaturated` の `> 20e9` | お使いの GPU の片方向リンク帯域の 7〜8 割（バイト/秒）を目安に |
| 温度の予兆 | `GPUTemperatureHigh` の `> 85` | データセンタGPUは 85〜90C、コンシューマは仕様に合わせる |
| 効率低下の判定 | `GPULowEfficiency` の `< 0.2` / `> 100`（W） | Grafana の平常時分布を見て、明らかに低い水準に設定 |

> **進め方の推奨**：まず Grafana（DCGM ダッシュボード）で平常時〜ピーク時の値を1〜2週間観察し、誤検知が出ない水準に上記しきい値を寄せてから本運用に入ると安定します。

### 10.3 設定の反映

```bash
# Prometheus に設定リロードを通知（--web.enable-lifecycle 有効時）
#   prometheus.yml / alert.rules.yml の変更はこれで反映
curl -X POST http://localhost:9090/-/reload

# Promtail / Loki / DCGM / Grafana の設定変更は再起動で反映
docker compose restart promtail loki dcgm-exporter grafana
```

逆に**監視を外す**ときは、同じ箇所から対象名やルールを削除するだけです。これにより、コンテナ監視は常に「**指定したコンテナだけ**」、GPU 監視は「**サーバー全 GPU を一貫したしきい値で**」見る状態を保てます。
