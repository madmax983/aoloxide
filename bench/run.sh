#!/usr/bin/env bash
# Benchmark aoloxide against an equivalent axum server with wrk.
# See bench/README.md for what is measured and how to read the results.
#
# Usage: bench/run.sh            (from anywhere)
# Env:   DURATION=10  WARMUP=2  CONNS="16 64 256"  WRK_THREADS=2
#        SERVER_CPUS=0-1  CLIENT_CPUS=2-3  THREADS=20 (aoloxide conn threads)
#        OUT=bench/results/<host>-<date>.md
set -euo pipefail

BENCH="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(dirname "$BENCH")"
DURATION="${DURATION:-10}"
WARMUP="${WARMUP:-2}"
CONNS="${CONNS:-16 64 256}"
WRK_THREADS="${WRK_THREADS:-2}"
SERVER_CPUS="${SERVER_CPUS:-0-1}"
CLIENT_CPUS="${CLIENT_CPUS:-2-3}"
THREADS="${THREADS:-20}"
AOL_PORT=18080
AXUM_PORT=18081
mkdir -p "$BENCH/results/raw"
OUT="${OUT:-$BENCH/results/$(hostname -s)-$(date +%Y%m%d-%H%M%S).md}"
CSV="${OUT%.md}.csv"

# name|path : the scenarios, identical paths on both servers.
SCENARIOS=(
  "hello|/hello"
  "static-1k|/1k.txt"
  "static-64k|/64k.txt"
  "adp-page|/page.adp?name=bench"
)

need() { command -v "$1" >/dev/null || { echo "missing: $1" >&2; exit 1; }; }
need wrk; need curl; need taskset

echo "== building release binaries"
cargo build --release -q -p aoloxide --manifest-path "$ROOT/Cargo.toml"
cargo build --release -q --manifest-path "$BENCH/axum-server/Cargo.toml"
NSD="$ROOT/target/release/nsd"
AXUM="$BENCH/axum-server/target/release/axum-bench-server"

WORK="$(mktemp -d)"
SERVER_PID=""
cleanup() {
  if [ -n "$SERVER_PID" ]; then kill "$SERVER_PID" 2>/dev/null || true; fi
  rm -rf "$WORK"
}
trap cleanup EXIT

echo "== preparing page root and server home in $WORK"
mkdir -p "$WORK/www" "$WORK/home/servers/bench/modules/tcl"
cp "$BENCH/www/page.adp" "$WORK/www/"
# Deterministic printable payloads (same bytes every run).
gen() { yes "aoloxide-vs-axum-benchmark-payload-0123456789abcdefghijklmnopqrstuvwxyz" | head -c "$1" || true; }
gen 1024 > "$WORK/www/1k.txt"
gen 65536 > "$WORK/www/64k.txt"
cp "$BENCH/aoloxide/bench.tcl" "$WORK/home/servers/bench/modules/tcl/"
"$NSD" --install-home "$WORK/home" >/dev/null

# wrk reporter: one CSV line (rps, p50/p90/p99/max in us, socket errors, non-2xx).
cat > "$WORK/report.lua" <<'EOF'
done = function(s, lat, req)
  local e = s.errors
  io.write(string.format("RESULT,%.1f,%d,%d,%d,%d,%d,%d\n",
    s.requests / (s.duration / 1e6),
    lat:percentile(50), lat:percentile(90), lat:percentile(99), lat.max,
    e.connect + e.read + e.write + e.timeout, e.status))
end
EOF

cpu_ticks() { awk '{print $14 + $15}' "/proc/$1/stat"; }   # utime + stime
hwm_kb() { awk '/VmHWM/ {print $2}' "/proc/$1/status"; }

