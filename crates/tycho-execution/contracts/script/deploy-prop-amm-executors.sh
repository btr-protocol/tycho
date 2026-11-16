#!/usr/bin/env bash
# Monad 143: Crystal + Capricorn executors and a 16-slot LeanTychoRouter through
# BTRCreate3Factory.deploy(bytes32,bytes). PREPARES ONLY: prints predicted addresses
# and the exact calldata for each `deploy` call; never signs, never sends.
# The owner sends them from the deployer EOA (CREATE3 address = f(deployer, salt)).
# Needs: forge build --use solar (out/), cast, RPC (read-only).
set -euo pipefail
RPC=${RPC:?read-only Monad RPC}
FACTORY=0x815d9297bB4dFf9eCaEB7C53017E0DE40300C75e
DEPLOYER=0x0a37aEc263CbA0aaBC09Bac56A0F2074a22E69A3
OUT=${OUT:-out}
# live router executors, in order (8), then the pamm ones already on chain (thog, woofi, poe)
LIVE=(0xbbbbbbb01f183CaF30f9269dA665b3c47449aC83 0xbbbbbbb0497a47C7FAA2A72AB33716FEe4B8261D
  0xbbbbbbb0352D36e84725A91732F65Ee03D5e423E 0xbbbbbbb03625B037f6317301A68Bc73beD14160B
  0xbbbbbbb03b6F50A52BCCF194E6e2f03A18462849 0xbbbbbbb03e8Ef4278A52DC145a4408D1015bb854
  0xbbbbbbb0E998aF620222a1811d4D966971bb8aEF 0xbbbbbbb0eA162Fb0dc7BA151F34214B7bc1C3626
  0xBEc1C4B1dc4071F04DeFC72d6a8d1768e5Fa413b 0x275B904b3ddad0EDa9cFB867F27e390C068Fe6Ea
  0x13a0C0feeeB5cf47529695aEdDC4A4551B7d8eb4)

die(){ echo "ABORT: $*" >&2; exit 1; }
init(){ jq -r .bytecode.object "$OUT/$1.sol/$1.json"; }
pred(){ cast call -r "$RPC" "$FACTORY" "predict(address,bytes32)(address)" "$DEPLOYER" "$1"; }
for a in "${LIVE[@]}"; do [ "$(cast codesize -r "$RPC" "$a")" -gt 0 ] || die "no code at $a"; done

S_CRY=$(cast keccak "btr.pamm.crystal.v1"); A_CRY=$(pred "$S_CRY")
S_CAP=$(cast keccak "btr.pamm.capricorn.v1"); A_CAP=$(pred "$S_CAP")
S_RTR=$(cast keccak "btr.pamm.router.v2"); A_RTR=$(pred "$S_RTR")

EX=$(IFS=,; echo "${LIVE[*]},$A_CRY,$A_CAP")
RTR_INIT=$(init LeanTychoRouter)$(cast abi-encode "f(address[])" "[$EX]" | cut -c3-)

emit(){ # name salt predicted initcode
  local cd; cd=$(cast calldata "deploy(bytes32,bytes)" "$2" "$4")
  [ "$(cast codesize -r "$RPC" "$3")" = 0 ] || echo "# $1 already has code at $3: skip"
  echo "## $1 -> $3"
  echo "to=$FACTORY from=$DEPLOYER value=0"
  echo "calldata=$cd"
  echo "calldata_keccak=$(cast keccak "$cd")"
}
emit CrystalExecutor "$S_CRY" "$A_CRY" "$(init CrystalExecutor)"
emit CapricornExecutor "$S_CAP" "$A_CAP" "$(init CapricornExecutor)"
emit "LeanTychoRouter(13 executors)" "$S_RTR" "$A_RTR" "$RTR_INIT"
echo "# after the sends: executors()(address[16]) on $A_RTR == [${EX}, 0, 0, 0]"
echo "# then the CoopArb mint with tychoRouter=$A_RTR and the Safe lane/swap-gate batches"
