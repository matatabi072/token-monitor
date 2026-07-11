//! token-monitor (GUI) — 極小ウィンドウで各サービスのトークン使用状況を常時表示。
//! 認証情報はアプリ内(設定ウィンドウ)で入力し config.toml に保存。ログイン状態＝情報入力済みか。

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")] // release時はコンソール窓を出さない

use chrono::{Datelike, Utc};
use eframe::egui;
use notify_rust::Notification;
use std::collections::HashSet;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};
use token_monitor::{
    fetch_claude, fetch_codex, http_client, load_config, ClaudeCfg, CodexCfg, Config, LimitBar,
    ServiceUsage,
};

const POLL_INTERVAL: Duration = Duration::from_secs(180);

// テーマ選択（永続化のため u8）: 0=OS準拠, 1=ダーク, 2=ライト
const THEME_SYSTEM: u8 = 0;
const THEME_DARK: u8 = 1;
const THEME_LIGHT: u8 = 2;

// 表示言語: 0=日本語, 1=English
const LANG_JP: u8 = 0;
const LANG_EN: u8 = 1;

// 設定ウィンドウのサイズ（開く位置の画面内クランプ計算にも使う）
const SETTINGS_W: f32 = 340.0;
const SETTINGS_H: f32 = 440.0;

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([300.0, 260.0])
            .with_min_inner_size([220.0, 60.0])
            .with_decorations(false) // 独自タイトルバー（テーマに追従させるため）
            .with_resizable(true)
            .with_title("Token Monitor"),
        ..Default::default()
    };
    eframe::run_native(
        "token-monitor",
        options,
        Box::new(|cc| Ok(Box::new(TokenApp::new(cc)))),
    )
}

struct TokenApp {
    rx: Receiver<Vec<ServiceUsage>>,
    tx_cmd: Sender<Config>,
    data: Vec<ServiceUsage>,
    last_update: Option<Instant>,
    loading: bool,

    // 設定（永続化）
    show_credit: bool,
    theme: u8,
    lang: u8,
    applied_dark: Option<bool>,
    settings_open: bool,
    settings_pos: Option<egui::Pos2>,

    // 認証情報の編集バッファ（config.toml と同期）
    claude_key: String,
    claude_org: String,
    codex_session: String,
    codex_session2: String,

    // アラート設定
    alert_enabled: bool,
    usage_thresholds: Vec<f64>,
    reset_minutes: Vec<i64>,
    usage_thresholds_str: String,
    reset_minutes_str: String,
    fired: HashSet<String>,
}

impl TokenApp {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        setup_fonts(&cc.egui_ctx);

        // 永続化設定を復元
        let (mut theme, mut show_credit) = (THEME_SYSTEM, false);
        let mut lang = LANG_JP;
        let mut alert_enabled = true;
        let mut usage_thresholds = vec![90.0];
        let mut reset_minutes = vec![10i64];
        if let Some(storage) = cc.storage {
            theme = eframe::get_value(storage, "theme").unwrap_or(THEME_SYSTEM);
            lang = eframe::get_value(storage, "lang").unwrap_or(LANG_JP);
            show_credit = eframe::get_value(storage, "show_credit").unwrap_or(false);
            alert_enabled = eframe::get_value(storage, "alert_enabled").unwrap_or(true);
            usage_thresholds =
                eframe::get_value(storage, "usage_thresholds").unwrap_or_else(|| vec![90.0]);
            reset_minutes =
                eframe::get_value(storage, "reset_minutes").unwrap_or_else(|| vec![10]);
        }

        // 認証情報を config.toml から読み込み（無ければ空＝未ログイン）
        let (mut claude_key, mut claude_org, mut codex_session, mut codex_session2) =
            (String::new(), String::new(), String::new(), String::new());
        if let Ok(cfg) = load_config() {
            if let Some(c) = cfg.claude {
                claude_key = c.session_key;
                claude_org = c.org_id;
            }
            if let Some(c) = cfg.codex {
                codex_session = c.session_token;
                codex_session2 = c.session_token2;
            }
        }

