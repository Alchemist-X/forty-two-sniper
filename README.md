# forty-two-sniper

Low-latency Rust scaffold for watching 42Space market creation events on BNB Chain and submitting an early `swapSimple` mint through the current 42 router.

The implementation uses Rust, Tokio, and Alloy because the hot path is network I/O plus ABI encoding/signing. This keeps runtime overhead lower and more predictable than a Node.js bot while still preserving mature EVM tooling.

## What it does

- Subscribes to `FTMarketController.CreateNewMarket` over WebSocket.
- Filters markets by collateral token.
- Builds router calldata for `FTRouter.swapSimple`.
- Applies a configurable legacy gas-price bump for BNB Chain.
- Provides an `approve` command for the BUSDT router allowance.
- Defaults to `dry_run = true`.

## Quick Start

```bash
cp config.example.toml config.toml
cp .env.example .env
export SNIPER_PRIVATE_KEY=0x...
cargo run --release -- check-config
cargo run --release -- approve --infinite
cargo run --release -- run
```

For real trading, replace the example RPC URLs with a paid low-latency BNB Chain endpoint near Tokyo and set `dry_run = false` only after testing with a dedicated wallet.

## Current 42 Addresses

These defaults are from the official 42 Deployments page checked on 2026-05-26:

- `FTMarketController`: `0xF21b2D4F8989b27f732e369907F25f0E8D95Fe62`
- `FTRouter`: `0x88888888338e60bfB4657187169cFFa5c8640E42`
- `BUSDT`: `0x55d398326f99059fF775485246999027B3197955`

The PDF research note used older router/controller addresses and a `swapMarketV2` example. BscScan verified ABI for the current router exposes `swapSimple`, so this repository uses `swapSimple`.

## Safety

This is an execution scaffold, not audited trading software. Use a fresh wallet, cap balances, leave dry-run on during integration, and expect total loss risk from bad markets, slippage, taxes, failed assumptions, and infrastructure races.
