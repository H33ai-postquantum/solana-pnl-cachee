//! Live Helius contest-grade benchmark.
//!
//! ```
//! cargo run --release --example live_helius_bench -- <wallet>
//! ```
//!
//! Test wallets by class:
//!
//! - **Busy**:     `CapuXNQoDviLvU1PxFiizLgPNQCxrsag1uMeyk6zLVps` (JUP treasury)
//! - **Busy**:     `9BBBcuLKsFMKARfQdw93Ltf4fgwaoGyGEHe6xJzEDA6G` (Raydium AMM)
//! - **Periodic**: `vines1vzrYbzLMRdu58ou5XTby4qAqVRLmqo36NKPTg` (~150 txs)
//! - **Sparse**:   any personal wallet with ≤100 txs
//!
//! The benchmark fires five passes against the target wallet,
//! plus one interference run against a different wallet between
//! passes 2 and 3, and reports cold / warm / warm+interference
//! wall-clock times plus cache-hit statistics.

use std::env;
use std::sync::Arc;
use std::time::Instant;

use solana_pnl_cachee::cache_layer::CacheLayer;
use solana_pnl_cachee::live::{solve_wallet_pnl_live, HeliusClient};
use solana_pnl_cachee::price::PriceHistory;

const INTERFERENCE_WALLET: &str = "9BBBcuLKsFMKARfQdw93Ltf4fgwaoGyGEHe6xJzEDA6G";

fn extract_api_key(rpc_url: &str) -> Option<String> {
    rpc_url
        .split_once("api-key=")
        .map(|(_, k)| k.split('&').next().unwrap_or(k).to_string())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let _ = dotenvy::dotenv();
    // Surface pipeline errors at WARN and above so silent parse
    // failures can't hide behind `txs=0` summary rows.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .with_writer(std::io::stderr)
        .init();

    let args: Vec<String> = env::args().collect();
    let wallet = args
        .get(1)
        .cloned()
        .unwrap_or_else(|| "vines1vzrYbzLMRdu58ou5XTby4qAqVRLmqo36NKPTg".to_string());

    let api_key = env::var("HELIUS_API_KEY")
        .ok()
        .or_else(|| env::var("HELIUS_BETA_RPC").ok().and_then(|u| extract_api_key(&u)))
        .or_else(|| env::var("HELIUS_GATEKEEPER_RPC").ok().and_then(|u| extract_api_key(&u)))
        .or_else(|| env::var("HELIUS_PARSE_TX").ok().and_then(|u| extract_api_key(&u)))
        .ok_or("HELIUS_API_KEY not set and no HELIUS_*_RPC env var contains ?api-key=")?;

    let client = HeliusClient::new(api_key)?;

    // Warmup runs off-clock so TCP + TLS + DNS are already primed.
    client.warmup().await.ok();

    let cache = Arc::new(CacheLayer::new());
    let prices = PriceHistory::constant(150.0);

    println!("=== Live Helius benchmark (contest-grade Cachee solver) ===");
    println!("Wallet: {wallet}");
    println!("Endpoint: https://beta.helius-rpc.com (gTFA + Enhanced Parse)");
    println!();

    // -----------------------------------------------------------------
    // Pass 1 — cold. Fresh cache, full gTFA + parse pipeline.
    // -----------------------------------------------------------------
    let started = Instant::now();
    let r1 = solve_wallet_pnl_live(&client, cache.clone(), &prices, &wallet).await?;
    let cold_elapsed = started.elapsed();
    println!(
        "Pass 1 (cold)   | {:>9.2} ms | txs={:5} | rpc_calls~{:3} | net_sol={:+.4}",
        cold_elapsed.as_secs_f64() * 1000.0,
        r1.summary.tx_count,
        r1.rpc_calls,
        r1.summary.net_sol(),
    );

    // -----------------------------------------------------------------
    // Pass 2 — warm. Same cache, cached history blob.
    // -----------------------------------------------------------------
    let started = Instant::now();
    let r2 = solve_wallet_pnl_live(&client, cache.clone(), &prices, &wallet).await?;
    let warm_elapsed = started.elapsed();
    println!(
        "Pass 2 (warm)   | {:>9.2} ms | txs={:5} | rpc_calls~{:3} | net_sol={:+.4}",
        warm_elapsed.as_secs_f64() * 1000.0,
        r2.summary.tx_count,
        r2.rpc_calls,
        r2.summary.net_sol(),
    );

    // -----------------------------------------------------------------
    // Interference — unrelated wallet, same cache.
    // -----------------------------------------------------------------
    if wallet != INTERFERENCE_WALLET {
        println!("Interference run against {INTERFERENCE_WALLET}");
        let _ = solve_wallet_pnl_live(
            &client,
            cache.clone(),
            &prices,
            INTERFERENCE_WALLET,
        )
        .await;
    }

    // -----------------------------------------------------------------
    // Pass 3 — warm after interference.
    // -----------------------------------------------------------------
    let started = Instant::now();
    let r3 = solve_wallet_pnl_live(&client, cache.clone(), &prices, &wallet).await?;
    let warm_after_elapsed = started.elapsed();
    println!(
        "Pass 3 (warm+n) | {:>9.2} ms | txs={:5} | rpc_calls~{:3} | net_sol={:+.4}",
        warm_after_elapsed.as_secs_f64() * 1000.0,
        r3.summary.tx_count,
        r3.rpc_calls,
        r3.summary.net_sol(),
    );

    // -----------------------------------------------------------------
    // Summary
    // -----------------------------------------------------------------
    let cold_ms = cold_elapsed.as_secs_f64() * 1000.0;
    let warm_ms = warm_elapsed.as_secs_f64() * 1000.0;
    let speedup = if warm_ms > 0.0 { cold_ms / warm_ms } else { 0.0 };

    let stats = cache.combined_stats();
    let total = stats.l0_hits + stats.l1_hits + stats.misses;
    let hit_ratio = if total == 0 {
        0.0
    } else {
        (stats.l0_hits + stats.l1_hits) as f64 / total as f64 * 100.0
    };

    println!();
    println!("=== Summary ===");
    println!("Cold        : {cold_ms:>9.2} ms");
    println!("Warm        : {warm_ms:>9.2} ms");
    println!(
        "Warm+interf : {:>9.2} ms",
        warm_after_elapsed.as_secs_f64() * 1000.0
    );
    println!("Speedup     : {speedup:>9.2}x  (cold / warm)");
    println!();
    println!(
        "Cache       : L0 hits={}  L1 hits={}  misses={}  hit ratio={:.1}%",
        stats.l0_hits, stats.l1_hits, stats.misses, hit_ratio
    );
    println!(
        "Memory      : {:.1} KiB across tx/price/density caches",
        stats.memory_bytes as f64 / 1024.0
    );

    Ok(())
}
