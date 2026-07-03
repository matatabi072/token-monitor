//! Token Monitor 共有ロジック — 認証設定の読み込み・各サービスの Usage 取得・共通モデルへの正規化。
//! CLI(tmctl) と GUI(token-monitor) の両方から使う。

use anyhow::{anyhow, Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

// ───────────────────────── 設定ファイル ─────────────────────────

#[derive(Deserialize, Serialize, Clone, Default)]
pub struct Config {
    pub claude: Option<ClaudeCfg>,
    pub codex: Option<CodexCfg>,
}

#[derive(Deserialize, Serialize, Clone)]
pub struct ClaudeCfg {
    #[serde(default = "yes")]
    pub enabled: bool,
    /// Cookie の sessionKey (sk-ant-sid02-...)
    pub session_key: String,
    /// 組織 UUID (usage URL の /organizations/<ここ>/usage)
    pub org_id: String,
}

#[derive(Deserialize, Serialize, Clone)]
pub struct CodexCfg {
    #[serde(default = "yes")]
    pub enabled: bool,
    /// ChatGPT の長寿命セッションCookie。サイズが大きいとブラウザが .0 / .1 に分割保存するため2つ持つ。
    /// これを基に取得のたび accessToken を自動発行するので、短命トークンの貼り直しが不要。
    /// session_token = __Secure-next-auth.session-token(.0)、session_token2 = .1（無ければ空）
    #[serde(default)]
    pub session_token: String,
    #[serde(default)]
    pub session_token2: String,
}

fn yes() -> bool {
    true
}

/// 実行ディレクトリの config.toml を読む
pub fn load_config() -> Result<Config> {
    let path = std::path::PathBuf::from("config.toml");
    let text = std::fs::read_to_string(&path).with_context(|| {
        format!(
            "{} が読めません。config.example.toml をコピーして認証情報を記入してください。",
            path.display()
        )
    })?;
    toml::from_str(&text).context("config.toml のパースに失敗")
}

// ───────────────────────── 共通モデル ─────────────────────────

#[derive(Clone)]
pub struct LimitBar {
    /// 英語表示ラベル（CLI/フォールバック用）
    pub label: String,
    /// 種別識別子（GUIの言語切替で使う）: session / weekly / weekly_scoped / monthly / other
    pub kind: String,
    /// weekly_scoped のモデル名など（例: Fable）
    pub scope: Option<String>,
    pub percent: f64,
    pub resets_at: Option<DateTime<Utc>>,
    pub active: bool,
}

#[derive(Clone)]
pub struct Credit {
    pub enabled: bool,
    /// 使用率％（バー描画用）。無い場合(残高ベース等)は None
    pub percent: Option<f64>,
    pub detail: String,
}

#[derive(Clone)]
pub struct ServiceUsage {
    pub service: String,
    pub logged_in: bool,
    pub bars: Vec<LimitBar>,
    pub credit: Option<Credit>,
    /// 取得エラー等の注記（ある場合はUIに表示）
    pub error: Option<String>,
}

impl ServiceUsage {
    pub fn logged_out(service: &str) -> Self {
        Self {
            service: service.into(),
            logged_in: false,
            bars: vec![],
            credit: None,
            error: None,
        }
    }
    pub fn errored(service: &str, msg: String) -> Self {
        Self {
            service: service.into(),
            logged_in: false,
            bars: vec![],
            credit: None,
            error: Some(msg),
        }
    }
}

// ───────────────────────── Claude JSON ─────────────────────────

#[derive(Deserialize)]
struct ClaudeUsage {
    #[serde(default)]
    limits: Vec<ClaudeLimit>,
    spend: Option<ClaudeSpend>,
}

#[derive(Deserialize)]
struct ClaudeLimit {
    kind: String,
    #[serde(default)]
    percent: f64,
    resets_at: Option<DateTime<Utc>>,
    #[serde(default)]
    is_active: bool,
    scope: Option<ClaudeScope>,
}

#[derive(Deserialize)]
struct ClaudeScope {
    model: Option<ClaudeModel>,
}

#[derive(Deserialize)]
struct ClaudeModel {
    display_name: Option<String>,
}

#[derive(Deserialize)]
struct ClaudeSpend {
    #[serde(default)]
    percent: f64,
    #[serde(default)]
    enabled: bool,
    #[serde(default)]
    can_purchase_credits: bool,
}

// ───────────────────────── Codex JSON ─────────────────────────

/// /api/auth/session のレスポンス（必要なのは accessToken のみ）
#[derive(Deserialize, Default)]
struct CodexSession {
    #[serde(rename = "accessToken")]
    access_token: Option<String>,
}

#[derive(Deserialize)]
struct CodexUsage {
    /// 将来 primary/secondary 以外の枠(例: tertiary_window)が追加されても
    /// 拾えるよう、決め打ちの構造体ではなく汎用マップとして受ける。
    rate_limit: Option<serde_json::Map<String, serde_json::Value>>,
    credits: Option<CodexCredits>,
}

#[derive(Deserialize)]
struct CodexWindow {
    #[serde(default)]
    used_percent: f64,
    limit_window_seconds: Option<i64>,
    reset_at: Option<i64>,
}

#[derive(Deserialize)]
struct CodexCredits {
    #[serde(default)]
    has_credits: bool,
    balance: Option<f64>,
}

// ───────────────────────── 取得ロジック ─────────────────────────

pub fn http_client() -> Result<reqwest::blocking::Client> {
    reqwest::blocking::Client::builder()
        .user_agent("token-monitor/0.1")
        .timeout(std::time::Duration::from_secs(20))
        .build()
        .context("HTTP クライアント初期化に失敗")
}

/// エラー応答本文の先頭を1行に丸めて表示用に（秘匿値は含まれない想定）
fn body_snippet(b: &str) -> String {
    let s: String = b.chars().take(200).collect();
    s.replace(['\n', '\r'], " ")
}

pub fn fetch_claude(client: &reqwest::blocking::Client, cfg: &ClaudeCfg) -> Result<ServiceUsage> {
    let url = format!("https://claude.ai/api/organizations/{}/usage", cfg.org_id);
    let resp = client
        .get(&url)
        .header("Cookie", format!("sessionKey={}", cfg.session_key))
        .header("anthropic-client-platform", "web_claude_ai")
        .header("anthropic-client-version", "1.0.0")
        .header("Accept", "application/json")
        .send()
        .context("Claude usage リクエスト送信に失敗")?;

    let status = resp.status();
    let body = resp.text().unwrap_or_default();
    if !status.is_success() {
        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            // 未ログイン/セッション切れ → 非表示扱い（仕様 3.2-3）
            return Ok(ServiceUsage::logged_out("ClaudeCode"));
        }
        return Err(anyhow!(
            "Claude usage HTTP {} : {}",
            status,
            body_snippet(&body)
        ));
    }

    let u: ClaudeUsage = serde_json::from_str(&body).context("Claude usage JSON のパースに失敗")?;

    let mut bars = Vec::new();
    for l in &u.limits {
        let (label, kind, scope) = match l.kind.as_str() {
            "session" => ("Session".to_string(), "session".to_string(), None),
            "weekly_all" => ("Weekly".to_string(), "weekly".to_string(), None),
            "weekly_scoped" => {
                let model = l
                    .scope
                    .as_ref()
                    .and_then(|s| s.model.as_ref())
                    .and_then(|m| m.display_name.as_deref())
                    .unwrap_or("scoped")
                    .to_string();
                (
                    format!("Weekly ({model})"),
                    "weekly_scoped".to_string(),
                    Some(model),
                )
            }
            other => (other.to_string(), other.to_string(), None),
        };
        bars.push(LimitBar {
            label,
            kind,
            scope,
            percent: l.percent,
            resets_at: l.resets_at,
            active: l.is_active,
        });
    }

    let credit = u.spend.map(|s| Credit {
        enabled: s.enabled,
        percent: Some(s.percent),
        detail: if s.enabled {
            format!("{}% 使用", s.percent)
        } else if s.can_purchase_credits {
            "OFF（購入可）".to_string()
        } else {
            "OFF".to_string()
        },
    });

    Ok(ServiceUsage {
        service: "ClaudeCode".into(),
        logged_in: true,
        bars,
        credit,
        error: None,
    })
}

