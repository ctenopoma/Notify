#!/bin/sh
# Self-healing entrypoint for dcgm-exporter.
#
# Problem: dcgm-exporter exits FATALLY if a custom counter names a DEV field the
# GPU/driver doesn't support (e.g. DCGM_FI_DEV_FB_RESERVED on some GPUs). PROF
# fields are merely skipped, but a single unsupported DEV field kills the whole
# exporter — forcing users to read logs and trim the CSV by hand.
#
# This wrapper starts the exporter against a working copy of the counters file;
# if it dies within a few seconds with "Could not find DCGM field X", it drops X
# and retries. The result: the exporter always comes up with the subset this GPU
# actually supports, with no manual CSV editing.
set -u

SRC=/etc/dcgm-exporter/custom-counters.csv
WORK=/tmp/dcgm-counters.csv
LOG=/tmp/dcgm-start.log
BIN="$(command -v dcgm-exporter 2>/dev/null || echo /usr/bin/dcgm-exporter)"

cp "$SRC" "$WORK"

i=0
while [ "$i" -lt 50 ]; do
  i=$((i + 1))
  : > "$LOG"
  "$BIN" -f "$WORK" >>"$LOG" 2>&1 &
  PID=$!

  # Give it a moment to either crash on startup or settle into serving.
  sleep 4
  if kill -0 "$PID" 2>/dev/null; then
    echo "[dcgm-start] exporter running (attempt $i); serving custom counters:"
    cat "$WORK"
    wait "$PID"        # block on the healthy process; propagate its exit code
    exit $?
  fi

  # It died during startup — inspect why.
  cat "$LOG"
  BAD="$(grep -oE 'Could not find DCGM field [A-Z0-9_]+' "$LOG" | awk '{print $NF}' | head -n1)"
  if [ -n "$BAD" ]; then
    echo "[dcgm-start] dropping unsupported field: $BAD"
    grep -v "^[[:space:]]*${BAD}[[:space:]]*," "$WORK" > "$WORK.tmp" && mv "$WORK.tmp" "$WORK"
    if [ ! -s "$WORK" ]; then
      echo "[dcgm-start] no supported fields remain — aborting." >&2
      exit 1
    fi
    continue
  fi

  # A different fatal (e.g. WSL2 'local_cpulist' missing, no GPU). Not fixable here.
  echo "[dcgm-start] exporter failed for a non-field reason; see log above." >&2
  exit 1
done

echo "[dcgm-start] too many retries; giving up." >&2
exit 1
