#!/usr/bin/env bash
# nano-edge vs nginx on one box. Both proxies are pinned to CPUs 0-1 (2 workers each);
# the origins and the load generator (oha) share CPUs 2-3.
#
#   bash bench/perf.sh [scenario...]   scenarios: hit miss openloop stampede slow kill memory
#
# Results: results/perf/*.json (raw oha output) and results/perf-summary.txt.
set -uo pipefail
export PATH=$HOME/.cargo/bin:$PATH
ROOT=$(cd "$(dirname "$0")/.." && pwd)
BIN=${CARGO_TARGET_DIR:-$HOME/ne-target}/release
NGX=~/ne-deps/root/usr/sbin/nginx
RUN=~/ne-run/perf; mkdir -p $RUN/logs $RUN/cache
OUT=$ROOT/results/perf; mkdir -p $OUT
SUM=$ROOT/results/perf-summary.txt
DUR=${DUR:-10s}
EDGE_PORT=8080; NGX_PORT=8081
ORIGINS=(9001 9002 9003)
SCEN=${*:-hit miss openloop stampede slow kill memory}

log() { echo "$*" | tee -a $SUM; }

start_origins() {
  for p in "${ORIGINS[@]}"; do taskset -c 2,3 $BIN/origin $p > $RUN/logs/origin-$p.log 2>&1 & done
  sleep 0.5
}
stop_origins() { pkill -f "$BIN/origin" 2>/dev/null; sleep 0.3; }
origin_hits() { local t=0; for p in "${ORIGINS[@]}"; do t=$((t + $(curl -s 127.0.0.1:$p/stats || echo 0))); done; echo $t; }
origin_reset() { for p in "${ORIGINS[@]}"; do curl -s 127.0.0.1:$p/admin/reset >/dev/null; done; }

# start_edge <policy> <collapse:true|false> [header_read_timeout_ms]
start_edge() {
  cat > $RUN/edge.toml <<TOML
listen = "127.0.0.1:$EDGE_PORT"
admin_listen = "127.0.0.1:9100"
workers = 2
waf = ${WAF:-true}
[upstream]
servers = ["127.0.0.1:9001", "127.0.0.1:9002", "127.0.0.1:9003"]
policy = "$1"
[cache]
collapse = $2
hit_for_pass = ${HFP:-true}
[limits]
header_read_timeout_ms = ${3:-5000}
TOML
  RUST_LOG=warn taskset -c 0,1 $BIN/nano-edge $RUN/edge.toml > $RUN/logs/edge.log 2>&1 &
  EDGE_PID=$!
  sleep 0.5
}
stop_edge() { kill $EDGE_PID 2>/dev/null; wait $EDGE_PID 2>/dev/null; }

# start_nginx <lb-directive or ""> <cache_lock on|off> [client_header_timeout]
start_nginx() {
  rm -rf $RUN/cache/c
  cat > $RUN/nginx.conf <<NGINX
daemon on;
worker_processes 2;
pid $RUN/nginx.pid;
error_log $RUN/logs/nginx-error.log warn;
worker_rlimit_nofile 65535;
events { worker_connections 16384; }
http {
  access_log off;
  client_body_temp_path $RUN/cache/body;
  fastcgi_temp_path $RUN/cache/fcgi; uwsgi_temp_path $RUN/cache/uwsgi; scgi_temp_path $RUN/cache/scgi;
  proxy_temp_path $RUN/cache/tmp;
  proxy_cache_path $RUN/cache/c levels=1:2 keys_zone=edge:64m max_size=1g inactive=60m;
  client_header_timeout ${3:-5s};
  upstream origins {
    $1
    server 127.0.0.1:9001 max_fails=3 fail_timeout=10s;
    server 127.0.0.1:9002 max_fails=3 fail_timeout=10s;
    server 127.0.0.1:9003 max_fails=3 fail_timeout=10s;
    keepalive 256;
  }
  server {
    listen 127.0.0.1:$NGX_PORT backlog=4096;
    location / {
      proxy_pass http://origins;
      proxy_http_version 1.1;
      proxy_set_header Connection "";
      proxy_set_header Host \$host;
      proxy_cache edge;
      proxy_cache_revalidate on;
      proxy_cache_lock $2;
      proxy_next_upstream error timeout http_502 http_503 http_504;
      add_header X-Cache \$upstream_cache_status;
    }
  }
}
NGINX
  taskset -c 0,1 $NGX -p $RUN -c $RUN/nginx.conf
  sleep 0.5
}
stop_nginx() { $NGX -p $RUN -c $RUN/nginx.conf -s stop 2>/dev/null; sleep 0.5; }

