#!/usr/bin/env bash
# Runs the horizontal-scaling benchmark and opens the monitoring dashboard.
#
#   ./run.sh                 default demo: sweep 1,2,4 validators, pre-signed load
#   ./run.sh [flags...]      any flag below overrides its default; unknown flags
#                            are passed straight to the er-bench binary
#
# Flags (defaults in brackets):
#   --sweep <list>            validator counts to sweep, e.g. 1,2,4   [1,2,4]
#   --users-per-shard <n>     delegated USDC holders per validator, even [512]
#   --presign-txs <n>         pre-sign N transfers/shard before the timed
#                             window (0 = sign just-in-time)           [1000000]
#   --blocktime-ms <n>        ER block time; keep 400 with presign so the
#                             blockhash outlives sign + send           [400]
#   --duration-secs <n>       load duration when presign is 0          [30]
#   --connections <n>         parallel HTTP connections per validator  [10]
#   --batch-size <n>          transactions per JSON-RPC batch          [400]
#   --signer-threads <n>      signer threads per validator shard       [3]
#   --dashboard-port <n>      monitoring UI port                       [3777]
#   --no-hold                 exit when the sweep finishes instead of
#                             keeping the dashboard up
set -euo pipefail
cd "$(dirname "$0")"

if [ "${1:-}" = "--help" ] || [ "${1:-}" = "-h" ]; then
  awk 'NR>1 && /^#/ { sub(/^# ?/, ""); print; next } NR>1 { exit }' "$0"
  exit 0
fi

BIN=bench/target/release/er-bench
[ -x "$BIN" ] || { echo "er-bench not built — run ./setup.sh first" >&2; exit 1; }
[ -e magicblock-validator ] || { echo "magicblock-validator checkout missing — run ./setup.sh first" >&2; exit 1; }

# Collect user flags; --no-hold is ours, everything else goes to er-bench.
HOLD=1
USER_ARGS=()
for a in "$@"; do
  if [ "$a" = "--no-hold" ]; then HOLD=0; else USER_ARGS+=("$a"); fi
done

has_flag() {
  local flag=$1 a
  for a in ${USER_ARGS[@]+"${USER_ARGS[@]}"}; do
    [ "$a" = "$flag" ] && return 0
  done
  return 1
}

# Defaults apply only when the user did not supply the flag
# (clap rejects duplicated arguments).
ARGS=()
has_flag --sweep           || ARGS+=(--sweep 1,2,4)
has_flag --users-per-shard || ARGS+=(--users-per-shard 512)
has_flag --presign-txs     || ARGS+=(--presign-txs 1000000)
has_flag --blocktime-ms    || ARGS+=(--blocktime-ms 400)
has_flag --connections     || ARGS+=(--connections 10)
[ "$HOLD" = 1 ] && ARGS+=(--hold)
ARGS+=(${USER_ARGS[@]+"${USER_ARGS[@]}"})

PORT=3777
prev=""
for a in ${USER_ARGS[@]+"${USER_ARGS[@]}"}; do
  [ "$prev" = "--dashboard-port" ] && PORT="$a"
  prev="$a"
done

# Open the monitoring dashboard once the bench is up.
( sleep 2
  if command -v open >/dev/null; then open "http://127.0.0.1:$PORT"
  elif command -v xdg-open >/dev/null; then xdg-open "http://127.0.0.1:$PORT"
  else echo "dashboard: http://127.0.0.1:$PORT"
  fi ) &

exec "$BIN" "${ARGS[@]}"
