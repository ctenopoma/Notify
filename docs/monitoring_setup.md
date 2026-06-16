# vLLM / LiteLLM コンテナ監視基盤 セットアップ手順（Prometheus / Loki / Alertmanager + Notify）

本ドキュメントは、Ubuntu サーバー上の Docker 環境で動く **特定のコンテナ（例: vLLM と LiteLLM）だけ** を対象に、

- **死活監視**（コンテナが落ちた / 応答しなくなった）
- **エラーログ監視**（コンテナの標準出力にエラーが出た）

の2軸で監視し、結果を Tauri 製デスクトップアプリ **Notify** にプッシュ通知として届けるための構築手順をまとめたものです。

> ⚠️ 本構成は「全コンテナを一括で監視する」ものではありません。**監視したいコンテナを名前で指定**し、それ以外は収集・通知の対象から外すことを前提に設計しています。対象を増やす／変える方法は最終章「9. 監視対象の追加・変更」を参照してください。

---

## 1. 監視の方針（この文書の前提）

### 1.1 監視するもの
| 軸 | 何を見るか | 仕組み |
|---|---|---|
| 死活（推奨） | アプリが応答しているか | 各コンテナの `/metrics` を Prometheus が直接スクレイプ → `up` メトリクス |
| 死活（汎用・予備） | コンテナが存在し動いているか | cAdvisor の `container_last_seen` を**コンテナ名で絞る** |
| エラーログ | 標準出力のエラー行 | Promtail が**対象コンテナのログだけ**収集 → Loki Ruler が評価 |

### 1.2 「指定コンテナだけ」を実現する3つの絞り込み点
監視対象を限定するために編集するのは、実質この3か所だけです。

1. **死活（直接スクレイプ）**: `prometheus.yml` の `scrape_configs` に**対象コンテナを個別に**列挙する。
2. **ログ収集**: `promtail-config.yml` の `relabel_configs` で `action: keep` を使い、**対象コンテナ名に一致するログだけ**残す。
3. **アラート条件**: 各ルールの label セレクタ（`up{job="vllm"}` / `{container="vllm"}` 等）で**対象コンテナだけ**を評価する。

### 1.3 データの流れ
```
[vLLM / LiteLLM コンテナ]
   │  /metrics（死活）         │  stdout ログ（エラー）
   ▼                          ▼
[Prometheus] ← cAdvisor    [Promtail] → [Loki + Ruler]
   │  alert.rules 評価            │  LogQL ルール評価
   └──────────┬─────────────────┘
              ▼
        [Alertmanager]  ← Notify アプリが /api/v2/alerts をポーリング
              ▼
        [Notify (Windows トレイ常駐アプリ)]
```

### 1.4 接続確認用ハートビート
Notify アプリは「API が応答しただけ」では接続中と判定せず、テスト用アラート **`AlwaysFiringTest`**（常時発火）を実際に受信できたときのみ「接続中」とみなします（デッドマンズスイッチ）。本構成にも最初からこのアラートを含めています（5.2 参照）。

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
    └── loki-rules/
        └── fake/                 # "fake" は Loki のデフォルトテナント
            └── llm-log-rules.yml
```

---

## 4. docker-compose.yml

死活（cAdvisor）とログ（Promtail）を**最初から含んだ完成形**です。

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

networks:
  monitor:
    name: monitor
```

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

---

## 5. 設定ファイル

### 5.1 `config/prometheus.yml`
Prometheus 自身・cAdvisor に加え、**監視対象を個別ジョブとして列挙**します（＝絞り込み点①）。

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

> - **vLLM** は `/metrics`（Prometheus 形式）を標準で公開します。
> - **LiteLLM** は Prometheus 連携を有効化（`litellm_settings.callbacks: ["prometheus"]` 等）すると `/metrics` を公開します。出せない構成の場合は、LiteLLM の死活は 5.2 の cAdvisor 方式（`container_last_seen` / `absent`）で代替してください。