        let (tx_result, rx) = mpsc::channel::<Vec<ServiceUsage>>();
        let (tx_cmd, rx_cmd) = mpsc::channel::<Config>();
        let ctx = cc.egui_ctx.clone();
        std::thread::spawn(move || worker(ctx, tx_result, rx_cmd));

        let app = Self {
            rx,
            tx_cmd,
            data: Vec::new(),
            last_update: None,
            loading: true,
            show_credit,
            theme,
            lang,
            applied_dark: None,
            settings_open: false,
            settings_pos: None,
            claude_key,
            claude_org,
            codex_session,
            codex_session2,
            alert_enabled,
            usage_thresholds_str: join_f(&usage_thresholds),
            reset_minutes_str: join_i(&reset_minutes),
            usage_thresholds,
            reset_minutes,
            fired: HashSet::new(),
        };
        // 初期取得を開始
        let _ = app.tx_cmd.send(app.build_config());
        app
    }

    // ── ログイン判定（プレースホルダ/空は未ログイン扱い） ──
    fn claude_logged_in(&self) -> bool {
        let k = self.claude_key.trim();
        let o = self.claude_org.trim();
        !k.is_empty() && !k.contains("XXXX") && !o.is_empty() && !o.starts_with("0000")
    }
    fn codex_logged_in(&self) -> bool {
        let t = self.codex_session.trim();
        t.len() >= 20
    }

    /// ログイン済みサービスだけを含む Config（ワーカーへ渡す）
    fn build_config(&self) -> Config {
        Config {
            claude: self.claude_logged_in().then(|| ClaudeCfg {
                enabled: true,
                session_key: self.claude_key.trim().to_string(),
                org_id: self.claude_org.trim().to_string(),
            }),
            codex: self.codex_logged_in().then(|| CodexCfg {
                enabled: true,
                session_token: self.codex_session.trim().to_string(),
                session_token2: self.codex_session2.trim().to_string(),
            }),
        }
    }

    /// 認証情報を config.toml に保存（アプリが所有）
    fn save_config(&self) {
        let cfg = Config {
            claude: Some(ClaudeCfg {
                enabled: true,
                session_key: self.claude_key.trim().to_string(),
                org_id: self.claude_org.trim().to_string(),
            }),
            codex: Some(CodexCfg {
                enabled: true,
                session_token: self.codex_session.trim().to_string(),
                session_token2: self.codex_session2.trim().to_string(),
            }),
        };
        if let Ok(text) = toml::to_string_pretty(&cfg) {
            let _ = std::fs::write("config.toml", text);
        }
    }

    /// 設定変更を保存し、ワーカーに再取得を要求
    fn apply_and_refresh(&mut self) {
        self.save_config();
        self.loading = true;
        let _ = self.tx_cmd.send(self.build_config());
    }

    fn find_usage(&self, prefix: &str) -> Option<&ServiceUsage> {
        self.data.iter().find(|s| s.service.starts_with(prefix))
    }

    /// 新データ取得時に閾値を判定して通知（同じ閾値で鳴り続けないよう発火済みを管理）
    fn check_alerts(&mut self) {
        if !self.alert_enabled {
            return;
        }
        let mut fired = std::mem::take(&mut self.fired);
        let mut notes: Vec<(String, String)> = Vec::new();

        for s in &self.data {
            if !s.logged_in {
                continue;
            }
            for b in &s.bars {
                // 使用率の閾値
                for &th in &self.usage_thresholds {
                    let key = format!("{}|{}|u{}", s.service, b.label, th);
                    if b.percent >= th {
                        if fired.insert(key) {
                            notes.push((
                                format!("{} — {}", s.service, b.label),
                                format!("使用率 {:.0}%（閾値 {:.0}% 超え）", b.percent, th),
                            ));
                        }
                    } else {
                        fired.remove(&key);
                    }
                }
                // リセットまでの残り時間の閾値
                if let Some(t) = b.resets_at {
                    let rem = (t - Utc::now()).num_minutes();
                    for &m in &self.reset_minutes {
                        let key = format!("{}|{}|r{}", s.service, b.label, m);
                        if rem >= 0 && rem <= m {
                            if fired.insert(key) {
                                notes.push((
                                    format!("{} — {}", s.service, b.label),
                                    format!("リセットまで約{}分", rem),
                                ));
                            }
                        } else {
                            fired.remove(&key);
                        }
                    }
                }
            }
        }

        self.fired = fired;
        for (title, body) in notes {
            notify(&title, &body);
        }
    }
}

