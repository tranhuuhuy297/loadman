#!/usr/bin/env bash
# Reproduce docs/BENCHMARKS.md. Requires: cargo, node >= 22, npm (for newman).
# Everything runs on one machine against the bundled target server.
set -euo pipefail
cd "$(dirname "$0")/.."
SECS=${SECS:-15}
cargo build --release --bin loadman --example target_server >/dev/null
./target/release/examples/target_server 127.0.0.1:8080 4 & SERVER=$!
trap 'kill $SERVER' EXIT
sleep 0.5

measure() { # label, command...
  local label=$1; shift
  /usr/bin/time -l "$@" > "bench/out-$label.txt" 2> "bench/time-$label.txt" || true
  local rss real user sys
  rss=$(awk '/maximum resident/ {print int($1/1048576)}' "bench/time-$label.txt")
  read -r real user sys < <(awk '/ real / {print $1, $3, $5}' "bench/time-$label.txt")
  echo "$label: wall ${real}s, cpu user ${user}s sys ${sys}s, peak RSS ${rss} MB"
}

echo "== A. 100 VUs, one GET, no scripts, ${SECS}s"
measure loadman ./target/release/loadman run bench/hello.json -u 100 -t "${SECS}s" --no-scripts --quiet -o bench/a-loadman.json
grep -E "requests|latency" bench/out-loadman.txt
measure node node bench/node-baseline.mjs http://127.0.0.1:8080/hello 100 "$SECS"
cat bench/out-node.txt

echo "== B. 1 VU, demo collection with pm.test scripts"
[ -d bench/node_modules/newman ] || npm install --prefix bench --no-audit --no-fund newman@6 >/dev/null
measure newman node bench/node_modules/newman/bin/newman.js run examples/demo/collection.json \
  -e examples/demo/environment.json -n 1000 --reporters cli --reporter-cli-no-summary --silent
echo "  newman: 3000 requests (1000 iterations x 3)"
measure loadman-1vu ./target/release/loadman run examples/demo/collection.json -e examples/demo/environment.json \
  -u 1 -t 10s --workers 1 --script-threads 1 --quiet
grep -E "requests|tests" bench/out-loadman-1vu.txt