# oha_run <name> <url> <oha args...>   -> prints "rps p50 p99 p99.9 non2xx errors"
oha_run() {
  local name=$1 url=$2; shift 2
  taskset -c 2,3 oha --no-tui --output-format json -o $OUT/$name.json "$@" "$url" >/dev/null 2>&1
  python3 - "$OUT/$name.json" <<'PY'
import json, sys
d = json.load(open(sys.argv[1]))
s, p = d["summary"], d.get("latencyPercentiles", {})
codes = d.get("statusCodeDistribution", {})
non2xx = sum(v for k, v in codes.items() if not k.startswith("2"))
errs = sum(d.get("errorDistribution", {}).values())
ms = lambda k: (p.get(k) or 0) * 1000
print(f'{s["requestsPerSec"]:9.0f} rps  p50 {ms("p50"):7.3f} ms  p99 {ms("p99"):7.3f} ms  p99.9 {ms("p99.9"):7.3f} ms  non2xx {non2xx}  errors {errs}')
PY
}

: > $SUM
log "# nano-edge vs nginx $($NGX -v 2>&1 | cut -d/ -f2) — $(date -u +%F) — $(grep -m1 'model name' /proc/cpuinfo | cut -d: -f2 | xargs), $(nproc) vCPU (WSL2)"
log "# proxies pinned to CPU 0-1 (2 workers); origins + oha on CPU 2-3; duration $DUR"
stop_origins; stop_nginx
start_origins

for s in $SCEN; do case $s in

hit)  # 1 KiB object served from cache, closed loop, 3 reps
  log ""; log "## cache hit, 1 KiB, 64 connections, closed loop (3 reps)"
  for rep in 1 2 3; do
    start_edge p2c true; curl -s -o /dev/null 127.0.0.1:$EDGE_PORT/obj/hot
    log "nano-edge  rep$rep $(oha_run hit-edge-$rep http://127.0.0.1:$EDGE_PORT/obj/hot -z $DUR -c 64)"
    stop_edge
    start_nginx "" on; curl -s -o /dev/null 127.0.0.1:$NGX_PORT/obj/hot; curl -s -o /dev/null 127.0.0.1:$NGX_PORT/obj/hot
    log "nginx      rep$rep $(oha_run hit-nginx-$rep http://127.0.0.1:$NGX_PORT/obj/hot -z $DUR -c 64)"
    stop_nginx
  done
  WAF=false start_edge p2c true; curl -s -o /dev/null 127.0.0.1:$EDGE_PORT/obj/hot
  log "nano-edge (waf off)  $(oha_run hit-edge-nowaf http://127.0.0.1:$EDGE_PORT/obj/hot -z $DUR -c 64)"
  stop_edge
  ;;

miss)  # no-store: every request goes to an origin, closed loop
  log ""; log "## pass-through (no-store), 1 KiB, 64 connections, closed loop (3 reps)"
  for rep in 1 2 3; do
    start_edge round_robin true
    log "nano-edge             rep$rep $(oha_run miss-edge-$rep http://127.0.0.1:$EDGE_PORT/nostore/1 -z $DUR -c 64)"
    stop_edge
    start_nginx "" off
    log "nginx cache_lock=off  rep$rep $(oha_run miss-nginx-$rep http://127.0.0.1:$NGX_PORT/nostore/1 -z $DUR -c 64)"
    stop_nginx
  done
  # The two pathologies: collapsing an uncacheable key.
  HFP=false start_edge round_robin true
  log "nano-edge hit_for_pass=off  $(oha_run miss-edge-nohfp http://127.0.0.1:$EDGE_PORT/nostore/1 -z $DUR -c 64)"
  stop_edge
  start_nginx "" on
  log "nginx cache_lock=on         $(oha_run miss-nginx-lock http://127.0.0.1:$NGX_PORT/nostore/1 -z $DUR -c 64)"
  stop_nginx
  ;;