### 5.2 `config/alert.rules.yml`
死活アラートとハートビートを定義します。**対象コンテナだけ**を評価しているのがポイントです（＝絞り込み点③）。

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
```

> **なぜ `up` が最も確実か**：`static_configs` で対象を明示しているため、コンテナが落ちても**系列が消えず** `up=0` になります。
> **cAdvisor の注意点**：コンテナ停止後しばらくすると `container_last_seen` の系列自体が消え、`time() - container_last_seen > 60` だけだと**アラートが勝手に解決**してしまいます。そのため「常時稼働すべきコンテナ」には `absent()` を併用します。`absent()` は正規表現と相性が悪いので**コンテナごとに1ルール**ずつ書きます。`name` は先頭スラッシュ無しのコンテナ名です。

### 5.3 `config/alertmanager.yml`
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

### 5.4 `config/loki-config.yml`
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

### 5.5 `config/promtail-config.yml`
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

> Docker SD のメタラベル `__meta_docker_container_name` は `/vllm` のように**先頭スラッシュ付き**です。`keep` の正規表現は `/(vllm|litellm)` とします。ホスト全体の syslog を併せて集めたい場合は、別途 `static_configs` の `varlogs` ジョブを追加してください（本書では対象コンテナに集中するため省略）。

### 5.6 `config/loki-rules/fake/llm-log-rules.yml`
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
> - `|~ \`(?i)error|...\`` は大文字小文字を無視した正規表現マッチ。vLLM では `cuda` / `out of memory` を加えて GPU OOM を拾えます。
> - `for: 0m`（1行でも即発火）はノイズが多めです。運用では `> 5`（5分で5行以上）など閾値を上げると安定します。

---

## 6. 起動と動作確認

```bash
cd /opt/monitor
docker compose up -d
```

各エンドポイントを確認します。

- **Prometheus**: `http://<サーバーIP>:9090` → Status > Targets で `vllm` / `litellm` / `cadvisor` が **UP** か確認
- **Alertmanager**: `http://<サーバーIP>:9093`
- **Alertmanager API**（Notify が読む）:
  ```bash
  curl http://localhost:9093/api/v2/alerts
  ```
  正常なら、まず `AlwaysFiringTest` を含む JSON 配列が返ります。

絞り込みの動作確認：
```bash
# cAdvisor が対象コンテナを観測しているか
curl -s http://localhost:8080/metrics | grep 'container_last_seen' | grep -E 'name="(vllm|litellm)"'

# Loki のルールが読み込まれているか
curl -s http://localhost:3100/loki/api/v1/rules

# 死活テスト: 対象コンテナを止めて 1〜2 分待つ
docker stop vllm     # → VLLMDown / VLLMContainerAbsent が発火し Notify に届く
docker start vllm    # → 復旧（Notify に [RESOLVED] 通知）

# エラーログテスト: 対象コンテナの stdout に ERROR を出す
docker exec vllm sh -c 'echo "ERROR test from vllm" 1>&2'   # → VLLMErrorLog が発火
```

---

## 7. Notify アプリ側の設定

1. Notify の「設定」タブを開く。
2. **Alertmanager API URL** に `http://<UbuntuサーバーIP>:9093` を入力（複数の Alertmanager があれば「URLを追加」で並列監視・重複排除）。
3. **テスト用アラート名（ハートビート）** が `AlwaysFiringTest` になっていることを確認（5.2 のルール名と一致させる）。
4. 「設定を保存」。各サーバーの状態が「接続中」になれば、`/metrics` 死活・cAdvisor 死活・ログ検知の各アラートがそのまま Notify に届きます。

---

## 8. ファイアウォール (UFW)

Notify が動く Windows PC から Alertmanager のポート `9093` へアクセスを許可します。

```bash
# 特定 PC のみ許可
sudo ufw allow from 192.168.1.50 to any port 9093 proto tcp
# または社内 LAN 全体
sudo ufw allow from 192.168.1.0/24 to any port 9093 proto tcp

sudo ufw enable
sudo ufw status
```

---

## 9. 監視対象の追加・変更

新しいコンテナ（例: `myapp`）を監視対象に加えるときは、**前述の3つの絞り込み点だけ**を編集します。

| 監視内容 | 編集ファイル | 追記内容 |
|---|---|---|
| 死活（推奨） | `prometheus.yml` ＋ `alert.rules.yml` | `job_name: 'myapp'` を `scrape_configs` に追加し、`up{job="myapp"} == 0` ルールを追加 |
| 死活（予備） | `alert.rules.yml` | `container_last_seen` の正規表現に追加（`name=~"vllm\|litellm\|myapp"`）＋ `absent(... name="myapp")` ルール |
| エラーログ | `promtail-config.yml` ＋ Loki ルール | `keep` の正規表現に追加（`/(vllm\|litellm\|myapp)`）＋ `{container="myapp"}` ルールを追加 |

編集後は設定を反映します。

```bash
# Prometheus に設定リロードを通知（--web.enable-lifecycle 有効時）
curl -X POST http://localhost:9090/-/reload
# Promtail / Loki は再起動で反映
docker compose restart promtail loki
```

逆に**監視を外す**ときは、同じ3か所から対象名を削除するだけです。これにより、常に「**指定したコンテナだけ**」を監視する状態を保てます。
