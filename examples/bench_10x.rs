//! 10-pass benchmark: cold + 9 warm runs against a single wallet.
//!
//! Reports per-pass timing, cold/warm/min/max/median/mean stats,
//! cache hit ratios, and RPC call counts.
//!
//! ```
//! cargo run --release --example bench_10x -- <wallet>
//! ```
//!
//! Defaults to `vines1vzrYbzLMRdu58ou5XTby4qAqVRLmqo36NKPTg` (~150 txs).
//!
//! Supports filters:
//!   --after-slot N     Only txs after slot N
//!   --before-slot N    Only txs before slot N
//!   --start-time N     Only txs with blockTime >= N
//!   --end-time N       Only txs with blockTime <= N

use std::env;
use std::sync::Arc;
use std::time::Instant;

use solana_pnl_cachee::cache_layer::CacheLayer;
use solana_pnl_cachee::live::{solve_wallet_pnl_filtered, HeliusClient, PnlFilters};
use solana_pnl_cachee::price::PriceHistory;

const PASSES: usize = 10;

fn parse_flag(args: &[String], flag: &str) -> Option<u64> {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1))
        .and_then(|v| v.parse().ok())
}

fn extract_api_key(rpc_url: &str) -> Option<String> {
    rpc_url
        .split_once("api-key=")
        .map(|(_, k)| k.split('&').next().unwrap_or(k).to_string())
}

fn median(sorted: &[f64]) -> f64 {
    let n = sorted.len();
    if n == 0 { return 0.0; }
    if n % 2 == 0 {
        (sorted[n / 2 - 1] + sorted[n / 2]) / 2.0
    } else {
        sorted[n / 2]
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let _ = dotenvy::dotenv();
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
        .filter(|a| !a.starts_with("--"))
        .cloned()
        .unwrap_or_else(|| "vines1vzrYbzLMRdu58ou5XTby4qAqVRLmqo36NKPTg".to_string());

    let filters = PnlFilters {
        after_slot: parse_flag(&args, "--after-slot"),
        before_slot: parse_flag(&args, "--before-slot"),
        start_time: parse_flag(&args, "--start-time"),
        end_time: parse_flag(&args, "--end-time"),
    };

    let api_key = env::var("HELIUS_API_KEY")
        .ok()
        .or_else(|| env::var("HELIUS_BETA_RPC").ok().and_then(|u| extract_api_key(&u)))
        .or_else(|| env::var("HELIUS_GATEKEEPER_RPC").ok().and_then(|u| extract_api_key(&u)))
        .or_else(|| env::var("HELIUS_PARSE_TX").ok().and_then(|u| extract_api_key(&u)))
        .ok_or("HELIUS_API_KEY not set and no HELIUS_*_RPC env var contains ?api-key=")?;

    let client = HeliusClient::new(api_key)?;
    client.warmup().await.ok();

    let cache = Arc::new(CacheLayer::new());
    let prices = PriceHistory::constant(150.0);

    println!("=== 10x Benchmark (single-stage gTFA full) ===");
    println!("Wallet  : {wallet}");
    if !filters.is_empty() {
        println!("Filters : {:?}", filters);
    }
    println!("Passes  : {PASSES}");
    println!();

    let mut times_ms: Vec<f64> = Vec::with_capacity(PASSES);

    for pass in 1..=PASSES {
        let label = if pass == 1 { "cold" } else { &format!("warm-{}", pass - 1) };

        let started = Instant::now();
        let report = solve_wallet_pnl_filtered(
            &client,
            cache.clone(),
            &prices,
            &wallet,
            &filters,
        )
        .await?;
        let elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;
        times_ms.push(elapsed_ms);

        println!(
            "Pass {:>2} ({:<7}) | {:>9.3} ms | txs={:5} | rpc~{:3} | net_sol={:+.6} | warm={}",
            pass,
            label,
            elapsed_ms,
            report.summary.tx_count,
            report.rpc_calls,
            report.summary.net_sol(),
            report.warm_cache,
        );
    }

    // Stats
    let cold_ms = times_ms[0];
    let mut warm_times: Vec<f64> = times_ms[1..].to_vec();
    warm_times.sort_by(|a, b| a.partial_cmp(b).unwrap());

    let warm_min = warm_times.first().copied().unwrap_or(0.0);
    let warm_max = warm_times.last().copied().unwrap_or(0.0);
    let warm_median = median(&warm_times);
    let warm_mean = warm_times.iter().sum::<f64>() / warm_times.len().max(1) as f64;
    let speedup = if warm_median > 0.0 { cold_ms / warm_median } else { 0.0 };

    let stats = cache.combined_stats();
    let total = stats.l0_hits + stats.l1_hits + stats.misses;
    let hit_pct = if total > 0 {
        (stats.l0_hits + stats.l1_hits) as f64 / total as f64 * 100.0
    } else {
        0.0
    };

    println!();
    println!("=== Summary ===");
    println!("Cold         : {:>9.3} ms", cold_ms);
    println!("Warm min     : {:>9.3} ms", warm_min);
    println!("Warm max     : {:>9.3} ms", warm_max);
    println!("Warm median  : {:>9.3} ms", warm_median);
    println!("Warm mean    : {:>9.3} ms", warm_mean);
    println!("Speedup      : {:>9.1}x  (cold / warm median)", speedup);
    println!();
    println!(
        "Cache        : L0={} L1={} miss={} ratio={:.1}%",
        stats.l0_hits, stats.l1_hits, stats.misses, hit_pct
    );
    println!(
        "Memory       : {:.1} KiB",
        stats.memory_bytes as f64 / 1024.0
    );

    Ok(())
}