/// デスクトップ通知（UIをブロックしないよう別スレッドで表示）
fn notify(title: &str, body: &str) {
    let (t, b) = (title.to_string(), body.to_string());
    std::thread::spawn(move || {
        let _ = Notification::new().summary(&t).body(&b).show();
    });
}

fn parse_thresholds_f(s: &str) -> Vec<f64> {
    s.split(',')
        .filter_map(|x| x.trim().parse::<f64>().ok())
        .filter(|v| *v > 0.0)
        .collect()
}

fn parse_thresholds_i(s: &str) -> Vec<i64> {
    s.split(',')
        .filter_map(|x| x.trim().parse::<i64>().ok())
        .filter(|v| *v > 0)
        .collect()
}

fn join_f(v: &[f64]) -> String {
    v.iter().map(|x| x.to_string()).collect::<Vec<_>>().join(", ")
}

fn join_i(v: &[i64]) -> String {
    v.iter().map(|x| x.to_string()).collect::<Vec<_>>().join(", ")
}

/// リセットまでの残り時間を表示言語に合わせて整形
/// リセット日を短い日付表記に（JP: "8/1", EN: "Aug 1"）
fn fmt_reset_date(t: chrono::DateTime<Utc>, jp: bool) -> String {
    if jp {
        format!("{}/{}", t.month(), t.day())
    } else {
        const MONTHS: [&str; 12] = [
            "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
        ];
        format!("{} {}", MONTHS[t.month0() as usize], t.day())
    }
}

fn fmt_remaining(resets_at: Option<chrono::DateTime<Utc>>, jp: bool) -> String {
    let Some(t) = resets_at else {
        return "---".into();
    };
    let secs = (t - Utc::now()).num_seconds();
    if secs <= 0 {
        return if jp { "まもなく".into() } else { "soon".into() };
    }
    let d = secs / 86400;
    let h = (secs % 86400) / 3600;
    let m = (secs % 3600) / 60;
    if jp {
        if d > 0 {
            format!("{d}日{h}時間")
        } else if h > 0 {
            format!("{h}時間{m}分")
        } else {
            format!("{m}分")
        }
    } else if d > 0 {
        format!("{d}d {h}h")
    } else if h > 0 {
        format!("{h}h{m}m")
    } else {
        format!("{m}m")
    }
}

