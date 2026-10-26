"""Summarize a pgbench -l latency log: p50/p99/avg in ms, plus txn count.

Usage: python3 -I percentile.py <pgbench_log> [<pgbench_log> ...]
Log lines: client_id txn_no latency_us script_no issue_time
"""
import statistics
import sys

for path in sys.argv[1:]:
    lat = []
    with open(path) as f:
        for line in f:
            parts = line.split()
            if len(parts) >= 3 and parts[2].isdigit():
                lat.append(int(parts[2]))
    if not lat:
        print(f"{path}: empty")
        continue
    lat.sort()
    n = len(lat)
    p = lambda q: lat[min(n - 1, int(n * q))] / 1000.0
    print(f"{path}: n={n} avg={statistics.fmean(lat)/1000:.3f}ms "
          f"p50={p(0.50):.3f}ms p90={p(0.90):.3f}ms p99={p(0.99):.3f}ms "
          f"max={lat[-1]/1000:.3f}ms")