# start <aoloxide|axum|axum-cached>: sets SERVER_PID and PORT.
start() {
  case "$1" in
    aoloxide)
      PORT=$AOL_PORT
      BENCH_HOME="$WORK/home" BENCH_PAGES="$WORK/www" BENCH_PORT=$PORT BENCH_THREADS=$THREADS \
        taskset -c "$SERVER_CPUS" "$NSD" -f -t "$BENCH/aoloxide/nsconfig.tcl" \
        > "$BENCH/results/raw/$1.log" 2>&1 &
      ;;
    axum|axum-cached)
      PORT=$AXUM_PORT
      local mode=(); [ "$1" = axum-cached ] && mode=(cached)
      taskset -c "$SERVER_CPUS" "$AXUM" "127.0.0.1:$PORT" "$WORK/www" "${mode[@]}" \
        > "$BENCH/results/raw/$1.log" 2>&1 &
      ;;
  esac
  SERVER_PID=$!
  for _ in $(seq 100); do curl -s -o /dev/null "http://127.0.0.1:$PORT/hello" && return 0; sleep 0.1; done
  echo "$1 did not come up on :$PORT (see results/raw/$1.log)" >&2; exit 1
}
stop() { kill "$SERVER_PID"; wait "$SERVER_PID" 2>/dev/null || true; SERVER_PID=""; }

echo "== checking every server returns aoloxide's bodies"
declare -A WANT
start aoloxide
for s in "${SCENARIOS[@]}"; do
  path="${s#*|}"; WANT[$path]="$(curl -sf "http://127.0.0.1:$PORT$path" | sha256sum)"
done
stop
for server in axum axum-cached; do
  start $server
  for s in "${SCENARIOS[@]}"; do
    path="${s#*|}"
    got="$(curl -sf "http://127.0.0.1:$PORT$path" | sha256sum)"
    if [ "$got" != "${WANT[$path]}" ]; then echo "$server: body mismatch on $path" >&2; exit 1; fi
  done
  stop
  echo "   $server ok"
done

HZ="$(getconf CLK_TCK)"
echo "server,scenario,conns,rps,p50_us,p90_us,p99_us,max_us,sock_errors,non2xx,cpu_us_per_req" > "$CSV"
declare -A RSS

# run_server <server> <scenario-name-glob>
run_server() {
  local server=$1 only=$2
  start "$server"
  for s in "${SCENARIOS[@]}"; do
    local name="${s%%|*}" path="${s#*|}"
    # shellcheck disable=SC2053  # glob match is intended
    [[ $name == $only ]] || continue
    for c in $CONNS; do
      local t=$WRK_THREADS; [ "$c" -lt "$t" ] && t=$c
      taskset -c "$CLIENT_CPUS" wrk -t"$t" -c"$c" -d"${WARMUP}s" "http://127.0.0.1:$PORT$path" >/dev/null
      local before after line
      before=$(cpu_ticks "$SERVER_PID")
      line=$(taskset -c "$CLIENT_CPUS" wrk -t"$t" -c"$c" -d"${DURATION}s" --latency \
               -s "$WORK/report.lua" "http://127.0.0.1:$PORT$path" \
             | tee "$BENCH/results/raw/$server-$name-c$c.txt" | grep '^RESULT,')
      after=$(cpu_ticks "$SERVER_PID")
      IFS=, read -r _ rps p50 p90 p99 max errs non2xx <<< "$line"
      local cpu
      cpu=$(awk -v d=$((after - before)) -v hz="$HZ" -v r="$rps" -v t="$DURATION" \
              'BEGIN { printf "%.1f", (d / hz) * 1e6 / (r * t) }')
      echo "$server,$name,$c,$rps,$p50,$p90,$p99,$max,$errs,$non2xx,$cpu" >> "$CSV"
      printf "   %-11s %-11s c=%-4s %10s req/s  p50 %6sus  p99 %7sus  cpu %5sus/req  err %s/%s\n" \
        "$server" "$name" "$c" "$rps" "$p50" "$p99" "$cpu" "$errs" "$non2xx"
    done
  done
  RSS[$server]=$(hwm_kb "$SERVER_PID")
  stop
}