/// 背景ワーカー：Config を受け取って取得し、以後は一定間隔（または新Config受信時）に再取得。
fn worker(ctx: egui::Context, tx: Sender<Vec<ServiceUsage>>, rx_cmd: Receiver<Config>) {
    let client = match http_client() {
        Ok(c) => c,
        Err(e) => {
            let _ = tx.send(vec![ServiceUsage::errored("HTTP", format!("{e:#}"))]);
            ctx.request_repaint();
            return;
        }
    };

    // 最初の Config を待つ
    let mut cfg = match rx_cmd.recv() {
        Ok(c) => c,
        Err(_) => return,
    };

    loop {
        let mut out = Vec::new();
        if let Some(c) = cfg.claude.as_ref().filter(|c| c.enabled) {
            out.push(
                fetch_claude(&client, c)
                    .unwrap_or_else(|e| ServiceUsage::errored("ClaudeCode", format!("{e:#}"))),
            );
        }
        if let Some(c) = cfg.codex.as_ref().filter(|c| c.enabled) {
            out.push(
                fetch_codex(&client, c)
                    .unwrap_or_else(|e| ServiceUsage::errored("Codex", format!("{e:#}"))),
            );
        }
        let _ = tx.send(out);
        ctx.request_repaint();

        // 次のポーリングまで待機。新しい Config が来たら即それで再取得。
        match rx_cmd.recv_timeout(POLL_INTERVAL) {
            Ok(new_cfg) => cfg = new_cfg,
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
}

impl eframe::App for TokenApp {
    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        eframe::set_value(storage, "theme", &self.theme);
        eframe::set_value(storage, "lang", &self.lang);
        eframe::set_value(storage, "show_credit", &self.show_credit);
        eframe::set_value(storage, "alert_enabled", &self.alert_enabled);
        eframe::set_value(storage, "usage_thresholds", &self.usage_thresholds);
        eframe::set_value(storage, "reset_minutes", &self.reset_minutes);
    }

    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // テーマ適用（変化時のみ）
        let sys_dark = ctx.input(|i| i.raw.system_theme) != Some(egui::Theme::Light);
        let want_dark = match self.theme {
            THEME_DARK => true,
            THEME_LIGHT => false,
            _ => sys_dark,
        };
        if self.applied_dark != Some(want_dark) {
            ctx.set_visuals(if want_dark {
                egui::Visuals::dark()
            } else {
                egui::Visuals::light()
            });
            self.applied_dark = Some(want_dark);
        }

        // 取得結果の反映
        let mut got_new = false;
        while let Ok(d) = self.rx.try_recv() {
            self.data = d;
            self.last_update = Some(Instant::now());
            self.loading = false;
            got_new = true;
        }
        if got_new {
            self.check_alerts();
        }

        // ── 独自タイトルバー（テーマ追従） ──
        let title_h = egui::TopBottomPanel::top("title_bar").show(ctx, |ui| {
            let full = ui.max_rect();
            let bar_rect = egui::Rect::from_min_size(full.min, egui::vec2(full.width(), 24.0));
            let resp = ui.interact(
                bar_rect,
                egui::Id::new("title_bar_drag"),
                egui::Sense::click_and_drag(),
            );
            if resp.drag_started() {
                ui.ctx().send_viewport_cmd(egui::ViewportCommand::StartDrag);
            }
            ui.horizontal(|ui| {
                ui.add_space(2.0);
                ui.label(egui::RichText::new("Token Monitor").strong());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.add(egui::Button::new("×").frame(false)).clicked() {
                        ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                    if ui.add(egui::Button::new("—").frame(false)).clicked() {
                        ui.ctx()
                            .send_viewport_cmd(egui::ViewportCommand::Minimized(true));
                    }
                });
            });
        })
        .response
        .rect
        .height();

        let content_h = egui::CentralPanel::default()
            .show(ctx, |ui| {
                ui.spacing_mut().item_spacing.y = 4.0;

            // ── 状態表示欄（ログイン済みサービスのみ・区切り線付き） ──
            let services = [
                ("ClaudeCode", "Claude", self.claude_logged_in()),
                ("Codex", "Codex", self.codex_logged_in()),
            ];
            let mut first = true;
            let mut any = false;
            for (default_name, prefix, logged_in) in services {
                if !logged_in {
                    continue;
                }
                any = true;
                if !first {
                    ui.separator(); // サービス間の区切り
                }
                first = false;

                let usage = self.find_usage(prefix);
                let color = service_color(prefix);
                let title = usage.map(|u| u.service.clone()).unwrap_or_else(|| default_name.into());

                ui.add_space(2.0);
                ui.label(egui::RichText::new(title).strong().color(color));

                match usage {
                    None => {
                        ui.label(egui::RichText::new("  取得中…").weak());
                    }
                    Some(u) if u.error.is_some() => {
                        ui.colored_label(
                            egui::Color32::from_rgb(220, 90, 90),
                            "  取得エラー / 要再ログイン",
                        );
                    }
                    Some(u) if !u.logged_in => {
                        ui.colored_label(
                            egui::Color32::from_rgb(220, 160, 60),
                            "  要再ログイン（トークン失効の可能性）",
                        );
                    }
                    Some(u) => {
                        for b in &u.bars {
                            let jp = self.lang == LANG_JP;
                            ui.horizontal(|ui| {
                                ui.label(self.bar_label(b));
                                ui.with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
                                        ui.label(format!(
                                            "{:.0}%  ({})",
                                            b.percent,
                                            fmt_remaining(b.resets_at, jp)
                                        ));
                                    },
                                );
                            });
                            draw_bar(ui, (b.percent / 100.0).clamp(0.0, 1.0) as f32, color);
                        }
                        if self.show_credit {
                            if let Some(c) = &u.credit {
                                let jp = self.lang == LANG_JP;
                                let credit_color = egui::Color32::from_rgb(210, 170, 70); // アンバー
                                // ラベル行（Credit + 使用額）… 常に表示
                                ui.horizontal(|ui| {
                                    ui.label(self.credit_label());
                                    ui.with_layout(
                                        egui::Layout::right_to_left(egui::Align::Center),
                                        |ui| {
                                            if let Some(u) = c.used_dollars {
                                                ui.label(format!(
                                                    "{} ${u:.2}",
                                                    if jp { "使用" } else { "used" }
                                                ));
                                            } else {
                                                ui.label(&c.detail);
                                            }
                                        },
                                    );
                                });
                                // バー … ％がある場合は併せて表示（両方出す）
                                if let Some(p) = c.percent {
                                    draw_bar(ui, (p / 100.0).clamp(0.0, 1.0) as f32, credit_color);
                                }
                                // 残高・リセット日（小さめの補足行）
                                if c.remaining_dollars.is_some() || c.resets_at.is_some() {
                                    ui.horizontal(|ui| {
                                        ui.with_layout(
                                            egui::Layout::right_to_left(egui::Align::Center),
                                            |ui| {
                                                let mut parts = Vec::new();
                                                if let Some(r) = c.remaining_dollars {
                                                    parts.push(format!(
                                                        "{} ${r:.2}",
                                                        if jp { "残高" } else { "balance" }
                                                    ));
                                                }
                                                if let (Some(t), Some(l)) =
                                                    (c.resets_at, c.limit_dollars)
                                                {
                                                    let d = fmt_reset_date(t, jp);
                                                    parts.push(if jp {
                                                        format!("{d}にリセット ${l:.2}")
                                                    } else {
                                                        format!("resets {d} ${l:.2}")
                                                    });
                                                }
                                                ui.small(parts.join("  ・  "));
                                            },
                                        );
                                    });
                                }
                            }
                        }
                    }
                }
            }

            if !any {
                ui.add_space(4.0);
                ui.label("未ログインです。⚙設定 からログインしてください。");
            }

            ui.separator();

            if self.loading && any {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(egui::RichText::new("更新中…").strong());
                });
            }

            // ── 操作行（メインはシンプルに） ──
            ui.horizontal(|ui| {
                if ui.button("更新").clicked() {
                    self.loading = true;
                    let _ = self.tx_cmd.send(self.build_config());
                }
                if ui.button("⚙ 設定").clicked() {
                    self.settings_open = true;
                    // メインウィンドウの右隣に開く（開くたびに現在位置へ追従）。
                    // 画面端でオフスクリーンになると開いたのに見えない事故になるため、
                    // モニタ範囲が取れる場合は右→左優先で画面内にクランプする。
                    self.settings_pos = ui.ctx().input(|i| {
                        let vp = i.viewport();
                        let outer = vp.outer_rect?;
                        let mut x = outer.max.x + 8.0;
                        let mut y = outer.min.y;
                        if let Some(mon) = vp.monitor_size {
                            if mon.x > 1.0 && mon.y > 1.0 {
                                if x + SETTINGS_W > mon.x {
                                    // 右に収まらないなら左隣に開く
                                    x = outer.min.x - SETTINGS_W - 8.0;
                                }
                                x = x.clamp(0.0, (mon.x - SETTINGS_W).max(0.0));
                                y = y.clamp(0.0, (mon.y - SETTINGS_H).max(0.0));
                            }
                        }
                        Some(egui::pos2(x, y))
                    });
                }
            });

            if let Some(t) = self.last_update {
                ui.label(
                    egui::RichText::new(format!("最終更新: {}秒前", t.elapsed().as_secs()))
                        .small()
                        .weak(),
                );
                }

                ui.min_rect().height()
            })
            .inner;

        // ── 設定ウィンドウ（別ウィンドウ） ──
        if self.settings_open {
            let mut close = false;
            let mut builder = egui::ViewportBuilder::default()
                .with_title("Token Monitor 設定")
                .with_inner_size([SETTINGS_W, SETTINGS_H])
                .with_decorations(false) // メイン同様、独自タイトルバーでテーマ追従
                .with_resizable(true);
            if let Some(p) = self.settings_pos {
                builder = builder.with_position(p); // メインの隣に開く
            }
            ctx.show_viewport_immediate(
                egui::ViewportId::from_hash_of("settings"),
                builder,
                |vctx, _class| {
                    egui::TopBottomPanel::top("settings_title").show(vctx, |ui| {
                        let full = ui.max_rect();
                        let bar =
                            egui::Rect::from_min_size(full.min, egui::vec2(full.width(), 24.0));
                        let r = ui.interact(
                            bar,
                            egui::Id::new("settings_drag"),
                            egui::Sense::click_and_drag(),
                        );
                        if r.drag_started() {
                            ui.ctx().send_viewport_cmd(egui::ViewportCommand::StartDrag);
                        }
                        ui.horizontal(|ui| {
                            ui.add_space(2.0);
                            ui.label(egui::RichText::new("設定").strong());
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                if ui.add(egui::Button::new("×").frame(false)).clicked() {
                                    close = true;
                                }
                            });
                        });
                    });
                    egui::CentralPanel::default().show(vctx, |ui| {
                        egui::ScrollArea::vertical().show(ui, |ui| {
                            self.settings_ui(ui);
                        });
                    });
                    if vctx.input(|i| i.viewport().close_requested()) {
                        close = true;
                    }
                },
            );
            if close {
                self.settings_open = false;
            }
        }

        // ── 縦幅オートフィット（コンテンツ量に合わせてウィンドウ高さを自動調整） ──
        let desired_h = title_h + content_h + 12.0;
        let cur = ctx.screen_rect();
        if (cur.height() - desired_h).abs() > 2.0 {
            ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(egui::vec2(
                cur.width(),
                desired_h,
            )));
        }

        // 「最終更新: N秒前」を進めるため緩やかに再描画
        ctx.request_repaint_after(Duration::from_secs(1));
    }
}

