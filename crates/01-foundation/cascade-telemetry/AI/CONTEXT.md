# cascade-telemetry — AI Design Context

## Role

Foundation crate. Provides `Span`, `MetricsRegistry`, structured log primitives — the §6.2 hooks every other crate uses.

## Pillar Reservations

- **§6.2 (Observability).** This crate's API is *the* reservation. Future OTLP / Prometheus exporters plug in here without touching call sites.

## Hard Invariants

1. **`Span::noop()` must be zero-cost.** No allocations, no atomics on the hot path when telemetry is disabled. The whole point of the reservation is that v1 pays nothing.
2. **API stability before functionality.** Adding a method to `Span` later affects every caller. Get the shape right early; the body can stay a stub.
3. **No I/O.** Exporters are *plugins* registered with the registry, not built into this crate.
4. **No external observability deps in v1.** Keep the dep graph tiny; OTel crates are heavy.

## Out of Scope

- The actual OTLP exporter (Phase 7).
- The Prometheus scrape endpoint (Phase 7).
- Persistent slow-query plan capture (Phase 7, lives in `cascade-sql-exec`).

## Open Design Questions

- Should `Span` be a trait (object-safe, dynamic dispatch) or a generic? Leaning **trait** so `Span::noop()` and a future `OtelSpan` can coexist without monomorphizing every caller. Cost: dynamic dispatch on the hot path. Mitigation: most v1 callers will only ever see `NoopSpan`, which the inliner can resolve.
- Metric naming convention — adopt OTel semantic conventions where they exist, prefix with `cascade.` otherwise.
