# Bounded channels with store-first persistence in EventBus

**Date:** 2026-03-26

## Context

The initial EventBus implementation used `tokio::sync::mpsc::UnboundedSender` for subscriber channels. Since LLM calls take seconds while connectors can produce events quickly, unbounded channels risk uncontrolled memory growth under load.

Additionally, in-process channels lose all pending messages on crash. If zymi dies mid-processing, the inbound event is lost.

## Decision

1. **Bounded channels** (capacity 256): `mpsc::channel(capacity)` instead of `unbounded_channel()`. Publisher uses `try_send` — non-blocking, drops the event for that subscriber if buffer is full.

2. **Store-first persistence**: `EventBus::publish()` always writes to `SqliteEventStore` before fanning out to subscribers. The store is the source of truth, not the channel.

3. **Crash recovery via orphan scan**: `AgentWorker::recover_orphaned()` runs on startup, querying the store for `UserMessageReceived` events without a matching `ResponseReady` (via `find_unmatched()` SQL `NOT EXISTS`). Orphaned events are reprocessed sequentially.

## Consequences

**Pros:**
- Predictable memory usage regardless of subscriber speed
- No event loss on crash — store always has the complete history
- Automatic recovery on restart without manual intervention
- Slow subscribers can replay from store at their own pace

**Cons:**
- Subscribers may miss real-time events if their buffer fills (acceptable — they can replay)
- Recovery scan adds startup latency proportional to orphaned event count (negligible in practice)
- Sequential recovery processing may delay startup if many events are orphaned (intentional — avoids overwhelming agent)
