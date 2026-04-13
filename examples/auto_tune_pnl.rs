//! Auto-tuned Nx2 balance curve: probes density first, picks optimal N, then fires.
//!
//! 1. Get bounds (oldest + newest slot) — 3 parallel calls
//! 2. Fire 4 pilot probes at log-spaced slot ranges — estimate tx density
//! 3. Pick optimal N based on density (sparse=few windows, busy=many)
//! 4. Fire Nx2 at optimal N — all 2N calls in parallel
//!
//! cargo run --release --example auto_tune_pnl -- <wallet>

use std::env;
use std::time::Instant;
use futures::future::join_all;

const PILOT_PROBES: usize = 4;
const MIN_WINDOWS: usize = 2;
const MAX_WINDOWS: usize = 100;
// Helius practical ceiling: ~200 concurrent gTFA calls before 429s
const MAX_PARALLEL_CALLS: usize = 200;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _ = dotenvy::dotenv();

    let wallet = env::args().nth(1).unwrap_or_else(|| {
        "vines1vzrYbzLMRdu58ou5XTby4qAqVRLmqo36NKPTg".to_string()
    });

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

    // Warmup connection
    let _ = http.post(&rpc)
        .json(&serde_json::json!({"jsonrpc":"2.0","id":0,"method":"getHealth"}))
        .send().await;

    let total_start = Instant::now();

    // ── Phase 1: Get bounds ──────────────────────────────────────
    let phase1 = Instant::now();
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
            anyhow::Ok(r["result"]["data"][0]["slot"].as_u64().unwrap_or(0))
        },
        async {
            let body = serde_json::json!({"jsonrpc":"2.0","id":3,"method":"getTransactionsForAddress",
                "params":[&wallet,{"sortOrder":"desc","limit":1}]});
            let r: serde_json::Value = http.post(&rpc).json(&body).send().await?.json().await?;
            anyhow::Ok(r["result"]["data"][0]["slot"].as_u64().unwrap_or(0))
        }
    );

    let balance = bal_res?;
    let oldest_slot = oldest_res?;
    let newest_slot = newest_res?;
    let phase1_ms = phase1.elapsed().as_secs_f64() * 1000.0;

    if oldest_slot == 0 || newest_slot == 0 {
        println!("No transactions found.");
        return Ok(());
    }

    let span = newest_slot - oldest_slot;
    println!("Wallet   : {wallet}");
    println!("Balance  : {} lamports = {:.9} SOL", balance, balance as f64 / 1e9);
    println!("Slots    : {} – {} (span {})", oldest_slot, newest_slot, span);
    println!("Phase 1  : {phase1_ms:.0} ms (bounds)");
    println!();

    // ── Phase 2: Pilot probes — estimate density ─────────────────
    let phase2 = Instant::now();
    let mut probe_futures = Vec::with_capacity(PILOT_PROBES);

    for i in 0..PILOT_PROBES {
        // Log-spaced: probe at 0%, 25%, 50%, 75% of the slot range
        let probe_start = oldest_slot + (span * i as u64) / PILOT_PROBES as u64;
        let probe_end = oldest_slot + (span * (i + 1) as u64) / PILOT_PROBES as u64;

        let http_c = http.clone();
        let rpc_c = rpc.clone();
        let wallet_c = wallet.clone();

        probe_futures.push(tokio::spawn(async move {
            let body = serde_json::json!({
                "jsonrpc":"2.0","id": i,
                "method":"getTransactionsForAddress",
                "params":[&wallet_c, {
                    "sortOrder":"asc","limit":100,
                    "filters":{"slot":{"gte": probe_start, "lt": probe_end}}
                }]
            });
            let r: serde_json::Value = http_c.post(&rpc_c).json(&body).send().await?.json().await?;
            let count = r["result"]["data"].as_array().map(|a| a.len()).unwrap_or(0);
            anyhow::Ok((i, probe_start, probe_end, count))
        }));
    }

    let probe_results = join_all(probe_futures).await;
    let phase2_ms = phase2.elapsed().as_secs_f64() * 1000.0;

    let mut total_txs_sampled = 0usize;
    let mut total_slots_sampled = 0u64;
    let mut probe_details = Vec::new();

    println!("Phase 2  : {phase2_ms:.0} ms (density probes)");
    println!("{:<8} {:<14} {:<14} {:<8}", "Probe", "Start", "End", "Txs");
    println!("{}", "-".repeat(50));

    for result in probe_results {
        match result {
            Ok(Ok((i, start, end, count))) => {
                println!("{:<8} {:<14} {:<14} {:<8}", i, start, end, count);
                total_txs_sampled += count;
                total_slots_sampled += end - start;
                probe_details.push((i, start, end, count));
            }
            Ok(Err(e)) => eprintln!("probe error: {e}"),
            Err(e) => eprintln!("task error: {e}"),
        }
    }

    // Estimate total transactions and density
    let density = if total_slots_sampled > 0 {
        total_txs_sampled as f64 / total_slots_sampled as f64
    } else {
        0.0
    };
    let estimated_total_txs = (density * span as f64) as u64;

    println!();
    println!("Sampled  : {} txs across {} slots", total_txs_sampled, total_slots_sampled);
    println!("Density  : {:.6} txs/slot", density);
    println!("Est total: ~{} txs", estimated_total_txs);

    // ── Phase 3: Pick optimal N ──────────────────────────────────
    // Goal: each window should contain ~10-50 txs for good resolution
    // without wasting credits on empty windows
    let target_txs_per_window = 25;
    let optimal_n = if estimated_total_txs == 0 {
        MIN_WINDOWS
    } else {
        let n = (estimated_total_txs as f64 / target_txs_per_window as f64).ceil() as usize;
        // Clamp to MIN..MAX and ensure 2*N doesn't exceed parallel ceiling
        n.max(MIN_WINDOWS).min(MAX_WINDOWS).min(MAX_PARALLEL_CALLS / 2)
    };

    let total_calls = optimal_n * 2;
    println!();
    println!("┌─────────────────────────────────────────┐");
    println!("│  AUTO-TUNE: N = {:<4} ({} parallel calls) │", optimal_n, total_calls);
    println!("└─────────────────────────────────────────┘");
    println!();

    // ── Phase 4: Fire Nx2 at optimal N ───────────────────────────
    let phase4 = Instant::now();
    let window_size = span / optimal_n as u64;
    let mut futures = Vec::with_capacity(total_calls);

    for i in 0..optimal_n {
        let start_slot = oldest_slot + (window_size * i as u64);
        let end_slot = if i == optimal_n - 1 { newest_slot + 1 } else { oldest_slot + (window_size * (i + 1) as u64) };

        let http_c = http.clone();
        let rpc_c = rpc.clone();
        let wallet_c = wallet.clone();

        // Call A: oldest tx in window (asc)
        futures.push(tokio::spawn({
            let http = http_c.clone();
            let rpc = rpc_c.clone();
            let w = wallet_c.clone();
            async move {
                let body = serde_json::json!({
                    "jsonrpc":"2.0","id": i * 2,
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
                anyhow::Ok(("asc", i, r))
            }
        }));

        // Call B: newest tx in window (desc)
        futures.push(tokio::spawn({
            let http = http_c;
            let rpc = rpc_c;
            let w = wallet_c;
            async move {
                let body = serde_json::json!({
                    "jsonrpc":"2.0","id": i * 2 + 1,
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
                anyhow::Ok(("desc", i, r))
            }
        }));
    }

    let results = join_all(futures).await;
    let phase4_ms = phase4.elapsed().as_secs_f64() * 1000.0;

    // Parse results
    let find_balance = |resp: &serde_json::Value, w: &str, field: &str| -> Option<u64> {
        let tx = &resp["result"]["data"][0];
        if tx.is_null() { return None; }
        let keys = tx["transaction"]["message"]["accountKeys"].as_array()?;
        let idx = keys.iter().position(|k|
            k.get("pubkey").and_then(|p| p.as_str()).or_else(|| k.as_str()) == Some(w))?;
        tx["meta"][field][idx].as_u64()
    };

    let mut windows: Vec<(usize, Option<u64>, Option<u64>)> = (0..optimal_n).map(|i| (i, None, None)).collect();
    let mut ok_count = 0;
    let mut err_count = 0;
    let mut active_windows = 0;

    for result in results {
        match result {
            Ok(Ok((dir, idx, resp))) => {
                ok_count += 1;
                if dir == "asc" {
                    let pre = find_balance(&resp, &wallet, "preBalances");
                    if pre.is_some() { windows[idx].1 = pre; }
                } else {
                    let post = find_balance(&resp, &wallet, "postBalances");
                    if post.is_some() { windows[idx].2 = post; }
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
        let end_t = if *i == optimal_n - 1 { newest_slot } else { start_t + window_size };
        let pnl = match (pre, post) {
            (Some(p), Some(q)) => {
                let d = *q as i64 - *p as i64;
                total_pnl += d;
                active_windows += 1;
                format!("{:+.9} SOL", d as f64 / 1e9)
            }
            _ => "—".to_string(),
        };
        println!("{:<6} {:<14} {:<14} {:<18} {:<18} {}",
            i, start_t, end_t,
            pre.map(|v| v.to_string()).unwrap_or("—".into()),
            post.map(|v| v.to_string()).unwrap_or("—".into()),
            pnl);
    }

    let total_ms = total_start.elapsed().as_secs_f64() * 1000.0;
    let per_point = if active_windows > 0 { phase4_ms / active_windows as f64 } else { 0.0 };
    let credits = 3 + (PILOT_PROBES * 50) + (total_calls * 50); // bounds + probes + Nx2

    println!("{}", "-".repeat(90));
    println!("Total PnL    : {} lamports = {:.9} SOL", total_pnl, total_pnl as f64 / 1e9);
    println!("Verify       : current balance = {} lamports", balance);
    println!();
    println!("┌─────────────────────────────────────────────────────────┐");
    println!("│  RESULTS                                                │");
    println!("│  Auto-tuned N : {:<4}  ({} active / {} total windows)     │", optimal_n, active_windows, optimal_n);
    println!("│  Phase 1      : {:<6.0} ms  (bounds — 3 calls)            │", phase1_ms);
    println!("│  Phase 2      : {:<6.0} ms  (density — {} probes)          │", phase2_ms, PILOT_PROBES);
    println!("│  Phase 4      : {:<6.0} ms  (Nx2 — {} parallel calls)    │", phase4_ms, total_calls);
    println!("│  Total        : {:<6.0} ms  end-to-end                    │", total_ms);
    println!("│  Per point    : {:<6.2} ms  ({} active windows)            │", per_point, active_windows);
    println!("│  Credits      : {}                                      │", credits);
    println!("│  Calls        : {} ok, {} err                             │", ok_count, err_count);
    println!("└─────────────────────────────────────────────────────────┘");

    Ok(())
}
