# 0009: Benchmark before each performance change

**Status:** Accepted, 2026-09-24

## Context
Zero-copy and throughput are goals. Without numbers, you can't tell whether a
change helped.

## Decision
`examples/bench` measures round-trip latency (p50/p99) and one-way throughput
across message sizes from 64 B to 8 MiB. Results are recorded in
[architecture.md](../architecture.md#benchmarks) at each milestone that touches the
transport.

## Consequences
- Always run it in release mode, on the same machine, to compare like with like.
- It measures the middleware, not a realistic workload.
