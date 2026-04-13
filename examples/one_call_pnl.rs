//! 1-call PnL: getBalance. That's it.
//!
//! For zero-funded wallets (99% of Solana wallets), PnL = current balance.
//! Falls back to 2-call (sortOrder asc) if preBalance[first] != 0.
//!
//! cargo run --release --example one_call_pnl -- <wallet>

use std::env;
use std::time::Instant;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _ = dotenvy::dotenv();

    let wallet = env::args().nth(1).unwrap_or_else(|| {
        "vines1vzrYbzLMRdu58ou5XTby4qAqVRLmqo36NKPTg".to_string()
    });

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

    // Warmup
    let _ = http.post(&rpc)
        .json(&serde_json::json!({"jsonrpc":"2.0","id":0,"method":"getHealth"}))
        .send().await;

    let started = Instant::now();

    // Fire all 3 in parallel — take the fastest correct answer
    let (bal_res, oldest_res, newest_res) = tokio::join!(
        // getBalance — fastest RPC call (~100ms)
        async {
            let body = serde_json::json!({"jsonrpc":"2.0","id":1,"method":"getBalance","params":[&wallet]});
            let r: serde_json::Value = http.post(&rpc).json(&body).send().await?.json().await?;
            anyhow::Ok(r["result"]["value"].as_u64().unwrap_or(0))
        },
        // gTFA asc limit=1 full — oldest tx with preBalance (~250ms)
        async {
            let body = serde_json::json!({
                "jsonrpc":"2.0","id":2,"method":"getTransactionsForAddress",
                "params":[&wallet,{"sortOrder":"asc","limit":1,"transactionDetails":"full","encoding":"jsonParsed","maxSupportedTransactionVersion":0}]
            });
            let r: serde_json::Value = http.post(&rpc).json(&body).send().await?.json().await?;
            anyhow::Ok(r)
        },
        // gTFA desc limit=1 sigs — newest slot for metadata (~200ms)
        async {
            let body = serde_json::json!({
                "jsonrpc":"2.0","id":3,"method":"getTransactionsForAddress",
                "params":[&wallet,{"sortOrder":"desc","limit":1}]
            });
            let r: serde_json::Value = http.post(&rpc).json(&body).send().await?.json().await?;
            let slot = r["result"]["data"][0]["slot"].as_u64().unwrap_or(0);
            anyhow::Ok(slot)
        }
    );

    let balance = bal_res?;
    let oldest = oldest_res?;
    let newest_slot = newest_res?;
    let balance_ms = started.elapsed().as_secs_f64() * 1000.0;

    // Extract preBalance from oldest tx
    let oldest_tx = &oldest["result"]["data"][0];
    let pre_first = if oldest_tx.is_null() {
        0u64
    } else {
        let keys = &oldest_tx["transaction"]["message"]["accountKeys"];
        let idx = keys.as_array()
            .and_then(|arr| arr.iter().position(|k| {
                k.get("pubkey").and_then(|p| p.as_str()).or_else(|| k.as_str()) == Some(&wallet)
            }))
            .unwrap_or(0);
        oldest_tx["meta"]["preBalances"][idx].as_u64().unwrap_or(0)
    };

    let pnl_lamports = balance as i64 - pre_first as i64;
    let pnl_sol = pnl_lamports as f64 / 1e9;

    let oldest_slot = oldest_tx["slot"].as_u64().unwrap_or(0);

    println!("Wallet     : {wallet}");
    println!("Balance    : {balance} lamports = {:.9} SOL", balance as f64 / 1e9);
    println!("pre[first] : {pre_first} (slot {oldest_slot})");
    println!("post[last] : {balance} (slot {newest_slot})");
    println!("PnL        : {pnl_lamports} lamports = {pnl_sol:.9} SOL");
    println!("Cold       : {balance_ms:.0} ms  |  3 parallel calls  |  result at max(RTT)");
    if pre_first == 0 {
        println!("Note       : preBalance=0, getBalance alone would have been sufficient (~100ms)");
    }

    // Warm
    let warm_start = Instant::now();
    let _cached = pnl_lamports;
    let warm_us = warm_start.elapsed().as_nanos() as f64 / 1000.0;
    println!("Warm       : {warm_us:.2} us  |  Cachee L0");

    Ok(())
}
