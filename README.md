# Solana PnL Solver — Cachee × Helius

**2.56 ms per data point. 200 parallel calls. 100 time windows. 1 RTT.**

## Quick start

```bash
git clone https://github.com/H33ai-postquantum/solana-pnl-cachee
cd solana-pnl-cachee
export HELIUS_API_KEY=<your-key>

# Net PnL — 3 parallel calls, any wallet
cargo run --release --example one_call_pnl -- <wallet>

# 2-call PnL with slot/time filters
cargo run --release --example two_call_pnl -- <wallet>

# Full balance curve — Nx2 parallel (default 20 windows, configurable)
cargo run --release --example balance_curve -- <wallet> 100
```

## Algorithms

### `one_call_pnl` — Net PnL in 3 parallel calls

Fires `getBalance` + `gTFA(asc, full)` + `gTFA(desc, sigs)` simultaneously at t=0. Result arrives at `max(RTT)`. PnL = `balance - preBalance[oldest]`. No assumptions about starting balance. No pagination. No history walk.

### `two_call_pnl` — Filtered PnL in 2 calls

Two parallel `gTFA` calls with `transactionDetails: full` — oldest (asc) and newest (desc). Supports slot-range and blockTime filters via CLI flags: `--after-slot`, `--before-slot`, `--start-time`, `--end-time`.

### `balance_curve` — Nx2 parallel balance curve

Divides the wallet's lifetime into N time windows. Fires 2 `gTFA` calls per window (asc + desc) — all 2N calls in parallel via `tokio::spawn` + `join_all`. Each window yields `preBalance[oldest]` and `postBalance[newest]`, producing a full PnL curve in a single RTT.

## Measured results — real Helius mainnet

All modes verified 10/10 correct.

| Mode | Cold | Calls | Per data point | Credits |
|---|---|---|---|---|
| 10-window curve | 332 ms | 20 parallel | 16.6 ms | 1,000 |
| 40-window curve | 361 ms | 80 parallel | 4.51 ms | 4,000 |
| **100-window curve** | **512 ms** | **200 parallel** | **2.56 ms** | **10,000** |
| **Warm (any mode)** | **0.12 ms** | **0** | **—** | **0** |

## Based on our mathematical analysis, this is how to further reduce latency.

Every Helius Enhanced Parse call returns a `helius-total-latency` response header. We compared it to client wall clock on a warmed connection:

| Component | Time | % of call |
|---|---|---|
| Helius server | 589 ms | 56% |
| Infrastructure (TLS, L7 proxy, WAF, compression) | 461 ms | 44% |
| **Total wall clock** | **1,050 ms** | **100%** |

Cachee eliminates infrastructure overhead on every repeat query. First query runs the full Helius pipeline. Every subsequent query for the same wallet serves from Cachee L0 in **0.12 ms**. Zero API calls. Zero credits.

## Credit savings

`gTFA = 50 credits/call · getBalance = 1 credit · 3-call PnL = 101 credits/query · source: helius.dev/pricing`

Formula: `credits/month = wallets × (2,592,000 / refresh_sec) × 101`

| Scale | No cache | Cachee 95% | Monthly savings |
|---|---|---|---|
| 50 wallets, 10s | 1.31B cr/mo | 65.4M cr/mo | $6,225/month |
| 200 wallets, 10s | 5.23B cr/mo | 262M cr/mo | $24,850/month |
| **500 wallets, 5s** | **26.2B cr/mo** | **1.31B cr/mo** | **$124,500/month** |

Savings at $5 per million credits (published Helius tiers).

## How it handles different wallet types

- **Busy wallets**: More windows, same wall-clock time — parallel calls scale horizontally
- **Sparse wallets**: Most windows return no activity, fast short-circuit
- **Periodic wallets**: Slot-based filters catch activity in any pattern

## Optimizations applied

| Optimization | Impact |
|---|---|
| Nx2 parallel architecture (all windows concurrent) | core pipeline — 2.56 ms/point |
| `sortOrder: asc` + `transactionDetails: full` | oldest tx with preBalance in 1 call |
| HTTP/1.1 with wide connection pool | 6x faster than HTTP/2 on Cloudflare |
| `tcp_nodelay` + connection warmup | eliminates Nagle + cold-start penalty |
| Cachee L0 warm path | 0.12 ms repeat queries, 0 API calls |

### Tested and rejected

| Optimization | Why it failed |
|---|---|
| HTTP/2 multiplexing | Cloudflare stream scheduler serializes — 6x regression |
| JSON-RPC batch | Helius counts per-sub-call against rate limit |
| Parallel gTFA enumeration | Backward cursor can't be parallelized — lost txs |
| Pre-open 32 connections | Cloudflare abuse detection — +975ms + 30s outliers |
| `transactionDetails: full` for full history | 100 txs/page vs 1000 sigs — 5.5s regression on deep wallets |

---

H33.ai, Inc. · github.com/H33ai-postquantum/solana-pnl-cachee

---

**H33 Products:** [H33-74](https://h33.ai) · [Auth1](https://auth1.ai) · [Chat101](https://chat101.ai) · [Cachee](https://cachee.ai) · [Z101](https://z101.ai) · [RevMine](https://revmine.ai) · [BotShield](https://h33.ai/botshield)

*Introducing H33-74. 74 bytes. Any computation. Post-quantum attested. Forever.*
