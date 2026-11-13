# Multi-window 30m soak

Command was `SOAK_SECONDS=1800 SOAK_DISPLAY=:166 bash scripts/soak_multi_window_inproc.sh`.
The Python sampler completed **1800s / 341 samples** with two windows alive:
RSS +8116 KiB, threads 23->23, FDs 31->31.

The shell wrapper line was edited while it was running, so the wrapper printed a spurious
`line 67: 1800: command not found`; the raw `samples.json` is the source of truth.
A clean rerun should be done for a final release artifact if this timing bug is considered
blocking. It is not a Tervo runtime failure.