echo "== running (${DURATION}s per point, ${WARMUP}s warmup, conns: $CONNS)"
run_server aoloxide '*'
run_server axum '*'
run_server axum-cached 'static-*'   # dynamic routes are identical to axum's

echo "== writing $OUT"
# shellcheck disable=SC2016  # awk program, not shell
AWK_LIB='
  function commas(x,   s, r) { s = sprintf("%.0f", x); r = ""
    while (length(s) > 3) { r = "," substr(s, length(s) - 2) r; s = substr(s, 1, length(s) - 3) }
    return s r }
  function ms(us) { return us >= 1000 ? sprintf("%.2f ms", us / 1000) : us " µs" }
  function has(srv, k) { return ((srv, k, 4) in v) }
  NR > 1 { k = $2 "," $3; if (!(k in seen)) { order[++n] = k; seen[k] = 1 }
           for (i = 4; i <= 11; i++) v[$1, k, i] = $i }'
{
  echo "# aoloxide vs axum: $(date -u +%Y-%m-%dT%H:%MZ)"
  echo
  echo "* Host: $(nproc) CPUs, $(awk -F': ' '/model name/ {print $2; exit}' /proc/cpuinfo), $(uname -sr)"
  echo "* Servers pinned to CPUs $SERVER_CPUS, wrk ($WRK_THREADS threads) to CPUs $CLIENT_CPUS"
  echo "* ${DURATION}s per point after ${WARMUP}s warmup; HTTP/1.1 keep-alive; aoloxide conn threads: $THREADS"
  echo "* aoloxide $(git -C "$ROOT" rev-parse --short HEAD), $(wrk --version 2>&1 | head -1 | cut -d' ' -f1-2), $(rustc --version)"
  echo "* Peak RSS: aoloxide $((RSS[aoloxide] / 1024)) MiB, axum $((RSS[axum] / 1024)) MiB, axum-cached $((RSS[axum-cached] / 1024)) MiB"
  echo
  echo "## Throughput"
  echo
  echo "| scenario | conns | aoloxide req/s | axum req/s | axum ÷ aoloxide | axum-cached req/s | axum-cached ÷ aoloxide |"
  echo "|---|---:|---:|---:|---:|---:|---:|"
  awk -F, "$AWK_LIB"'
    END { for (j = 1; j <= n; j++) { k = order[j]; split(k, p, ",")
      a = v["aoloxide", k, 4]; x = v["axum", k, 4]
      c1 = "—"; c2 = "—"
      if (has("axum-cached", k)) { y = v["axum-cached", k, 4]; c1 = commas(y); c2 = sprintf("%.2f×", y / a) }
      printf "| %s | %s | %s | %s | %.2f× | %s | %s |\n", p[1], p[2], commas(a), commas(x), x / a, c1, c2 } }' "$CSV"
  echo
  echo "## Latency, server CPU per request, errors"
  echo
  echo "| scenario | conns | server | p50 | p90 | p99 | max | CPU µs/req | errors |"
  echo "|---|---:|---|---:|---:|---:|---:|---:|---:|"
  awk -F, "$AWK_LIB"'
    END { split("aoloxide axum axum-cached", srv, " ")
      for (j = 1; j <= n; j++) { k = order[j]; split(k, p, ",")
        for (s = 1; s <= 3; s++) { if (!has(srv[s], k)) continue
          printf "| %s | %s | %s | %s | %s | %s | %s | %s | %d |\n", p[1], p[2], srv[s],
            ms(v[srv[s], k, 5]), ms(v[srv[s], k, 6]), ms(v[srv[s], k, 7]), ms(v[srv[s], k, 8]),
            v[srv[s], k, 11], v[srv[s], k, 9] + v[srv[s], k, 10] } } }' "$CSV"
} > "$OUT"
cat "$OUT"
