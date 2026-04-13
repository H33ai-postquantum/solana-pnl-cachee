//! Nx2 balance curve: 20 time windows × 2 calls each = full PnL history in 1 RTT.
//!
//! cargo run --release --example balance_curve -- <wallet> [windows]

use std::env;
use std::time::Instant;
use futures::future::join_all;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _ = dotenvy::dotenv();

    let args: Vec<String> = env::args().collect();
    let wallet = args.get(1).filter(|a| !a.starts_with('-')).cloned()
        .unwrap_or_else(|| "vines1vzrYbzLMRdu58ou5XTby4qAqVRLmqo36NKPTg".to_string());
    let num_windows: usize = args.get(2).and_then(|v| v.parse().ok()).unwrap_or(20);

    let api_key = env::var("HELIUS_API_KEY")
        .ok()
        .or_else(|| env::var("HELIUS_GATEKEEPER_RPC").ok().and_then(|u|
            u.split("api-key=").nth(1).map(|k| k.split('&').next().unwrap_or(k).to_string())))
        .expect("need HELIUS_API_KEY or HELIUS_GATEKEEPER_RPC");

    let rpc = format!("https://mainnet.helius-rpc.com/?api-key={api_key}");
    let http = reqwest::Client::builder()
        .http1_only()
        .tcp_nodelay(true)
        .pool_max_idle_per_host(64)
        .timeout(std::time::Duration::from_secs(15))
        .build()?;

    // Warmup
    let _ = http.post(&rpc)
        .json(&serde_json::json!({"jsonrpc":"2.0","id":0,"method":"getHealth"}))
        .send().await;

    // Step 1: get bounds (oldest + newest slot) — reuse the 3-call pattern
    let (bal_res, oldest_res, newest_res) = tokio::join!(
        async {
            let body = serde_json::json!({"jsonrpc":"2.0","id":1,"method":"getBalance","params":[&wallet]});
            let r: serde_json::Value = http.post(&rpc).json(&body).send().await?.json().await?;
            anyhow::Ok(r["result"]["value"].as_u64().unwrap_or(0))
        },
        async {
            let body = serde_json::json!({"jsonrpc":"2.0","id":2,"method":"getTransactionsForAddress",
                "params":[&wallet,{"sortOrder":"asc","limit":1}]});
            let r: serde_json::Value = http.post(&rpc).json(&body).send().await?.json().await?;
            let slot = r["result"]["data"][0]["slot"].as_u64().unwrap_or(0);
            anyhow::Ok(slot)
        },
        async {
            let body = serde_json::json!({"jsonrpc":"2.0","id":3,"method":"getTransactionsForAddress",
                "params":[&wallet,{"sortOrder":"desc","limit":1}]});
            let r: serde_json::Value = http.post(&rpc).json(&body).send().await?.json().await?;
            let slot = r["result"]["data"][0]["slot"].as_u64().unwrap_or(0);
            anyhow::Ok(slot)
        }
    );

    let current_balance = bal_res?;
    let oldest_slot = oldest_res?;
    let newest_slot = newest_res?;

    if oldest_slot == 0 || newest_slot == 0 {
        println!("No transactions found.");
        return Ok(());
    }

    let span = newest_slot - oldest_slot;
    let window_size = span / num_windows as u64;

    println!("Wallet  : {wallet}");
    println!("Balance : {} lamports = {:.9} SOL", current_balance, current_balance as f64 / 1e9);
    println!("Slots   : {} - {} (span {})", oldest_slot, newest_slot, span);
    println!("Windows : {} × {} slots each", num_windows, window_size);
    println!();

    // Step 2: fire Nx2 calls — all parallel
    let started = Instant::now();
    let mut futures = Vec::with_capacity(num_windows * 2);

    for i in 0..num_windows {
        let start_slot = oldest_slot + (window_size * i as u64);
        let end_slot = if i == num_windows - 1 { newest_slot + 1 } else { oldest_slot + (window_size * (i + 1) as u64) };

        let http_c = http.clone();
        let rpc_c = rpc.clone();
        let wallet_c = wallet.clone();
        let window_idx = i;

        // Call A: oldest tx in this window (asc, full)
        futures.push(tokio::spawn({
            let http = http_c.clone();
            let rpc = rpc_c.clone();
            let w = wallet_c.clone();
            async move {
                let body = serde_json::json!({
                    "jsonrpc":"2.0","id": window_idx * 2,
                    "method":"getTransactionsForAddress",
                    "params":[&w, {
                        "sortOrder":"asc","limit":1,
                        "transactionDetails":"full",
                        "encoding":"jsonParsed",
                        "maxSupportedTransactionVersion":0,
                        "filters":{"slot":{"gte": start_slot, "lt": end_slot}}
                    }]
                });
                let r: serde_json::Value = http.post(&rpc).json(&body).send().await?.json().await?;
                anyhow::Ok(("asc", window_idx, r))
            }
        }));

        // Call B: newest tx in this window (desc, full)
        futures.push(tokio::spawn({
            let http = http_c;
            let rpc = rpc_c;
            let w = wallet_c;
            async move {
                let body = serde_json::json!({
                    "jsonrpc":"2.0","id": window_idx * 2 + 1,
                    "method":"getTransactionsForAddress",
                    "params":[&w, {
                        "sortOrder":"desc","limit":1,
                        "transactionDetails":"full",
                        "encoding":"jsonParsed",
                        "maxSupportedTransactionVersion":0,
                        "filters":{"slot":{"gte": start_slot, "lt": end_slot}}
                    }]
                });
                let r: serde_json::Value = http.post(&rpc).json(&body).send().await?.json().await?;
                anyhow::Ok(("desc", window_idx, r))
            }
        }));
    }

    let results = join_all(futures).await;
    let ms = started.elapsed().as_secs_f64() * 1000.0;

    // Parse results into window data
    let find_balance = |resp: &serde_json::Value, w: &str, field: &str| -> Option<u64> {
        let tx = &resp["result"]["data"][0];
        if tx.is_null() { return None; }
        let keys = tx["transaction"]["message"]["accountKeys"].as_array()?;
        let idx = keys.iter().position(|k|
            k.get("pubkey").and_then(|p| p.as_str()).or_else(|| k.as_str()) == Some(w))?;
        tx["meta"][field][idx].as_u64()
    };

    let mut windows: Vec<(usize, Option<u64>, Option<u64>)> = (0..num_windows).map(|i| (i, None, None)).collect();

    let mut ok_count = 0;
    let mut err_count = 0;
    for result in results {
        match result {
            Ok(Ok((dir, idx, resp))) => {
                ok_count += 1;
                if dir == "asc" {
                    windows[idx].1 = find_balance(&resp, &wallet, "preBalances");
                } else {
                    windows[idx].2 = find_balance(&resp, &wallet, "postBalances");
                }
            }
            Ok(Err(e)) => { err_count += 1; eprintln!("call error: {e}"); }
            Err(e) => { err_count += 1; eprintln!("task error: {e}"); }
        }
    }

    // Print balance curve
    println!("{:<6} {:<14} {:<14} {:<18} {:<18} {:<15}", "Win", "Start", "End", "preBalance", "postBalance", "Window PnL");
    println!("{}", "-".repeat(90));

    let mut total_pnl: i64 = 0;
    for (i, pre, post) in &windows {
        let start_t = oldest_slot + (window_size * *i as u64);
        let end_t = if *i == num_windows - 1 { newest_slot } else { start_t + window_size };
        let pnl = match (pre, post) {
            (Some(p), Some(q)) => {
                let d = *q as i64 - *p as i64;
                total_pnl += d;
                format!("{:+.9} SOL", d as f64 / 1e9)
            }
            _ => "no activity".to_string(),
        };
        println!("{:<6} {:<14} {:<14} {:<18} {:<18} {}", i, start_t, end_t,
            pre.map(|v| v.to_string()).unwrap_or("-".into()),
            post.map(|v| v.to_string()).unwrap_or("-".into()),
            pnl);
    }

    println!("{}", "-".repeat(90));
    println!("Total PnL  : {} lamports = {:.9} SOL", total_pnl, total_pnl as f64 / 1e9);
    println!("Verify     : current balance = {} lamports = {:.9} SOL", current_balance, current_balance as f64 / 1e9);
    println!("Calls      : {} parallel ({} ok, {} err)", num_windows * 2, ok_count, err_count);
    println!("Cold       : {ms:.0} ms for {} windows  |  all parallel  |  1 RTT", num_windows);

    Ok(())
}
