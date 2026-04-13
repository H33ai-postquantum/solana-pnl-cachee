//! Cold-vs-warm cache demo — the runnable version of the
//! benchmark story. Prints a side-by-side comparison of the same
//! wallet queried twice through the same `CacheLayer`.
//!
//! ## Run it
//!
//! ```
//! cargo run --release --example cold_vs_warm_demo
//! ```
//!
//! ## What it shows
//!
//! - First run is cold — density cache miss, full probe + window
//!   fetch pipeline.
//! - Second run is warm — density cache hit, probes skipped,
//!   windows re-use cached tx blobs where signatures overlap.
//! - Third run is warm after cross-wallet interference — we run
//!   a completely unrelated wallet in between to prove Cachee's
//!   CacheeLFU admission keeps the hot wallet's entries in L0.

use std::sync::Arc;
use std::time::{Duration, Instant};

use solana_pnl_cachee::cache_layer::CacheLayer;
use solana_pnl_cachee::price::PriceHistory;
use solana_pnl_cachee::rpc::{MockRpc, RpcClient, TxRecord};
use solana_pnl_cachee::solver::{solve_wallet_pnl, SolverConfig};

fn synthetic_wallet(prefix: &str, tx_count: usize, seed_slot: u64) -> Vec<TxRecord> {
    (0..tx_count)
        .map(|i| {
            let slot = seed_slot + (i as u64) * 20_000;
            let magnitude = 500_000_000 - (i as i64 % 100) * 1_000_000;
            let sign = if i % 3 == 0 { 1 } else { -1 };
            TxRecord {
                signature: format!("{}-{}", prefix, i),
                slot,
                block_time: 1_700_000_000 + (i as u64) * 120,
                sol_delta_lamports: sign * magnitude,
                success: i % 37 != 0,
            }
        })
        .collect()
}

#[tokio::main]
async fn main() {
    let mut mock = MockRpc::new().with_latency(Duration::from_millis(5));
    mock.insert_history("hot-wallet", synthetic_wallet("hot", 500, 200_000_000));
    mock.insert_history("cold-wallet", synthetic_wallet("cold", 500, 210_000_000));
    let rpc: Arc<dyn RpcClient> = Arc::new(mock);

    let cache = Arc::new(CacheLayer::new());
    let prices = PriceHistory::constant(150.0);
    let config = SolverConfig::default();

    println!("=== Cold-vs-warm cache demo (Cachee Solana PnL solver) ===\n");

    // Pass 1: cold — fresh cache, nothing warmed up.
    let started = Instant::now();
    let report = solve_wallet_pnl(rpc.clone(), cache.clone(), &prices, "hot-wallet", &config)
        .await
        .expect("pass 1");
    println!(
        "Pass 1 (cold)   | {:>8.2} ms | txs={:4} | net_sol={:+8.4} | L0 hits={:4} misses={:4}",
        started.elapsed().as_secs_f64() * 1000.0,
        report.summary.tx_count,
        report.summary.net_sol(),
        report.cache_hits_l0,
        report.cache_misses,
    );

    // Pass 2: warm — same wallet, same cache. Density cache should
    // hit, probe phase should be skipped.
    let started = Instant::now();
    let report = solve_wallet_pnl(rpc.clone(), cache.clone(), &prices, "hot-wallet", &config)
        .await
        .expect("pass 2");
    println!(
        "Pass 2 (warm)   | {:>8.2} ms | txs={:4} | net_sol={:+8.4} | L0 hits={:4} misses={:4}",
        started.elapsed().as_secs_f64() * 1000.0,
        report.summary.tx_count,
        report.summary.net_sol(),
        report.cache_hits_l0,
        report.cache_misses,
    );

    // Cross-wallet interference: run a totally unrelated wallet
    // through the same cache. This is the admission test —
    // does CacheeLFU keep the hot wallet's entries in L0?
    let _ = solve_wallet_pnl(rpc.clone(), cache.clone(), &prices, "cold-wallet", &config)
        .await
        .expect("interference");

    // Pass 3: warm again, after interference. Should still be fast.
    let started = Instant::now();
    let report = solve_wallet_pnl(rpc.clone(), cache.clone(), &prices, "hot-wallet", &config)
        .await
        .expect("pass 3");
    println!(
        "Pass 3 (warm+n) | {:>8.2} ms | txs={:4} | net_sol={:+8.4} | L0 hits={:4} misses={:4}",
        started.elapsed().as_secs_f64() * 1000.0,
        report.summary.tx_count,
        report.summary.net_sol(),
        report.cache_hits_l0,
        report.cache_misses,
    );

    let stats = cache.combined_stats();
    let total = stats.l0_hits + stats.l1_hits + stats.misses;
    let hit_ratio = if total == 0 {
        0.0
    } else {
        (stats.l0_hits + stats.l1_hits) as f64 / total as f64 * 100.0
    };
    println!(
        "\nCache footprint | L0 hits={} L1 hits={} misses={} | overall hit ratio={:.1}%",
        stats.l0_hits, stats.l1_hits, stats.misses, hit_ratio
    );
    println!(
        "Memory bytes    | {:.1} KiB across tx/price/density caches",
        stats.memory_bytes as f64 / 1024.0
    );
}
