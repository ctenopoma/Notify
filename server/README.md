# Notify 監視サーバー（監視スタック + Web 管理コンソール）

[docs/monitoring_setup.md](../docs/monitoring_setup.md) の監視基盤を **実際に動く docker compose** として実装し、
さらに **ブラウザから設定ファイルを生成・反映できる Web 管理コンソール** を同梱したものです。

監視できるもの:

- **コンテナ死活**（指定コンテナの `/metrics` スクレイプ / cAdvisor 観測 / 消失検知）
- **GPU 負荷・健全性**（DCGM Exporter：VRAM / NVLink / 温度・スロットリング / 効率）
- **ホスト CPU・メモリ・ディスク**（node-exporter）
- **コンテナ別 CPU・メモリ**（cAdvisor）
- **独自メトリクス / 独自アラート条件**（任意の exporter・PromQL）

アラートは **Grafana ユニファイドアラート**に集約されます。Grafana が Prometheus データソースに対して
ルールを評価し、内蔵 Alertmanager（`/api/alertmanager/grafana/api/v2/...`）で発火中アラートを公開します。
デスクトップアプリ **Notify** はこの Grafana 内蔵 Alertmanager を `:3000` でポーリングします
（認証は Grafana のサービスアカウントトークン）。独立した Alertmanager / Loki / promtail は廃止しました。
ログ由来のアラートは扱いません（メトリクスのみ）。

---

## 構成

```
server/
├── docker-compose.yml        # 監視スタック全体 + admin(管理UI) + node-exporter
├── .env.example              # → .env にコピーし MONITOR_DIR を設定
├── admin/                    # Web 管理コンソール（FastAPI + 素のJS）
│   ├── app.py                #   API・docker/compose 操作・起動時の自動スキャン
│   ├── generator.py          #   monitor-config.json → 各設定ファイルを生成
│   ├── catalog.py            #   DCGMメトリクス定義・アラートテンプレート
│   ├── Dockerfile / requirements.txt
│   └── static/               #   index.html / app.js / styles.css
└── config/                   # 生成される設定（初期値も同梱）
    ├── monitor-config.json   #   ★唯一の設定ソース（UIが読み書き）
    ├── prometheus.yml        #   スクレイプ設定（ルール評価は Grafana 側）
    ├── dcgm-counters.csv
    └── grafana/provisioning/
        ├── datasources/      #   Prometheus データソース（静的, uid=prometheus）
        ├── dashboards/       #   ダッシュボード（静的）
        └── alerting/rules.yml #   ★アラートルール（monitor-config.json から生成）
```

`config/prometheus.yml` / `dcgm-counters.csv` / `grafana/provisioning/alerting/rules.yml` は
**`monitor-config.json` から生成** されます。手で編集せず、Web UI から操作してください
（grafana の datasources / dashboards は静的）。

---

## セットアップ

前提: Ubuntu に Docker / Docker Compose、GPU 監視には NVIDIA ドライバ + NVIDIA Container Toolkit
（詳細は [docs/monitoring_setup.md](../docs/monitoring_setup.md) §2）。

```bash
# 1. 任意の場所に配置（例: /opt/monitor）
sudo cp -r server /opt/monitor
cd /opt/monitor

# 2. .env を作成し、MONITOR_DIR をこのディレクトリの「絶対パス」に設定
cp .env.example .env
sed -i "s#^MONITOR_DIR=.*#MONITOR_DIR=$(pwd)#" .env

# 3. 起動
docker compose up -d
```

> **MONITOR_DIR が重要**: admin コンテナはホストの docker デーモンに対して `docker compose` を実行します。
> compose の相対バインド（`./config/...`）がデーモン側で正しく解決されるよう、admin コンテナはこのディレクトリを
> **ホストと同じ絶対パス** にマウントします。`MONITOR_DIR` がこのディレクトリの実パスと一致している必要があります。

アクセス先:

| URL | 用途 |
|---|---|
| `http://<host>:8088` | **Web 管理コンソール** |
| `http://<host>:3000` | Grafana（ダッシュボード ＋ アラート ＋ Notify がポーリングする内蔵 Alertmanager API） |
| `http://<host>:9090` | Prometheus |

---

## Web 管理コンソールでできること

起動すると **稼働中コンテナ・各 exporter の応答・実際に取得できる DCGM フィールド** を自動スキャンします。

- **概要 / 状態**: スタックの応答状況、コンテナ一覧、コンテナの起動/停止/再起動、実測 DCGM フィールド
- **監視対象コンテナ**: 監視するコンテナを名前で指定（死活・cAdvisor・消失）。検出済みコンテナからワンクリック追加
- **メトリクス**: node-exporter / cAdvisor の ON/OFF、DCGM カウンタの選択（この GPU で取得できない PROF 系は「未検出」と警告）、独自スクレイプジョブの追加
- **アラート**: GPU・ホスト/コンテナ資源のテンプレートからしきい値を指定して追加、独自 PromQL 条件の作成・組み合わせ（Grafana ルールとして生成）
- **保持 / ディスク**: Prometheus 保持（期間/サイズ）、コンテナログ(stdout/stderr)のローテーション（サイズ×個数）
- **操作 / プレビュー**: 生成される設定ファイルのプレビュー、保存して反映、`docker compose up/restart/recreate/pull/down`

### 「保存」と「反映」の違い

- **設定を保存**: `monitor-config.json` を保存し、各設定ファイルを再生成（サービスには触れない）
- **保存して反映**: 上記 + Prometheus を hot-reload + grafana/dcgm を restart（Grafana はアラートルールのプロビジョニングを再読込）
- **ログローテーション（`LOG_MAX_*`）の変更だけ**は compose の `recreate` が必要（操作タブの「再生成」）

---

## 監視対象（vLLM / LiteLLM 等）の参加

Prometheus が `vllm:8000` のような名前でスクレイプし、cAdvisor がコンテナ名で識別できるよう、
監視対象を同じ `monitor` ネットワークに参加させます（[docs §4.2](../docs/monitoring_setup.md) 参照）。

```yaml
services:
  vllm:
    container_name: vllm
    networks: [monitor]
networks:
  monitor:
    external: true
```

---

## トラブルシューティング

- **admin から compose が動かない**: `.env` の `MONITOR_DIR` がこのディレクトリの絶対パスか確認。
- **DCGM フィールドが「未検出」**: GPU が PROF 非対応 / `nv-fabricmanager` 未起動 / イメージのプロファイリングモジュール欠落など
  （[docs §9.1](../docs/monitoring_setup.md) を参照）。未検出のカウンタはチェックを外してください。
- **設定の手動編集が消える**: `config/` の yml は再生成で上書きされます。恒久的な変更は UI から行ってください。