impl TokenApp {
    fn settings_ui(&mut self, ui: &mut egui::Ui) {
        ui.add_space(4.0);

        // ── ClaudeCode（折りたたみ・ヘッダに実ログイン状態） ──
        let cl = svc_status(self.claude_logged_in(), self.find_usage("Claude"));
        egui::CollapsingHeader::new(status_header("ClaudeCode", cl))
            .default_open(!matches!(cl, SvcStatus::LoggedIn))
            .show(ui, |ui| {
                ui.label("sessionKey（claude.ai の Cookie）:");
                ui.add(egui::TextEdit::singleline(&mut self.claude_key).password(true).desired_width(f32::INFINITY));
                ui.label("組織ID（usage URL の organizations/●/usage）:");
                ui.add(egui::TextEdit::singleline(&mut self.claude_org).desired_width(f32::INFINITY));
                ui.horizontal(|ui| {
                    if ui.button("保存してログイン").clicked() {
                        self.apply_and_refresh();
                    }
                    if ui.button("ログアウト").clicked() {
                        // 組織IDは固定・非機密なので残す（再ログインは sessionKey の貼り直しだけで済む）
                        self.claude_key.clear();
                        self.apply_and_refresh();
                    }
                });
                ui.collapsing("❓ sessionKey / 組織ID の取得方法", |ui| {
                    ui.label("1. ブラウザで claude.ai にログイン");
                    ui.hyperlink_to("→ claude.ai の使用量ページを開く", "https://claude.ai/settings/usage");
                    ui.label("2. F12 →「アプリケーション」→ Cookie → claude.ai");
                    ui.label("3. sessionKey の値(sk-ant-sid02-…)をコピーして上に貼付");
                    ui.label("4. 組織IDは usage ページURLの organizations/●/usage の ● 部分");
                });
            });

        // ── Codex（折りたたみ） ──
        let cx = svc_status(self.codex_logged_in(), self.find_usage("Codex"));
        egui::CollapsingHeader::new(status_header("Codex", cx))
            .default_open(!matches!(cx, SvcStatus::LoggedIn))
            .show(ui, |ui| {
                ui.label("セッションCookie .0（__Secure-next-auth.session-token.0）:");
                ui.add(egui::TextEdit::singleline(&mut self.codex_session).password(true).desired_width(f32::INFINITY));
                ui.label(".1（分割されている場合のみ・無ければ空）:");
                ui.add(egui::TextEdit::singleline(&mut self.codex_session2).password(true).desired_width(f32::INFINITY));
                ui.horizontal(|ui| {
                    if ui.button("保存してログイン").clicked() {
                        self.apply_and_refresh();
                    }
                    if ui.button("ログアウト").clicked() {
                        self.codex_session.clear();
                        self.codex_session2.clear();
                        self.apply_and_refresh();
                    }
                });
                ui.collapsing("❓ セッションCookie の取得方法", |ui| {
                    ui.label("1. ブラウザで chatgpt.com にログイン");
                    ui.hyperlink_to("→ ChatGPT を開く", "https://chatgpt.com");
                    ui.label("2. F12 →「アプリケーション」→ Cookie → chatgpt.com");
                    ui.label("3. __Secure-next-auth.session-token.0 の値を上の「.0」へ");
                    ui.label("4. .1 があれば その値を「.1」へ（大きいと2つに分割されます）");
                    ui.label("※長寿命なので一度貼れば貼り直し不要（accessTokenは自動発行）");
                });
            });

        // ── アラート（折りたたみ） ──
        egui::CollapsingHeader::new(egui::RichText::new("アラート").strong())
            .show(ui, |ui| {
                ui.checkbox(&mut self.alert_enabled, "デスクトップ通知を有効にする");
                ui.label("使用率の閾値％（カンマ区切りで複数可 例: 80, 90, 95）:");
                if ui
                    .add(egui::TextEdit::singleline(&mut self.usage_thresholds_str).desired_width(f32::INFINITY))
                    .changed()
                {
                    self.usage_thresholds = parse_thresholds_f(&self.usage_thresholds_str);
                }
                ui.label("リセット前に通知する分（カンマ区切りで複数可 例: 30, 10）:");
                if ui
                    .add(egui::TextEdit::singleline(&mut self.reset_minutes_str).desired_width(f32::INFINITY))
                    .changed()
                {
                    self.reset_minutes = parse_thresholds_i(&self.reset_minutes_str);
                }
                if ui.button("テスト通知").clicked() {
                    notify("Token Monitor", "テスト通知です");
                }
            });

        ui.add_space(8.0);
        ui.separator();
        ui.add_space(8.0);

        // ── 表示設定 ──
        ui.checkbox(&mut self.show_credit, "追加クレジットを表示");
        ui.horizontal(|ui| {
            ui.label("テーマ:");
            ui.selectable_value(&mut self.theme, THEME_SYSTEM, "OS準拠");
            ui.selectable_value(&mut self.theme, THEME_DARK, "ダーク");
            ui.selectable_value(&mut self.theme, THEME_LIGHT, "ライト");
        });
        ui.horizontal(|ui| {
            ui.label("表示言語:");
            ui.selectable_value(&mut self.lang, LANG_JP, "日本語");
            ui.selectable_value(&mut self.lang, LANG_EN, "English");
        });

        ui.add_space(6.0);
        if ui.button("閉じる").clicked() {
            self.settings_open = false;
        }
    }

