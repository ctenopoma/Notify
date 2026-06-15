# UbuntuでのDocker監視基盤（Prometheus/Loki/Alertmanager）セットアップ手順

本ドキュメントでは、Ubuntuサーバー上でDockerを使用して監視基盤（Prometheus, Alertmanager, Loki, Promtail）を構築し、開発したTauriアラートモニターアプリ（Notify）に通知データを供給するための設定・準備手順を解説します。

---

## 1. 前提条件とインストール

Ubuntuサーバーに Docker および Docker Compose がインストールされている必要があります。

```bash
# Dockerのインストール確認
docker --version
docker compose version
```

未インストールの場合は、以下のコマンドでセットアップしてください。
```bash
sudo apt update
sudo apt install -y docker.io docker-compose-v2
sudo systemctl enable --now docker
```

---

## 2. ディレクトリ構成

サーバー上の任意の作業ディレクトリ（例: `/opt/monitor`）に、以下の構成で設定ファイルを作成します。

```text
/opt/monitor/
├── docker-compose.yml
└── config/
    ├── prometheus.yml
    ├── alert.rules.yml
    ├── alertmanager.yml
    ├── loki-config.yml
    └── promtail-config.yml
```

---

## 3. 設定ファイルの作成

### 3.1 `docker-compose.yml`
監視に必要なコンテナ群を定義します。Alertmanagerのポート `9093` は外部（Tauriアプリ）からアクセスできるようホストにマッピングします。

```yaml
version: '3.8'

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
    ports:
      - "9090:9090"
    restart: unless-stopped

  alertmanager:
    image: prom/alertmanager:latest
    container_name: alertmanager
    volumes:
      - ./config/alertmanager.yml:/etc/alertmanager/alertmanager.yml
    command:
      - '--config.file=/etc/alertmanager/alertmanager.yml'
    ports:
      - "9093:9093"
    restart: unless-stopped

  loki:
    image: grafana/loki:latest
    container_name: loki
    volumes:
      - ./config/loki-config.yml:/etc/loki/local-config.yaml
    ports:
      - "3100:3100"
    command: -config.file=/etc/loki/local-config.yaml
    restart: unless-stopped

  promtail:
    image: grafana/promtail:latest
    container_name: promtail
    volumes:
      - ./config/promtail-config.yml:/etc/promtail/config.yml
      - /var/log:/var/log
    command: -config.file=/etc/promtail/config.yml
    restart: unless-stopped

volumes:
  prometheus-data:
```

### 3.2 `config/prometheus.yml`
PrometheusからAlertmanagerへアラートを送信するための設定を行います。

```yaml
global:
  scrape_interval: 15s
  evaluation_interval: 15s

# Alertmanagerの指定
alerting:
  alertmanagers:
    - static_configs:
        - targets:
            - 'alertmanager:9093'

# アラートルールの読み込み
rule_files:
  - 'alert.rules.yml'

scrape_configs:
  - job_name: 'prometheus'
    static_configs:
      - targets: ['localhost:9090']
```

### 3.3 `config/alert.rules.yml`
監視ルールを定義します。テスト用に、常に発火するダミーアラート（`AlwaysFiringTest`）と、コンテナが停止した際に発火するアラートの例を記載します。

```yaml
groups:
  - name: test_rules
    rules:
      # 常時発火するデバッグ用アラート (Tauriアプリの通知テスト用)
      - alert: AlwaysFiringTest
        expr: vector(1)
        for: 10s
        labels:
          severity: warning
        annotations:
          summary: "Tauri通知テスト用アラート"
          description: "このアラートは常時発火するように設定されています。接続テストに利用してください。"

      # インスタンスダウン検知アラート
      - alert: InstanceDown
        expr: up == 0
        for: 1m
        labels:
          severity: critical
        annotations:
          summary: "インスタンス [{{ $labels.instance }}] が停止しています"
          description: "対象のジョブ [{{ $labels.job }}] が1分以上停止しています。"
```

### 3.4 `config/alertmanager.yml`
Alertmanagerのルーティング設定です。Tauriアプリ側からAPIで情報を Pull するため、Alertmanager 自体から外部通知（Slackやメール等）を行わない「最小構成」を定義します。

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
  # クライアントアプリがAPI経由で取得するため、受信先はダミー（ログ出力のみ）で問題ありません
  - name: 'default-receiver'
```

### 3.5 `config/loki-config.yml`
Loki（ログ集約エンジン）の基本構成です。

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
```

### 3.6 `config/promtail-config.yml`
Lokiにホスト（Ubuntu）のログを送信するエージェントの設定です。

```yaml
server:
  http_listen_port: 9080
  grpc_listen_port: 0

positions:
  filename: /tmp/positions.yaml

clients:
  - url: http://loki:3100/loki/api/v1/push

scrape_configs:
  - job_name: system
    static_configs:
      - targets:
          - localhost
        labels:
          job: varlogs
          __path__: /var/log/*log
```

---

## 4. 起動と動作確認

### 4.1 コンテナの起動
ディレクトリとファイルを配置したら、以下のコマンドで起動します。

```bash
# ディレクトリの作成
mkdir -p config
# （上記で作成した設定ファイルを配置したあと）
# コンテナのバックグラウンド起動
docker compose up -d
```

### 4.2 動作確認
起動後、ブラウザや `curl` コマンドで接続できるか確認します。

- **Prometheus UI**: `http://<サーバーIP>:9090`
- **Alertmanager UI**: `http://<サーバーIP>:9093`
- **Alertmanager API**: `http://<サーバーIP>:9093/api/v2/alerts`

特に、Tauriアプリが直接読み込む API の応答を確認してください。
```bash
curl http://localhost:9093/api/v2/alerts
```
設定が正しく行われていれば、`AlwaysFiringTest` アラートの情報を含んだ JSON 配列が返却されます。

---

## 5. ファイアウォール (UFW) の設定

Tauriアプリが動く Windows PC から Ubuntu サーバーの Alertmanager にアクセスできるように、ポート `9093` を開放する必要があります。

```bash
# 接続元の特定のPC（例: 192.168.1.50）からのアクセスを許可する場合
sudo ufw allow from 192.168.1.50 to any port 9093 proto tcp

# または、社内LAN（例: 192.168.1.0/24）全体に許可する場合
sudo ufw allow from 192.168.1.0/24 to any port 9093 proto tcp

# UFWの有効化とステータス確認
sudo ufw enable
sudo ufw status
```

これで Ubuntu 上の Alertmanager 監視環境の準備は完了です。Tauriアプリの設定画面で `http://<UbuntuサーバーIP>:9093` を入力することで、常駐トレイからのアラート検知が開始されます。
