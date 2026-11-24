# Multi-window 30m soak (clean run)

```bash
SOAK_SECONDS=1800 SOAK_DISPLAY=:167 \
SOAK_OUT=$PWD/.logs/soak/mw-30m-clean \
SOAK_RUNTIME_DIR=/tmp/xtervo-mw-soak-clean \
  bash scripts/soak_multi_window_inproc.sh
```

Result: **PASS 1800s**, 2 windows alive throughout.

```text
RSS growth:  +9212 KiB
threads:     23 -> 23
fds:         31 -> 31
samples:     341
```

This is the clean rerun; the earlier 30m wrapper log with `1800: command not found`
was caused by editing the shell script while it was executing. Runtime remained healthy.