    /// バーのラベルを表示言語に合わせて返す（用語は実UIに準拠）
    fn bar_label(&self, b: &LimitBar) -> String {
        let jp = self.lang == LANG_JP;
        match b.kind.as_str() {
            "session" => if jp { "現在のセッション" } else { "Session" }.to_string(),
            "weekly" => if jp { "週間制限" } else { "Weekly" }.to_string(),
            "monthly" => if jp { "月間利用上限" } else { "Monthly" }.to_string(),
            // モデル別の週間枠（例: Fable）は実UI同様そのモデル名で表示
            "weekly_scoped" => b.scope.clone().unwrap_or_else(|| "scoped".into()),
            _ => b.label.clone(),
        }
    }

    fn credit_label(&self) -> &'static str {
        if self.lang == LANG_JP {
            "利用クレジット"
        } else {
            "Credit"
        }
    }
}

/// 設定画面のサービス状態（ヘッダ表示用）。実際の取得結果に基づく。
#[derive(Clone, Copy)]
enum SvcStatus {
    LoggedIn,
    LoggedOut,
    Failed,
    Checking,
}

/// 情報入力済み(configured)＋実取得結果からログイン状態を判定
fn svc_status(configured: bool, usage: Option<&ServiceUsage>) -> SvcStatus {
    if !configured {
        return SvcStatus::LoggedOut;
    }
    match usage {
        None => SvcStatus::Checking,
        Some(u) if u.error.is_some() || !u.logged_in => SvcStatus::Failed,
        Some(_) => SvcStatus::LoggedIn,
    }
}

