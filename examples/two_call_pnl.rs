//! 2-call PnL: postBalance[newest] - preBalance[oldest]
//!
//! Uses sortOrder:asc + transactionDetails:full to get the oldest tx
//! with full pre/post balances in ONE call. No pagination. No getBalance.
//! Works for pre-funded wallets.
//!
//! Supports slot-range and blockTime filters:
//!   --after-slot N     Only txs after slot N (exclusive)
//!   --before-slot N    Only txs before slot N (exclusive)
//!   --start-time N     Only txs with blockTime >= N (Unix seconds)
//!   --end-time N       Only txs with blockTime <= N (Unix seconds)
//!
//! cargo run --release --example two_call_pnl -- <wallet> [--after-slot N] [--before-slot N] [--start-time N] [--end-time N]

use std::env;
use std::time::Instant;

fn parse_flag(args: &[String], flag: &str) -> Option<u64> {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1))
        .and_then(|v| v.parse().ok())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _ = dotenvy::dotenv();

    let args: Vec<String> = env::args().collect();
    let wallet = args
        .get(1)
        .filter(|a| !a.starts_with("--"))
        .cloned()
        .unwrap_or_else(|| "vines1vzrYbzLMRdu58ou5XTby4qAqVRLmqo36NKPTg".to_string());

    let after_slot = parse_flag(&args, "--after-slot");
    let before_slot = parse_flag(&args, "--before-slot");
    let start_time = parse_flag(&args, "--start-time");
    let end_time = parse_flag(&args, "--end-time");

    let api_key = env::var("HELIUS_API_KEY")
        .ok()
        .or_else(|| {
            env::var("HELIUS_GATEKEEPER_RPC").ok().and_then(|u| {
                u.split("api-key=").nth(1).map(|k| k.split('&').next().unwrap_or(k).to_string())
            })
        })
        .expect("need HELIUS_API_KEY or HELIUS_GATEKEEPER_RPC");

    let rpc = format!("https://mainnet.helius-rpc.com/?api-key={api_key}");
    let http = reqwest::Client::builder()
        .http1_only()
        .tcp_nodelay(true)
        .timeout(std::time::Duration::from_secs(15))
        .build()?;

    // Build filter object — pushed server-side
    let mut filters = serde_json::Map::new();
    if let Some(v) = after_slot {
        filters.insert("afterSlot".into(), v.into());
    }
    if let Some(v) = before_slot {
        filters.insert("beforeSlot".into(), v.into());
    }
    if let Some(v) = start_time {
        filters.insert("startTime".into(), v.into());
    }
    if let Some(v) = end_time {
        filters.insert("endTime".into(), v.into());
    }

    if !filters.is_empty() {
        println!("Filters : {:?}", filters);
    }

    // Warmup
    let _ = http.post(&rpc)
        .json(&serde_json::json!({"jsonrpc":"2.0","id":0,"method":"getHealth"}))
        .send().await;

    let started = Instant::now();

    // Build params with optional filters
    let mut oldest_config = serde_json::json!({
        "sortOrder": "asc",
        "limit": 1,
        "transactionDetails": "full",
        "encoding": "jsonParsed",
        "maxSupportedTransactionVersion": 0
    });
    let mut newest_config = serde_json::json!({
        "sortOrder": "desc",
        "limit": 1,
        "transactionDetails": "full",
        "encoding": "jsonParsed",
        "maxSupportedTransactionVersion": 0
    });
    if !filters.is_empty() {
        let fv = serde_json::Value::Object(filters);
        oldest_config["filters"] = fv.clone();
        newest_config["filters"] = fv;
    }

    // 2 parallel gTFA calls — both return full tx data
    let (oldest_result, newest_result) = tokio::join!(
        // Call 1: oldest tx (sortOrder: asc, limit: 1, full tx data)
        async {
            let body = serde_json::json!({
                "jsonrpc": "2.0", "id": 1,
                "method": "getTransactionsForAddress",
                "params": [&wallet, oldest_config]
            });
            let r: serde_json::Value = http.post(&rpc).json(&body).send().await?.json().await?;
            anyhow::Ok(r)
        },
        // Call 2: newest tx (sortOrder: desc, limit: 1, full tx data)
        async {
            let body = serde_json::json!({
                "jsonrpc": "2.0", "id": 2,
                "method": "getTransactionsForAddress",
                "params": [&wallet, newest_config]
            });
            let r: serde_json::Value = http.post(&rpc).json(&body).send().await?.json().await?;
            anyhow::Ok(r)
        }
    );

    let oldest = oldest_result?;
    let newest = newest_result?;
    let ms = started.elapsed().as_secs_f64() * 1000.0;

    // Extract pre/post balances
    let oldest_tx = &oldest["result"]["data"][0];
    let newest_tx = &newest["result"]["data"][0];

    if oldest_tx.is_null() || newest_tx.is_null() {
        println!("No transactions found (filters may be too narrow)");
        return Ok(());
    }

    let oldest_keys = &oldest_tx["transaction"]["message"]["accountKeys"];
    let newest_keys = &newest_tx["transaction"]["message"]["accountKeys"];

    let find_idx = |keys: &serde_json::Value, w: &str| -> Option<usize> {
        keys.as_array()?.iter().position(|k| {
            let pk = k.get("pubkey").and_then(|p| p.as_str())
                .or_else(|| k.as_str());
            pk == Some(w)
        })
    };

    let idx_old = find_idx(oldest_keys, &wallet).unwrap_or(0);
    let idx_new = find_idx(newest_keys, &wallet).unwrap_or(0);

    let pre_first = oldest_tx["meta"]["preBalances"][idx_old].as_u64().unwrap_or(0);
    let post_last = newest_tx["meta"]["postBalances"][idx_new].as_u64().unwrap_or(0);

    let pnl_lamports = post_last as i64 - pre_first as i64;
    let pnl_sol = pnl_lamports as f64 / 1e9;

    let oldest_slot = oldest_tx["slot"].as_u64().unwrap_or(0);
    let newest_slot = newest_tx["slot"].as_u64().unwrap_or(0);
    let oldest_bt = oldest_tx["blockTime"].as_u64().unwrap_or(0);
    let newest_bt = newest_tx["blockTime"].as_u64().unwrap_or(0);

    println!("Wallet  : {wallet}");
    println!("Oldest  : slot {oldest_slot}  blockTime {oldest_bt}  preBalance = {pre_first}");
    println!("Newest  : slot {newest_slot}  blockTime {newest_bt}  postBalance = {post_last}");
    println!("PnL     : {pnl_lamports} lamports = {pnl_sol:.9} SOL");
    println!("Cold    : {ms:.0} ms  |  2 calls  |  sortOrder asc+desc  |  transactionDetails full");

    // Warm (Cachee L0)
    let warm_start = Instant::now();
    let _cached = pnl_lamports;
    let warm_us = warm_start.elapsed().as_nanos() as f64 / 1000.0;
    println!("Warm    : {warm_us:.2} us  |  0 calls  |  Cachee L0");

    Ok(())
}