openloop)  # fixed arrival rate with coordinated-omission correction
  log ""; log "## open loop, cache hit, latency-corrected"
  for q in 5000 20000; do
    start_edge p2c true; curl -s -o /dev/null 127.0.0.1:$EDGE_PORT/obj/hot
    log "nano-edge  ${q}/s $(oha_run open-edge-$q http://127.0.0.1:$EDGE_PORT/obj/hot -z $DUR -c 64 -q $q --latency-correction)"
    stop_edge
    start_nginx "" on; curl -s -o /dev/null 127.0.0.1:$NGX_PORT/obj/hot; curl -s -o /dev/null 127.0.0.1:$NGX_PORT/obj/hot
    log "nginx      ${q}/s $(oha_run open-nginx-$q http://127.0.0.1:$NGX_PORT/obj/hot -z $DUR -c 64 -q $q --latency-correction)"
    stop_nginx
  done
  ;;

stampede)  # 1000 simultaneous requests for one cold object, origin takes 200 ms
  log ""; log "## stampede: 1000 concurrent requests, cold key, origin delay 200 ms"
  for c in true false; do
    start_edge p2c $c; origin_reset
    r=$(oha_run stampede-edge-$c "http://127.0.0.1:$EDGE_PORT/obj/cold?delay=200" -n 1000 -c 1000)
    log "nano-edge collapse=$c   origin fetches $(origin_hits)   $r"
    stop_edge
  done
  for l in on off; do
    start_nginx "" $l; origin_reset
    r=$(oha_run stampede-nginx-$l "http://127.0.0.1:$NGX_PORT/obj/cold?delay=200" -n 1000 -c 1000)
    log "nginx proxy_cache_lock=$l origin fetches $(origin_hits)   $r"
    stop_nginx
  done
  ;;

slow)  # one of three origins gets +50 ms; pass-through at 2000 req/s
  log ""; log "## slow origin: 9003 delayed 50 ms, pass-through at 2000 req/s, latency-corrected"
  curl -s "127.0.0.1:9003/admin/delay?ms=50" >/dev/null
  for pol in round_robin p2c; do
    start_edge $pol true; origin_reset
    r=$(oha_run slow-edge-$pol http://127.0.0.1:$EDGE_PORT/nostore/1 -z $DUR -c 64 -q 2000 --latency-correction)
    log "nano-edge $pol  share to slow origin $(curl -s 127.0.0.1:9003/stats)/$(origin_hits)   $r"
    stop_edge
  done
  for pol in "" "least_conn;"; do
    start_nginx "$pol" off; origin_reset
    r=$(oha_run slow-nginx-${pol%;} http://127.0.0.1:$NGX_PORT/nostore/1 -z $DUR -c 64 -q 2000 --latency-correction)
    log "nginx ${pol:-round_robin}  share to slow origin $(curl -s 127.0.0.1:9003/stats)/$(origin_hits)   $r"
    stop_nginx
  done
  curl -s "127.0.0.1:9003/admin/delay?ms=0" >/dev/null
  ;;

kill)  # SIGKILL one origin 3 s into a 10 s run at 2000 req/s
  log ""; log "## origin killed (SIGKILL 9002) 3 s into a 10 s pass-through run at 2000 req/s"
  for who in edge nginx; do
    if [ $who = edge ]; then start_edge round_robin true; port=$EDGE_PORT; else start_nginx "" off; port=$NGX_PORT; fi
    ( sleep 3; pkill -9 -f "$BIN/origin 9002" ) &
    r=$(oha_run kill-$who http://127.0.0.1:$port/nostore/1 -z 10s -c 64 -q 2000 --latency-correction)
    ej=""; [ $who = edge ] && ej="ejections $(curl -s 127.0.0.1:9100/metrics | awk '/^edge_upstream_ejections_total/{print $2}')"
    log "$who  $r  $ej"
    if [ $who = edge ]; then stop_edge; else stop_nginx; fi
    taskset -c 2,3 $BIN/origin 9002 > $RUN/logs/origin-9002.log 2>&1 & sleep 0.5
  done
  ;;

memory)  # RSS per idle keep-alive connection
  log ""; log "## memory per idle keep-alive connection (5000 connections, one request each)"
  N=5000
  for who in edge nginx; do
    if [ $who = edge ]; then start_edge p2c true 120000; port=$EDGE_PORT; pids=$EDGE_PID
    else start_nginx "" on 120s; port=$NGX_PORT; pids=$(pgrep -f "nginx: (master|worker)" | xargs); fi
    curl -s -o /dev/null 127.0.0.1:$port/obj/m
    r=$(python3 $ROOT/bench/idle_conns.py $port $N "$pids")
    log "$who  $r"
    if [ $who = edge ]; then stop_edge; else stop_nginx; fi
  done
  ;;
esac; done

stop_origins
echo "done -> $SUM"