fn status_header(name: &str, st: SvcStatus) -> egui::RichText {
    let (label, color) = match st {
        SvcStatus::LoggedIn => ("● ログイン中", egui::Color32::from_rgb(90, 190, 110)),
        SvcStatus::LoggedOut => ("○ 未ログイン", egui::Color32::GRAY),
        SvcStatus::Failed => ("✕ ログイン失敗", egui::Color32::from_rgb(220, 90, 90)),
        SvcStatus::Checking => ("… 確認中", egui::Color32::from_rgb(220, 160, 60)),
    };
    egui::RichText::new(format!("{name}   {label}")).strong().color(color)
}

/// 四角いプログレスバー（角丸なし・枠付き・テキストを重ねない）
fn draw_bar(ui: &mut egui::Ui, frac: f32, color: egui::Color32) {
    let width = ui.available_width();
    let height = 12.0;
    let (rect, _resp) = ui.allocate_exact_size(egui::vec2(width, height), egui::Sense::hover());

    let dark = ui.visuals().dark_mode;
    let track = if dark {
        egui::Color32::from_gray(45)
    } else {
        egui::Color32::from_gray(222)
    };
    let border = if dark {
        egui::Color32::from_gray(110)
    } else {
        egui::Color32::from_gray(150)
    };

    let painter = ui.painter();
    painter.rect_filled(rect, 0.0, track);
    let fill_rect =
        egui::Rect::from_min_size(rect.min, egui::vec2(rect.width() * frac, rect.height()));
    painter.rect_filled(fill_rect, 0.0, color);
    painter.rect_stroke(rect, 0.0, egui::Stroke::new(1.0, border));
}

