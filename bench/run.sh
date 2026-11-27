#!/usr/bin/env bash
# Benchmark aoloxide against an equivalent axum server and, optionally, the
# original C AOLserver 4.5, with wrk.  See bench/README.md.
#
# Usage: bench/run.sh            (from anywhere)
# Env:   DURATION=10  WARMUP=2  CONNS="16 64 256"  WRK_THREADS=2
#        SERVER_CPUS=0-1  CLIENT_CPUS=2-3  THREADS=20 (conn threads, both nsds)
#        AOLSERVER_HOME=<prefix from bench/build-aolserver.sh>  (adds C AOLserver)
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
AOLSERVER_HOME="${AOLSERVER_HOME:-}"
PORT_BASE=18080
mkdir -p "$BENCH/results/raw"
OUT="${OUT:-$BENCH/results/$(hostname -s)-$(date +%Y%m%d-%H%M%S).md}"
CSV="${OUT%.md}.csv"

# name|path : the scenarios, identical paths on every server.
SCENARIOS=(
  "hello|/hello"
  "static-1k|/1k.txt"
  "static-64k|/64k.txt"
  "adp-page|/page.adp?name=bench"
)
# AOLserver 4.5 keeps a connection alive only when the client asks for it
# (aoloxide deviates: HTTP/1.1 keep-alive by default).  Ask every server, so
# all of them are measured on keep-alive connections.
KEEP=(-H "Connection: keep-alive")

need() { command -v "$1" >/dev/null || { echo "missing: $1" >&2; exit 1; }; }
need wrk; need curl; need taskset

SERVERS=(aoloxide)
if [ -n "$AOLSERVER_HOME" ]; then
  [ -x "$AOLSERVER_HOME/bin/nsd" ] || { echo "no $AOLSERVER_HOME/bin/nsd (see bench/build-aolserver.sh)" >&2; exit 1; }
  SERVERS+=(aolserver)
fi
SERVERS+=(axum axum-cached)

echo "== building release binaries"
cargo build --release -q -p aoloxide --manifest-path "$ROOT/Cargo.toml"
cargo build --release -q --manifest-path "$BENCH/axum-server/Cargo.toml"
NSD="$ROOT/target/release/nsd"
AXUM="$BENCH/axum-server/target/release/axum-bench-server"

WORK="$(mktemp -d "${TMPDIR:-/tmp}/aolbench.XXXXXX")"
SERVER_PID=""
cleanup() {
  if [ -n "$SERVER_PID" ]; then kill -9 "$SERVER_PID" 2>/dev/null || true; fi
  rm -rf "$WORK"
}
trap cleanup EXIT

echo "== preparing page root and server homes in $WORK"
mkdir -p "$WORK/www" "$WORK/home/servers/bench/modules/tcl"
cp "$BENCH/www/page.adp" "$WORK/www/"
# Deterministic printable payloads (same bytes every run).
gen() { yes "aoloxide-vs-axum-benchmark-payload-0123456789abcdefghijklmnopqrstuvwxyz" | head -c "$1" || true; }
gen 1024 > "$WORK/www/1k.txt"
gen 65536 > "$WORK/www/64k.txt"
cp "$BENCH/aoloxide/bench.tcl" "$WORK/home/servers/bench/modules/tcl/"
"$NSD" --install-home "$WORK/home" >/dev/null

AOL_USER=()
if [ -n "$AOLSERVER_HOME" ]; then
  # C nsd's home: its own bin/ (init.tcl, nssock.so) and Tcl library, plus
  # the bench server's private library and a writable log dir (pid file).
  mkdir -p "$WORK/aolhome/servers/bench/modules/tcl" "$WORK/aolhome/log"
  ln -s "$AOLSERVER_HOME/bin" "$WORK/aolhome/bin"
  ln -s "$AOLSERVER_HOME/modules" "$WORK/aolhome/modules"
  cp "$BENCH/aoloxide/bench.tcl" "$WORK/aolhome/servers/bench/modules/tcl/"
  if [ "$(id -u)" = 0 ]; then
    # nsd will not run as root: run it as nobody, who must reach $WORK.
    AOL_USER=(-u nobody)
    chmod 755 "$WORK"; chmod -R o+rX "$WORK"; chmod 777 "$WORK/aolhome/log"
  fi
fi

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
# Per-thread "tid switches" snapshot; ctx_delta sums each live thread's
# change (a thread that exits mid-run drops out instead of going negative).
ctx_snapshot() {
  local f tid
  for f in /proc/"$1"/task/*/status; do
    tid=${f%/status}; tid=${tid##*/}
    awk -v t="$tid" '/ctxt_switches/ {s += $2} END {print t, s + 0}' "$f" 2>/dev/null || true
  done
}
ctx_delta() { awk 'NR == FNR {a[$1] = $2; next} {d += $2 - (($1 in a) ? a[$1] : 0)} END {print d + 0}' "$1" "$2"; }
hwm_kb() { awk '/VmHWM/ {print $2}' "/proc/$1/status"; }

