#!/usr/bin/env bash
# Rehearses the Monad deployment on an anvil fork: FeeCalculator, executors and TychoRouterV3
# through the normal deploy scripts, roles as roles.json grants them, executors registered by the
# EXECUTOR_SETTER_ROLE holder (impersonated), the 1-day activation timelock warped, then one swap
# per executor via examples/monad_router_rehearsal.rs.
#
# Signs with anvil's test key 0, read from anvil's own banner at run time. Nothing is broadcast
# outside the fork.
#
#   cd crates/tycho-execution/contracts && npm ci && scripts/rehearse-monad-fork.sh
#
# Env: MONAD_RPC (fork source, default public RPC), FORK_BLOCK (default latest), FORK_PORT (8546),
# REHEARSAL_DIR (logs + executors.json, default a temp dir), CREATE3_FACTORY / CREATE3_SALTS
# (optional, rehearse the CREATE3 path; see scripts/utils.js).
set -euo pipefail

cd "$(dirname "$0")/.."
WORK=${REHEARSAL_DIR:-$(mktemp -d)}
mkdir -p "$WORK"
PORT=${FORK_PORT:-8546}
FORK=http://127.0.0.1:$PORT

anvil --fork-url "${MONAD_RPC:-https://rpc.monad.xyz}" ${FORK_BLOCK:+--fork-block-number "$FORK_BLOCK"} \
    --chain-id 143 --port "$PORT" >"$WORK/anvil.log" 2>&1 &
ANVIL=$!
trap 'kill $ANVIL 2>/dev/null' EXIT
until cast chain-id --rpc-url "$FORK" >/dev/null 2>&1; do
    kill -0 $ANVIL 2>/dev/null || { cat "$WORK/anvil.log" >&2; exit 1; }
    sleep 1
done
echo "fork block $(cast block-number --rpc-url "$FORK"), logs in $WORK"

PRIVATE_KEY=$(awk '/Private Keys/{f=1; next} f && /^\(0\)/{print $2; exit}' "$WORK/anvil.log")
export PRIVATE_KEY RPC_URL=$FORK SKIP_VERIFY=1

npx hardhat run scripts/deploy-fee-calculator.js --network monad | tee "$WORK/fee-calculator.log"
FEE_CALCULATOR=$(awk '/^FeeCalculator: /{print $2}' "$WORK/fee-calculator.log")
export FEE_CALCULATOR
npx hardhat run scripts/deploy-executors.js --network monad | tee "$WORK/executors.log"
npx hardhat run scripts/deploy-router.js --network monad | tee "$WORK/router.log"
ROUTER=$(awk '/^TychoRouterV3: /{print $2}' "$WORK/router.log")

# "<protocol>: <address>" lines; PancakeSwap V3 swaps through the Uniswap V3 executor.
node -e '
const fs = require("fs");
const m = {};
for (const l of fs.readFileSync(process.argv[1], "utf8").split("\n")) {
    const r = l.match(/^([a-z0-9_:]+): (0x[0-9a-fA-F]{40})$/);
    if (r) m[r[1]] = r[2];
}
m.pancakeswap_v3 = m.uniswap_v3;
fs.writeFileSync(process.argv[2], JSON.stringify(m, null, 2));
' "$WORK/executors.log" "$WORK/executors.json"
cat "$WORK/executors.json"; echo

role() { node -p "require('./scripts/roles.json').monad.$1[0]"; }
SETTER=$(role EXECUTOR_SETTER_ROLE)
for r in EXECUTOR_SETTER_ROLE:EXECUTOR_SETTER_ROLE PAUSER_ROLE:UNPAUSER_ROLE \
    UNPAUSER_ROLE:UNPAUSER_ROLE ROUTER_FEE_SETTER_ROLE:ROUTER_FEE_SETTER; do
    hash=$(cast call "$ROUTER" "${r%%:*}()(bytes32)" --rpc-url "$FORK")
    holder=$(role "${r##*:}")
    [ "$(cast call "$ROUTER" 'hasRole(bytes32,address)(bool)' "$hash" "$holder" --rpc-url "$FORK")" = true ] ||
        { echo "router: ${r%%:*} not held by $holder" >&2; exit 1; }
done
has() { cast call "$1" 'hasRole(bytes32,address)(bool)' "$2" "$3" --rpc-url "$FORK"; }
eq() { [ "$(echo "$1" | tr A-F a-f)" = "$(echo "$2" | tr A-F a-f)" ] || { echo "$3: $1 != $2" >&2; exit 1; }; }
DEPLOYER=$(cast wallet address "$PRIVATE_KEY")
FEE_SETTER_ROLE=$(cast call "$FEE_CALCULATOR" 'ROUTER_FEE_SETTER_ROLE()(bytes32)' --rpc-url "$FORK")
for r in EXECUTOR_SETTER_ROLE PAUSER_ROLE UNPAUSER_ROLE ROUTER_FEE_SETTER_ROLE; do
    eq "$(has "$ROUTER" "$(cast call "$ROUTER" "$r()(bytes32)" --rpc-url "$FORK")" "$DEPLOYER")" false "deployer $r"
done
eq "$(has "$ROUTER" 0x0000000000000000000000000000000000000000000000000000000000000000 "$DEPLOYER")" false "deployer DEFAULT_ADMIN_ROLE"
eq "$(has "$FEE_CALCULATOR" "$FEE_SETTER_ROLE" "$DEPLOYER")" false "deployer FeeCalculator ROUTER_FEE_SETTER_ROLE"
eq "$(has "$FEE_CALCULATOR" "$FEE_SETTER_ROLE" "$(role ROUTER_FEE_SETTER)")" true "FeeCalculator ROUTER_FEE_SETTER_ROLE"
eq "$(cast call "$FEE_CALCULATOR" 'getRouterFeeReceiver()(address)' --rpc-url "$FORK")" "$(role ROUTER_FEE_RECEIVER)" "router fee receiver"
eq "$(cast call "$ROUTER" 'getFeeCalculator()(address)' --rpc-url "$FORK")" "$FEE_CALCULATOR" "router fee calculator"
echo "roles ok: router + FeeCalculator roles on $SETTER, fee receiver $(role ROUTER_FEE_RECEIVER), deployer holds none"

EXECUTORS=$(node -p 'const m = require(process.argv[1]); "[" + [...new Set(Object.values(m))].join(",") + "]"' "$WORK/executors.json")
cast rpc anvil_impersonateAccount "$SETTER" --rpc-url "$FORK" >/dev/null
cast rpc anvil_setBalance "$SETTER" 0x56BC75E2D63100000 --rpc-url "$FORK" >/dev/null
cast send "$ROUTER" 'setExecutors(address[])' "$EXECUTORS" --from "$SETTER" --unlocked --rpc-url "$FORK" |
    grep -E '^(status|gasUsed)'
cast rpc evm_increaseTime 86401 --rpc-url "$FORK" >/dev/null
cast rpc evm_mine --rpc-url "$FORK" >/dev/null

cargo run -q -p tycho-simulation --example monad_router_rehearsal -- \
    --rpc "$FORK" --router "$ROUTER" --executors "$WORK/executors.json"
