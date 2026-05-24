# cascade-telemetry

`Span`, `MetricsRegistry`, and structured logging primitives.

## Role

Layer **01-foundation**. Provides the *hook surface* every hot-path function uses for observability. In v1 these are largely no-ops; in Phase 7+ we wire OTLP / Prometheus / log exporters behind them without changing call sites.

## Why this is a foundation crate

The §6.2 reservation requires that every hot-path function take a `&Span` (or `Span::noop()`). The `MetricsRegistry` similarly must exist from day one so subsystems can register counters / histograms even if nothing exports them yet. If we deferred this crate, retrofitting span parameters into thousands of call sites later would be painful.

## Public API (v1)

- `Span` — opaque handle; `Span::noop()` for the default. Future: real OTel context.
- `MetricsRegistry` — counters, histograms; no-op storage in v1.
- Structured log macros with reserved fields: `trace_id`, `span_id`, `database_id`, `query_id`.

## Dependencies

None internal. Deliberately no OTel deps in v1 — those land in Phase 7.

## See also

- [AI/CONTEXT.md](AI/CONTEXT.md)
- README §6.2.