# start <server>: sets SERVER_PID and PORT.
start() {
  case "$1" in
    aoloxide)
      PORT=$PORT_BASE
      BENCH_HOME="$WORK/home" BENCH_PAGES="$WORK/www" BENCH_PORT=$PORT BENCH_THREADS=$THREADS \
        taskset -c "$SERVER_CPUS" "$NSD" -f -t "$BENCH/aoloxide/nsconfig.tcl" \
        > "$BENCH/results/raw/$1.log" 2>&1 &
      ;;
    aolserver)
      PORT=$((PORT_BASE + 1))
      BENCH_HOME="$WORK/aolhome" BENCH_PAGES="$WORK/www" BENCH_PORT=$PORT BENCH_THREADS=$THREADS \
        LD_LIBRARY_PATH="$AOLSERVER_HOME/lib" \
        taskset -c "$SERVER_CPUS" "$AOLSERVER_HOME/bin/nsd" -f "${AOL_USER[@]}" \
        -t "$BENCH/aoloxide/nsconfig.tcl" > "$BENCH/results/raw/$1.log" 2>&1 &
      ;;
    axum|axum-cached)
      PORT=$((PORT_BASE + 2))
      local mode=(); [ "$1" = axum-cached ] && mode=(cached)
      taskset -c "$SERVER_CPUS" "$AXUM" "127.0.0.1:$PORT" "$WORK/www" "${mode[@]}" \
        > "$BENCH/results/raw/$1.log" 2>&1 &
      ;;
  esac
  SERVER_PID=$!
  for _ in $(seq 100); do curl -s -o /dev/null "http://127.0.0.1:$PORT/hello" && return 0; sleep 0.1; done
  echo "$1 did not come up on :$PORT (see bench/results/raw/$1.log)" >&2; exit 1
}
# C AOLserver 4.5 can crash during shutdown under Tcl 8.6, so don't wait long.
stop() {
  kill "$SERVER_PID" 2>/dev/null || true
  for _ in $(seq 50); do kill -0 "$SERVER_PID" 2>/dev/null || break; sleep 0.1; done
  kill -9 "$SERVER_PID" 2>/dev/null || true
  wait "$SERVER_PID" 2>/dev/null || true
  SERVER_PID=""
}

echo "== checking every server returns aoloxide's bodies"
declare -A WANT
start aoloxide
for s in "${SCENARIOS[@]}"; do
  path="${s#*|}"; WANT[$path]="$(curl -sf "http://127.0.0.1:$PORT$path" | sha256sum)"
done
stop
for server in "${SERVERS[@]:1}"; do
  start "$server"
  for s in "${SCENARIOS[@]}"; do
    path="${s#*|}"
    got="$(curl -sf "http://127.0.0.1:$PORT$path" | sha256sum)"
    if [ "$got" != "${WANT[$path]}" ]; then echo "$server: body mismatch on $path" >&2; exit 1; fi
  done
  stop
  echo "   $server ok"
done

HZ="$(getconf CLK_TCK)"
echo "server,scenario,conns,rps,p50_us,p90_us,p99_us,max_us,sock_errors,non2xx,cpu_us_per_req,ctxsw_per_req" > "$CSV"
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
      taskset -c "$CLIENT_CPUS" wrk -t"$t" -c"$c" -d"${WARMUP}s" "${KEEP[@]}" \
        "http://127.0.0.1:$PORT$path" >/dev/null
      local cpu0 cpu1 line
      cpu0=$(cpu_ticks "$SERVER_PID"); ctx_snapshot "$SERVER_PID" > "$WORK/ctx0"
      line=$(taskset -c "$CLIENT_CPUS" wrk -t"$t" -c"$c" -d"${DURATION}s" --latency "${KEEP[@]}" \
               -s "$WORK/report.lua" "http://127.0.0.1:$PORT$path" \
             | tee "$BENCH/results/raw/$server-$name-c$c.txt" | grep '^RESULT,')
      cpu1=$(cpu_ticks "$SERVER_PID"); ctx_snapshot "$SERVER_PID" > "$WORK/ctx1"
      IFS=, read -r _ rps p50 p90 p99 max errs non2xx <<< "$line"
      local cpu sw
      cpu=$(awk -v d=$((cpu1 - cpu0)) -v hz="$HZ" -v r="$rps" -v t="$DURATION" \
              'BEGIN { printf "%.1f", (d / hz) * 1e6 / (r * t) }')
      sw=$(awk -v d="$(ctx_delta "$WORK/ctx0" "$WORK/ctx1")" -v r="$rps" -v t="$DURATION" \
             'BEGIN { printf "%.2f", d / (r * t) }')
      echo "$server,$name,$c,$rps,$p50,$p90,$p99,$max,$errs,$non2xx,$cpu,$sw" >> "$CSV"
      printf "   %-11s %-11s c=%-4s %10s req/s  p50 %6sus  p99 %7sus  cpu %5sus/req  ctxsw %4s/req  err %s/%s\n" \
        "$server" "$name" "$c" "$rps" "$p50" "$p99" "$cpu" "$sw" "$errs" "$non2xx"
    done
  done
  RSS[$server]=$(hwm_kb "$SERVER_PID")
  stop
}

