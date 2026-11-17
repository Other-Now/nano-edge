#!/usr/bin/env bash
# Run the HTTP cache conformance suite (github.com/http-tests/cache-tests, the suite behind
# cache-tests.fyi) against nano-edge and against nginx with the suite's own nginx config.
# Writes results/cache-tests-{nano-edge,nginx}.json and a summary.
set -uo pipefail
export PATH=$HOME/.cargo/bin:$HOME/tools/node-v22.11.0-linux-x64/bin:$PATH
ROOT=$(cd "$(dirname "$0")/.." && pwd)
CT=${CT:-$HOME/ne-deps/cache-tests}
NGX=${NGX:-$HOME/ne-deps/root/usr/sbin/nginx}
RUN=~/ne-run/ct; rm -rf $RUN; mkdir -p $RUN/cache $RUN/logs
BIN=${CARGO_TARGET_DIR:-$HOME/ne-target}/release/nano-edge

cleanup() { kill $SRV $EDGE 2>/dev/null; $NGX -p $RUN -c $RUN/nginx.conf -s stop 2>/dev/null; }
trap cleanup EXIT

(cd $CT && npm_config_protocol=http npm_config_port=8000 npm_config_pidfile=$RUN/server.pid   exec node test-engine/server/server.mjs) > $RUN/server.log 2>&1 & SRV=$!

cat > $RUN/edge.toml <<TOML
listen = "127.0.0.1:8001"
waf = false
[upstream]
servers = ["127.0.0.1:8000"]
policy = "round_robin"
TOML
$BIN $RUN/edge.toml > $RUN/edge.log 2>&1 & EDGE=$!

# The suite's docker/nginx/nginx.conf, unchanged apart from paths and the listener.
cat > $RUN/nginx.conf <<NGINX
daemon on;
pid $RUN/nginx.pid;
error_log $RUN/logs/error.log;
events {}
http {
  access_log off;
  client_body_temp_path $RUN/cache/body;
  fastcgi_temp_path $RUN/cache/fcgi; uwsgi_temp_path $RUN/cache/uwsgi; scgi_temp_path $RUN/cache/scgi;
  proxy_cache_path $RUN/cache/c levels=1:2 keys_zone=my-cache:8m max_size=1000m inactive=600m;
  proxy_temp_path $RUN/cache/tmp;
  server {
    listen 127.0.0.1:8002;
    location / {
      proxy_pass http://127.0.0.1:8000;
      proxy_cache my-cache;
      proxy_cache_revalidate on;
      proxy_http_version 1.1;
    }
  }
}
NGINX
$NGX -p $RUN -c $RUN/nginx.conf
for i in $(seq 50); do curl -sf -o /dev/null http://127.0.0.1:8000/ -X GET -w '' 2>/dev/null; [ $? -ne 7 ] && break; sleep 0.2; done
sleep 1

cd $CT
cli() { npm_config_base=$1 npm_config_id= npm_package_config_id= node --no-warnings test-engine/cli.mjs; }
cli http://127.0.0.1:8001 > $ROOT/results/cache-tests-nano-edge.json 2> $RUN/cli-edge.err
cli http://127.0.0.1:8002 > $ROOT/results/cache-tests-nginx.json 2> $RUN/cli-nginx.err
for f in nano-edge nginx; do
  if [ ! -s $ROOT/results/cache-tests-$f.json ]; then
    echo "== empty result for $f; logs:"; tail -n 20 $RUN/*.log $RUN/*.err $RUN/logs/* 2>/dev/null; exit 1
  fi
done
node --input-type=module -e "$(cat $ROOT/bench/ct-summary.mjs)" $CT $ROOT/results/cache-tests-nano-edge.json $ROOT/results/cache-tests-nginx.json | tee $ROOT/results/cache-tests-summary.txt