/// セッションCookieから accessToken を自動発行する（短命トークンの手動貼付を不要にする）。
/// Cookieが分割(.0/.1)されている場合はブラウザと同じ形で両方送る。
fn codex_access_token(
    client: &reqwest::blocking::Client,
    cfg: &CodexCfg,
) -> Result<Option<String>> {
    let t1 = cfg.session_token.trim();
    let t2 = cfg.session_token2.trim();
    let cookie = if t2.is_empty() {
        format!("__Secure-next-auth.session-token={t1}")
    } else {
        format!(
            "__Secure-next-auth.session-token.0={t1}; __Secure-next-auth.session-token.1={t2}"
        )
    };
    let resp = client
        .get("https://chatgpt.com/api/auth/session")
        .header("Cookie", cookie)
        .header("Accept", "application/json")
        .send()
        .context("Codex セッション確認リクエストに失敗")?;
    if !resp.status().is_success() {
        return Ok(None); // セッション無効
    }
    let body = resp.text().unwrap_or_default();
    let session: CodexSession = serde_json::from_str(&body).unwrap_or_default();
    Ok(session.access_token.filter(|t| !t.is_empty()))
}

pub fn fetch_codex(client: &reqwest::blocking::Client, cfg: &CodexCfg) -> Result<ServiceUsage> {
    // 1) セッションCookie → accessToken（毎回自動更新）
    let access_token = match codex_access_token(client, cfg)? {
        Some(t) => t,
        None => return Ok(ServiceUsage::logged_out("Codex")), // セッション失効/未ログイン
    };

    // 2) accessToken → usage
    let resp = client
        .get("https://chatgpt.com/backend-api/wham/usage")
        .bearer_auth(&access_token)
        .header("Accept", "application/json")
        .send()
        .context("Codex usage リクエスト送信に失敗")?;

    let status = resp.status();
    let body = resp.text().unwrap_or_default();
    if !status.is_success() {
        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            return Ok(ServiceUsage::logged_out("Codex"));
        }
        return Err(anyhow!(
            "Codex usage HTTP {} : {}",
            status,
            body_snippet(&body)
        ));
    }

    let u: CodexUsage = serde_json::from_str(&body).context("Codex usage JSON のパースに失敗")?;

    let mut bars = Vec::new();
    if let Some(rl) = &u.rate_limit {
        // 既知の2枠(primary/secondary)を先に、それ以外の "*_window" キーは
        // 見つかり次第あとに追加する(将来 Codex 側が枠を増やしても自動的に拾う)。
        let known_order = ["primary_window", "secondary_window"];
        let mut keys: Vec<&String> = rl.keys().collect();
        keys.sort_by_key(|k| {
            known_order
                .iter()
                .position(|kn| *kn == k.as_str())
                .unwrap_or(known_order.len())
        });

        for key in keys {
            if !key.ends_with("_window") {
                continue;
            }
            let Some(win) = rl
                .get(key)
                .and_then(|v| serde_json::from_value::<CodexWindow>(v.clone()).ok())
            else {
                continue;
            };
            let fallback: String = known_order
                .iter()
                .position(|kn| *kn == key.as_str())
                .map(|i| if i == 0 { "Primary" } else { "Secondary" }.to_string())
                .unwrap_or_else(|| window_key_fallback(key));
            let (label, kind) = window_label(win.limit_window_seconds, &fallback);
            bars.push(LimitBar {
                label,
                kind,
                scope: None,
                percent: win.used_percent,
                resets_at: win.reset_at.and_then(|t| DateTime::from_timestamp(t, 0)),
                active: true,
            });
        }
    }

    let credit = u.credits.map(|c| Credit {
        enabled: c.has_credits,
        percent: None, // Codex は残高ベースで使用率が無い
        detail: match c.balance {
            Some(b) => format!("${b:.2}"),
            None => {
                if c.has_credits {
                    "あり".into()
                } else {
                    "なし".into()
                }
            }
        },
    });

    Ok(ServiceUsage {
        service: "Codex".into(),
        logged_in: true,
        bars,
        credit,
        error: None,
    })
}