echo "== running (${DURATION}s per point, ${WARMUP}s warmup, conns: $CONNS)"
for server in "${SERVERS[@]}"; do
  if [ "$server" = axum-cached ]; then
    run_server axum-cached 'static-*'   # dynamic routes are identical to axum's
  else
    run_server "$server" '*'
  fi
done

echo "== writing $OUT"
# shellcheck disable=SC2016  # awk program, not shell
AWK_LIB='
  function commas(x,   s, r) { s = sprintf("%.0f", x); r = ""
    while (length(s) > 3) { r = "," substr(s, length(s) - 2) r; s = substr(s, 1, length(s) - 3) }
    return s r }
  function ms(us) { return us >= 1000 ? sprintf("%.2f ms", us / 1000) : us " µs" }
  function has(srv, k) { return ((srv, k, 4) in v) }
  function rps(srv, k) { return has(srv, k) ? commas(v[srv, k, 4]) : "—" }
  function ratio(a, b, k) { return (has(a, k) && has(b, k)) ? sprintf("%.2f×", v[a, k, 4] / v[b, k, 4]) : "—" }
  NR > 1 { k = $2 "," $3; if (!(k in seen)) { order[++n] = k; seen[k] = 1 }
           for (i = 4; i <= 12; i++) v[$1, k, i] = $i }'
{
  echo "# aoloxide vs AOLserver vs axum: $(date -u +%Y-%m-%dT%H:%MZ)"
  echo
  echo "* Host: $(nproc) CPUs, $(awk -F': ' '/model name/ {print $2; exit}' /proc/cpuinfo), $(uname -sr)"
  echo "* Servers pinned to CPUs $SERVER_CPUS, wrk ($WRK_THREADS threads) to CPUs $CLIENT_CPUS"
  echo "* ${DURATION}s per point after ${WARMUP}s warmup; keep-alive (\`Connection: keep-alive\` sent); conn threads (both nsds): $THREADS"
  echo "* aoloxide $(git -C "$ROOT" rev-parse --short HEAD), $(wrk --version 2>&1 | head -1 | cut -d' ' -f1-2), $(rustc --version)"
  if [ -n "$AOLSERVER_HOME" ]; then
    echo "* AOLserver: C build from bench/build-aolserver.sh at $AOLSERVER_HOME, $(tclsh8.6 <<< 'puts "Tcl [info patchlevel]"' 2>/dev/null || echo 'Tcl 8.6')"
  fi
  printf "* Peak RSS:"; for s in "${SERVERS[@]}"; do printf " %s %s MiB" "$s" "$((RSS[$s] / 1024))"; done; echo
  echo
  echo "## Throughput (req/s)"
  echo
  echo "| scenario | conns | AOLserver (C) | aoloxide | aoloxide ÷ C | axum | axum ÷ aoloxide | axum-cached | axum-cached ÷ aoloxide |"
  echo "|---|---:|---:|---:|---:|---:|---:|---:|---:|"
  awk -F, "$AWK_LIB"'
    END { for (j = 1; j <= n; j++) { k = order[j]; split(k, p, ",")
      printf "| %s | %s | %s | %s | %s | %s | %s | %s | %s |\n", p[1], p[2],
        rps("aolserver", k), rps("aoloxide", k), ratio("aoloxide", "aolserver", k),
        rps("axum", k), ratio("axum", "aoloxide", k),
        rps("axum-cached", k), ratio("axum-cached", "aoloxide", k) } }' "$CSV"
  echo
  echo "## Latency, server CPU and context switches per request, errors"
  echo
  echo "| scenario | conns | server | p50 | p90 | p99 | max | CPU µs/req | ctxsw/req | errors |"
  echo "|---|---:|---|---:|---:|---:|---:|---:|---:|---:|"
  awk -F, -v servers="${SERVERS[*]}" "$AWK_LIB"'
    END { ns = split(servers, srv, " ")
      for (j = 1; j <= n; j++) { k = order[j]; split(k, p, ",")
        for (s = 1; s <= ns; s++) { if (!has(srv[s], k)) continue
          printf "| %s | %s | %s | %s | %s | %s | %s | %s | %s | %d |\n", p[1], p[2], srv[s],
            ms(v[srv[s], k, 5]), ms(v[srv[s], k, 6]), ms(v[srv[s], k, 7]), ms(v[srv[s], k, 8]),
            v[srv[s], k, 11], v[srv[s], k, 12], v[srv[s], k, 9] + v[srv[s], k, 10] } } }' "$CSV"
} > "$OUT"
cat "$OUT"
