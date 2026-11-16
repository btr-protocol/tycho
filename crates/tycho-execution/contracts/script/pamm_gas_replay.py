"""Replays one WMON->USDC leg per venue on Monad's live state with eth_estimateGas +
eth_createAccessList and state overrides (Host, executors, router injected; no tx sent),
so every number is Monad's own gas schedule, not foundry's.

usage: python3 -I script/pamm_gas_replay.py <rpc> test/gas-code.json [amount_wei]
Three paths per venue: direct (inline venue call), adapter (one delegatecall, no router),
router (LeanTychoRouter.singleSwap). Each without and with the access list the node builds.
"""
import json, subprocess, sys

RPC, CODE = sys.argv[1], json.load(open(sys.argv[2]))
X = int(sys.argv[3]) if len(sys.argv) > 3 else 100 * 10**18
WMON = "0x3bd359C1119dA7Da1D913D1C4D2B7c461115433A"
USDC = "0x754704Bc059F8C67012fEd69BC8A327a5aafb603"
HOST = "0x00000000000000000000000000000000000000d0"
ROUTER = "0x00000000000000000000000000000000000000d1"
FROM = "0x00000000000000000000000000000000000000d2"
VEN = {
    "thog": "0x80c74517BCC2D67fFE02D3ED886796272F647210",
    "woofi": "0x5520385bFcf07Ec87C4c53A7d8d65595Dff69FA4",
    "poe": "0x02A8A16613a421EabaD6861fF6d8159f6D5EDB8f",
    "crystal": "0x508254C838B2e936b0631440c5C6E3AB3A4A98bd",
    "capricorn": "0x63093325C05cD32B18034d3Ea29199fB7098e4dF",
    "hanji": "0x1aeD222dda944a87703c918745b11bE13f8eEf10",
}
NAMES = list(VEN)
EX = {n: "0x%040x" % (0xE0 + i) for i, n in enumerate(NAMES)}


def sh(*a):
    return subprocess.run(a, capture_output=True, text=True, check=True).stdout.strip()


def h(b):
    return b[2:] if b.startswith("0x") else b


def data(n):
    v = h(VEN[n]).lower()
    w, u = h(WMON).lower(), h(USDC).lower()
    return {
        "thog": v + w + u, "woofi": v + w + u, "hanji": v + w + u,
        "poe": v + w + u + "01", "capricorn": v + w + u + "00", "crystal": w + u,
    }[n]


def calls(n):
    out = {}
    d = {"thog": "thog(address,address,address,uint256)", "woofi": "woo(address,address,address,uint256)",
         "crystal": "crystal(address,address,address,uint256)"}
    if n in d:
        out["direct"] = sh("cast", "calldata", d[n], VEN[n], WMON, USDC, str(X))
    elif n == "poe":
        out["direct"] = sh("cast", "calldata", "poe(address,address,bool,uint256)", VEN[n], WMON, "true", str(X))
    elif n == "capricorn":
        out["direct"] = sh("cast", "calldata", "capricorn(address,address,bool,uint256)", VEN[n], WMON, "false", str(X))
    out["adapter"] = sh("cast", "calldata", "adapter(address,bytes,uint256)", EX[n], "0x" + data(n), str(X))
    out["router"] = sh("cast", "calldata", "viaRouter(address,address,address,uint256,bytes)", ROUTER, WMON, USDC,
                       str(X), "0x" + h(EX[n]) + data(n))
    return out


bal_slot = sh("cast", "index", "address", HOST, "3")  # WMON = WETH9 layout, balanceOf at slot 3
ov = {HOST: {"code": CODE["host"]}, ROUTER: {"code": CODE["router"]},
      WMON: {"stateDiff": {bal_slot: "0x%064x" % X}}}
for n in NAMES:
    ov[EX[n]] = {"code": CODE[n]}


def rpc(m, *p):
    r = sh("cast", "rpc", "-r", RPC, m, "--raw", json.dumps(list(p)))
    return json.loads(r)


print("venue,path,gas_no_list,gas_with_list,saved,list_addrs,list_slots")
for n in NAMES:
    for path, cd in calls(n).items():
        tx = {"from": FROM, "to": HOST, "data": cd}
        try:
            g0 = int(rpc("eth_estimateGas", tx, "latest", ov), 16)
            al = rpc("eth_createAccessList", tx, "latest", ov)["accessList"]
            g1 = int(rpc("eth_estimateGas", dict(tx, accessList=al), "latest", ov), 16)
            print(f"{n},{path},{g0},{g1},{g0 - g1},{len(al)},{sum(len(a['storageKeys']) for a in al)}", flush=True)
            json.dump(al, open(f"test/al-{n}-{path}.json", "w"), indent=1)
        except subprocess.CalledProcessError as e:
            print(f"{n},{path},ERR {e.stderr.strip()[:160]}", flush=True)