/// 未知の "*_window" キー名からラベルの見た目を作る（例: "tertiary_window" -> "Tertiary"）
fn window_key_fallback(key: &str) -> String {
    let base = key.strip_suffix("_window").unwrap_or(key);
    let mut c = base.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => "Window".to_string(),
    }
}

/// 制限枠の秒数から (英語ラベル, 種別) を決める
fn window_label(seconds: Option<i64>, fallback: &str) -> (String, String) {
    match seconds {
        Some(s) if s <= 6 * 3600 => ("Session".into(), "session".into()),
        Some(s) if s <= 8 * 86400 => ("Weekly".into(), "weekly".into()),
        Some(s) if s <= 32 * 86400 => ("Monthly".into(), "monthly".into()),
        _ => (fallback.into(), "other".into()),
    }
}

// ───────────────────────── 整形ヘルパー ─────────────────────────

/// resets_at までの残り時間を "3h33m" / "2d 5h" 形式に
pub fn remaining(resets_at: Option<DateTime<Utc>>) -> String {
    let Some(t) = resets_at else {
        return "---".into();
    };
    let secs = (t - Utc::now()).num_seconds();
    if secs <= 0 {
        return "まもなく".into();
    }
    let d = secs / 86400;
    let h = (secs % 86400) / 3600;
    let m = (secs % 3600) / 60;
    if d > 0 {
        format!("{d}d {h}h")
    } else if h > 0 {
        format!("{h}h{m}m")
    } else {
        format!("{m}m")
    }
}

/// コンソール用のバーグラフ（CLI で使用）
pub fn bar_graph(percent: f64) -> String {
    const WIDTH: usize = 14;
    let filled = (((percent / 100.0) * WIDTH as f64).round() as usize).min(WIDTH);
    format!(
        "[{}{}]",
        "\u{2588}".repeat(filled),
        "\u{2591}".repeat(WIDTH - filled)
    )
}