fn service_color(name: &str) -> egui::Color32 {
    if name.starts_with("Claude") {
        egui::Color32::from_rgb(78, 172, 110)
    } else if name.starts_with("Codex") {
        egui::Color32::from_rgb(80, 140, 220)
    } else {
        egui::Color32::from_rgb(160, 120, 200)
    }
}

/// 日本語表示のため、OSのシステムフォントを可能なら読み込む（バイナリを肥大化させない）。
fn setup_fonts(ctx: &egui::Context) {
    const CANDIDATES: &[&str] = &[
        "C:/Windows/Fonts/YuGothR.ttc",
        "C:/Windows/Fonts/meiryo.ttc",
        "C:/Windows/Fonts/msgothic.ttc",
        "/System/Library/Fonts/ヒラギノ角ゴシック W3.ttc",
        "/Library/Fonts/Arial Unicode.ttf",
        "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
        "/usr/share/fonts/truetype/noto/NotoSansCJK-Regular.ttc",
    ];
    for path in CANDIDATES {
        if let Ok(bytes) = std::fs::read(path) {
            let mut fonts = egui::FontDefinitions::default();
            fonts
                .font_data
                .insert("jp".to_owned(), egui::FontData::from_owned(bytes));
            fonts
                .families
                .entry(egui::FontFamily::Proportional)
                .or_default()
                .insert(0, "jp".to_owned());
            fonts
                .families
                .entry(egui::FontFamily::Monospace)
                .or_default()
                .push("jp".to_owned());
            ctx.set_fonts(fonts);
            return;
        }
    }
}
