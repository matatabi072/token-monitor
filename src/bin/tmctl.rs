//! tmctl — Token Monitor の CLI/PoC。コンソールに使用状況を表示する。

use anyhow::Result;
use token_monitor::{
    bar_graph, fetch_claude, fetch_codex, http_client, load_config, remaining, ServiceUsage,
};

fn print_service(s: &ServiceUsage, verbose: bool) {
    if let Some(err) = &s.error {
        if verbose {
            println!("  {}  … 取得エラー: {err}", s.service);
        } else {
            println!("  {}  … 取得エラー（詳細は --verbose を指定）", s.service);
        }
        return;
    }
    if !s.logged_in {
        // 仕様 3.2-3: 未ログインは非表示（詰める）。CLI では明示ログのみ。
        println!("  {}  … 未ログイン/セッション切れ（非表示対象）", s.service);
        return;
    }
    println!("  {}", s.service);
    for b in &s.bars {
        let star = if b.active { "*" } else { " " };
        println!(
            "   {star} {:<16} {} {:>3.0}% ({})",
            b.label,
            bar_graph(b.percent),
            b.percent,
            remaining(b.resets_at)
        );
    }
    if let Some(c) = &s.credit {
        let mark = if c.enabled { "ON " } else { "off" };
        println!("     Credit [{mark}] {}", c.detail);
    }
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!("tmctl — Token Monitor CLI");
        println!("使い方: tmctl [--verbose|-v]");
        println!("  --verbose, -v   設定値の文字数や取得エラーの詳細を表示する");
        return Ok(());
    }
    let verbose = args.iter().any(|a| a == "--verbose" || a == "-v");

    let cfg = load_config()?;

    // --- 認証情報のサニティチェック（値そのものは表示しない）。--verbose 時のみ表示。 ---
    if verbose {
        if let Some(c) = cfg.claude.as_ref() {
            let ph_key = c.session_key.contains("XXXX");
            let ph_org = c.org_id.starts_with("0000");
            eprintln!(
                "  [debug] claude cfg: session_key {} 文字{}, org_id {} 文字{}",
                c.session_key.len(),
                if ph_key { " ← ⚠プレースホルダのまま" } else { "" },
                c.org_id.len(),
                if ph_org { " ← ⚠プレースホルダのまま" } else { "" },
            );
        }
        if let Some(c) = cfg.codex.as_ref() {
            let ph_tok = c.session_token.len() < 20;
            eprintln!(
                "  [debug] codex cfg: session_token {} 文字{}",
                c.session_token.len(),
                if ph_tok { " ← ⚠プレースホルダ/短すぎ" } else { "" },
            );
        }
    }

    let client = http_client()?;

    println!("=== Token Monitor (CLI) ===");

    if let Some(c) = cfg.claude.as_ref().filter(|c| c.enabled) {
        match fetch_claude(&client, c) {
            Ok(s) => print_service(&s, verbose),
            Err(e) => print_service(&ServiceUsage::errored("ClaudeCode", format!("{e:#}")), verbose),
        }
    }
    if let Some(c) = cfg.codex.as_ref().filter(|c| c.enabled) {
        match fetch_codex(&client, c) {
            Ok(s) => print_service(&s, verbose),
            Err(e) => print_service(&ServiceUsage::errored("Codex", format!("{e:#}")), verbose),
        }
    }

    Ok(())
}
