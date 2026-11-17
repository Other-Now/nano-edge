#!/usr/bin/env bash
# Fetch benchmark deps into ~/ne-deps without sudo: nginx (extracted .deb), oha, cache-tests.
set -euo pipefail
export PATH=$HOME/.cargo/bin:$HOME/tools/node-v22.11.0-linux-x64/bin:$PATH
D=~/ne-deps; mkdir -p "$D"; cd "$D"
if [ ! -x root/usr/sbin/nginx ]; then
  apt-get download nginx nginx-common
  for f in *.deb; do dpkg -x "$f" root; done
fi
root/usr/sbin/nginx -V 2>&1 | head -1
if [ ! -x ~/.cargo/bin/oha ]; then cargo install oha --locked; fi
if [ ! -d cache-tests ]; then git clone --depth 1 https://github.com/http-tests/cache-tests.git; fi
if [ ! -d cache-tests/node_modules ]; then (cd cache-tests && npm install --no-audit --no-fund); fi
echo SETUP-OK
