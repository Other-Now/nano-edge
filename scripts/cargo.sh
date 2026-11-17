#!/usr/bin/env bash
# Run cargo inside WSL with the target dir on the Linux filesystem (fast builds).
# usage: wsl -d Ubuntu-24.04 -- bash /mnt/d/self_projects/nano-edge/scripts/cargo.sh test
export PATH=$HOME/.cargo/bin:$PATH
export CARGO_TARGET_DIR=$HOME/ne-target
cd "$(dirname "$0")/.."
exec cargo "$@"
