#!/bin/bash
# Restart overnight eval until it exits 0 or 12 hours pass.
set -euo pipefail
cd /Users/william/q-harness/eval/nightly
mkdir -p runs
end=$((SECONDS + 12*3600))
while (( SECONDS < end )); do
  echo "$(date +%H:%M:%S) start run.py" >> runs/wrap.log
  python3 -u run.py "$@"
  rc=$?
  echo "$(date +%H:%M:%S) run.py exit $rc" >> runs/wrap.log
  if [[ $rc -eq 0 ]]; then
    exit 0
  fi
  sleep 15
done
echo "time budget exhausted" >> runs/wrap.log
exit 1
