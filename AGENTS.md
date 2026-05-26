# Agent Notes

This project is latency-sensitive trading infrastructure. Treat performance overhead as a correctness concern, not a cleanup task.

- Keep the hot path short: WebSocket log decode, filter, calldata build, sign, broadcast.
- Do not add blocking I/O, synchronous DNS, REST metadata fetches, database writes, or heavy logging before transaction submission.
- The current QuickNode endpoint budget is 15 requests/second. Keep HTTP RPC calls explicitly rate-limited and avoid receipt polling on the buy hot path unless intentionally enabled.
- Price calculation and logging are for sell decisions and post-buy analytics. Never put quote calculations before the buy transaction submission path.
- Prefer allocation-free or low-allocation code on the event path.
- Measure p50, p95, and p99 latency for detection-to-broadcast changes.
- Default to explicit configuration and fail-fast startup validation.
- Never commit private keys, RPC secrets, production wallet addresses, or live `.env` files.
