//! Monad fork rehearsal for a self-deployed TychoRouterV3: one swap per executor, encoded with
//! `TychoRouterEncoder`, sent through the router on an anvil fork, checked against a quote.
//!
//! Quotes: Uniswap V3 / PancakeSwap V3 / Uniswap V4 / Kuru are native tycho-simulation states
//! built from the fork's own storage and views. Balancer V3 and Curve are VM-simulated in
//! tycho-simulation (native math over indexer-fed VM state): Curve is checked against the pool's
//! `get_dy`, Balancer V3 is execution-only (its `expectedAmountOut` is the dry-run output).
//! A quoted swap fails when the executed amount is more than 1 bps off the quote.
//!
//! Needs anvil (impersonation, balance and storage cheats); the router and executors are
//! deployed and activated first by
//! `crates/tycho-execution/contracts/scripts/rehearse-monad-fork.sh`.
//!
//!   cargo run -p tycho-simulation --example monad_router_rehearsal -- \
//!     --rpc http://127.0.0.1:8546 --router 0x... --executors executors.json
use std::{collections::HashMap, str::FromStr};

use alloy::{
    primitives::{keccak256, Address, Bytes as ABytes, I256, U256},
    providers::{DynProvider, Provider, ProviderBuilder},
    rpc::types::TransactionRequest,
    sol,
    sol_types::{SolCall, SolValue},
};
use anyhow::{anyhow, bail, Result};
use clap::Parser;
use num_bigint::BigUint;
use tycho_execution::encoding::{
    evm::{
        encoder_builders::TychoRouterEncoderBuilder,
        swap_encoder::swap_encoder_registry::SwapEncoderRegistry, ROUTER_ETH_ADDRESS,
    },
    models::{Solution, Swap},
};
use tycho_simulation::{
    evm::protocol::{
        kuru::book::{state_from_views, IKuru},
        uniswap_v3::{fee_tier::FeeTier, state::UniswapV3State},
        uniswap_v4::state::{UniswapV4Fees, UniswapV4State},
        utils::uniswap::tick_list::TickInfo,
    },
    tycho_common::{
        models::{protocol::ProtocolComponent, token::Token, Chain},
        simulation::protocol_sim::ProtocolSim,
        Bytes,
    },
};

const MULTICALL3: &str = "0xcA11bde05977b3631167028862bE2a173976CA11";
const POOL_MANAGER: &str = "0x188d586ddcf52439676ca21a244753fa19f9ea8e";
const MON: &str = "0x0000000000000000000000000000000000000000";
const WMON: &str = "0x3bd359C1119dA7Da1D913D1C4D2B7c461115433A";
const USDC: &str = "0x754704Bc059F8C67012fEd69BC8A327a5aafb603";
const AUSD: &str = "0x00000000eFE302BEAA2b3e6e1b18d08D69a9012a";
const USDT0: &str = "0xe7cd86e13AC4309349F30B3435a9d337750fC82D";
const WN_USDC: &str = "0x8d5c2Df3Eef09088Fcccf3376D8EcD0Dd505f642";
const WN_USDT0: &str = "0x4e8aaecCE10ad9394e96fE5f2bd4e587A7B04298";
const USDC_BALANCE_SLOT: u64 = 9;
/// Bitmap words read each side of the current tick; the rehearsal sizes stay well inside.
const BITMAP_WORDS: i32 = 8;

sol! {
    struct Call3 { address target; bool allowFailure; bytes callData; }
    struct Result3 { bool success; bytes returnData; }
    function aggregate3(Call3[] calls) external payable returns (Result3[] returnData);

    interface IV3Pool {
        function slot0() external view returns (uint160 sqrtPriceX96, int24 tick);
        function liquidity() external view returns (uint128);
        function fee() external view returns (uint24);
        function tickSpacing() external view returns (int24);
        function tickBitmap(int16 word) external view returns (uint256);
        function ticks(int24 tick) external view returns (uint128 liquidityGross, int128 liquidityNet);
    }
    interface IPoolManager { function extsload(bytes32 slot) external view returns (bytes32); }
    interface ICurve { function get_dy(int128 i, int128 j, uint256 dx) external view returns (uint256); }
    interface IERC20 {
        function balanceOf(address) external view returns (uint256);
        function approve(address, uint256) external returns (bool);
    }
    interface IWMON { function deposit() external payable; }
    interface IERC4626 { function deposit(uint256 assets, address receiver) external returns (uint256); }
}

#[derive(Parser)]
struct Args {
    #[arg(long, default_value = "http://127.0.0.1:8546")]
    rpc: String,
    #[arg(long)]
    router: Address,
    /// JSON `{protocol_system: executor}` for Monad.
    #[arg(long)]
    executors: String,
    /// Impersonated swapper; any code-less address.
    #[arg(long, default_value = "0x00000000000000000000000000000000B7700001")]
    user: Address,
    /// Slippage floor below the quote, in bps.
    #[arg(long, default_value_t = 100)]
    slippage_bps: u64,
}

struct Case {
    name: &'static str,
    system: &'static str,
    pool: &'static str,
    token_in: (&'static str, &'static str, u32),
    token_out: (&'static str, &'static str, u32),
    amount_in: U256,
    attrs: Vec<(&'static str, Vec<u8>)>,
}

fn addr(s: &str) -> Address {
    Address::from_str(s).expect("address constant")
}

fn token((a, sym, dec): (&str, &str, u32)) -> Token {
    Token::new(&Bytes::from_str(a).unwrap(), sym, dec, 0, &[Some(0)], Chain::Monad, 100)
}

fn big(v: U256) -> BigUint {
    BigUint::from_bytes_be(&v.to_be_bytes::<32>())
}

fn u256(v: &BigUint) -> U256 {
    U256::from_be_slice(&v.to_bytes_be())
}

struct Fork {
    p: DynProvider,
}

impl Fork {
    async fn call(&self, to: Address, data: Vec<u8>) -> Result<ABytes> {
        Ok(self
            .p
            .call(
                TransactionRequest::default()
                    .to(to)
                    .input(ABytes::from(data).into()),
            )
            .await?)
    }

    async fn multicall(&self, calls: Vec<(Address, Vec<u8>)>) -> Result<Vec<ABytes>> {
        let calls: Vec<Call3> = calls
            .into_iter()
            .map(|(target, d)| Call3 { target, allowFailure: false, callData: ABytes::from(d) })
            .collect();
        let raw = self
            .call(addr(MULTICALL3), aggregate3Call { calls }.abi_encode())
            .await?;
        Ok(aggregate3Call::abi_decode_returns(&raw)?
            .into_iter()
            .map(|r| r.returnData)
            .collect())
    }

    async fn anvil<P: serde::Serialize + Clone + Send + Sync + Unpin + std::fmt::Debug>(
        &self,
        method: &'static str,
        params: P,
    ) -> Result<()> {
        self.p
            .raw_request::<P, serde_json::Value>(method.into(), params)
            .await?;
        Ok(())
    }

    /// Sends as `from` (impersonated); returns gas used, failing on revert.
    async fn send(&self, from: Address, to: Address, value: U256, data: Vec<u8>) -> Result<u64> {
        let tx = TransactionRequest::default()
            .from(from)
            .to(to)
            .value(value)
            .input(ABytes::from(data).into());
        let receipt = self
            .p
            .send_transaction(tx)
            .await?
            .get_receipt()
            .await?;
        if !receipt.status() {
            bail!("tx {} reverted", receipt.transaction_hash);
        }
        Ok(receipt.gas_used)
    }

    /// `eth_call` of a router swap; returns its `amountOut`.
    async fn dry_run(
        &self,
        from: Address,
        to: Address,
        value: U256,
        data: Vec<u8>,
    ) -> Result<U256> {
        let tx = TransactionRequest::default()
            .from(from)
            .to(to)
            .value(value)
            .input(ABytes::from(data).into());
        self.p
            .call(tx)
            .await
            .map(|r| U256::from_be_slice(&r[..32]))
            .map_err(|e| anyhow!("dry run: {e}"))
    }

    async fn balance(&self, tok: Address, who: Address) -> Result<U256> {
        if tok == Address::ZERO {
            return Ok(self.p.get_balance(who).await?);
        }
        let r = self
            .call(tok, IERC20::balanceOfCall(who).abi_encode())
            .await?;
        Ok(IERC20::balanceOfCall::abi_decode_returns(&r)?)
    }
}

/// Initialized ticks within `BITMAP_WORDS` words of `tick`, read via `bitmap(word)` and
/// `net(tick)` call builders (pool views for V3, PoolManager extsload for V4).
async fn read_ticks(
    fork: &Fork,
    tick: i32,
    spacing: i32,
    bitmap: impl Fn(i16) -> (Address, Vec<u8>),
    net: impl Fn(i32) -> (Address, Vec<u8>),
    decode_net: impl Fn(&[u8]) -> Result<i128>,
) -> Result<Vec<TickInfo>> {
    let word = (tick.div_euclid(spacing)) >> 8;
    let words: Vec<i16> = ((word - BITMAP_WORDS)..=(word + BITMAP_WORDS))
        .map(|w| w as i16)
        .collect();
    let maps = fork
        .multicall(
            words
                .iter()
                .map(|w| bitmap(*w))
                .collect(),
        )
        .await?;
    let mut idx = Vec::new();
    for (w, m) in words.iter().zip(maps) {
        let m = U256::from_be_slice(&m[..32]);
        for bit in 0..256 {
            if m.bit(bit) {
                idx.push(((*w as i32) * 256 + bit as i32) * spacing);
            }
        }
    }
    let nets = fork
        .multicall(idx.iter().map(|t| net(*t)).collect())
        .await?;
    idx.iter()
        .zip(nets)
        .map(|(t, n)| Ok(TickInfo::new(*t, decode_net(&n)?)?))
        .collect()
}

async fn quote_v3(fork: &Fork, c: &Case) -> Result<BigUint> {
    let pool = addr(c.pool);
    let r = fork
        .multicall(vec![
            (pool, IV3Pool::slot0Call {}.abi_encode()),
            (pool, IV3Pool::liquidityCall {}.abi_encode()),
            (pool, IV3Pool::feeCall {}.abi_encode()),
            (pool, IV3Pool::tickSpacingCall {}.abi_encode()),
        ])
        .await?;
    // PancakeSwap's slot0 has a wider tail; the first two words match Uniswap's.
    let sqrt = U256::from_be_slice(&r[0][..32]);
    let tick = I256::from_be_bytes::<32>(r[0][32..64].try_into()?).as_i32();
    let liq = IV3Pool::liquidityCall::abi_decode_returns(&r[1])?;
    let fee = IV3Pool::feeCall::abi_decode_returns(&r[2])?.to::<u32>();
    let spacing = IV3Pool::tickSpacingCall::abi_decode_returns(&r[3])?.as_i32();
    let ticks = read_ticks(
        fork,
        tick,
        spacing,
        |w| (pool, IV3Pool::tickBitmapCall { word: w }.abi_encode()),
        |t| {
            (
                pool,
                IV3Pool::ticksCall { tick: alloy::primitives::aliases::I24::try_from(t).unwrap() }
                    .abi_encode(),
            )
        },
        |d| Ok(i128::try_from(I256::from_be_bytes::<32>(d[32..64].try_into()?))?),
    )
    .await?;
    let state = UniswapV3State::new(liq, sqrt, FeeTier::new(fee, spacing as u16)?, tick, ticks)?;
    Ok(state
        .get_amount_out(big(c.amount_in), &token(c.token_in), &token(c.token_out))?
        .amount)
}

async fn quote_v4(fork: &Fork, c: &Case) -> Result<BigUint> {
    let pm = addr(POOL_MANAGER);
    // StateLibrary: pools mapping at slot 6; Pool.State = slot0, feeGrowth0, feeGrowth1,
    // liquidity, ticks, tickBitmap.
    let base =
        U256::from_be_bytes(keccak256((U256::from_str(c.pool)?, U256::from(6u64)).abi_encode()).0);
    let load = |slot: U256| {
        (pm, IPoolManager::extsloadCall { slot: slot.to_be_bytes::<32>().into() }.abi_encode())
    };
    let r = fork
        .multicall(vec![load(base), load(base + U256::from(3u64))])
        .await?;
    let s0 = U256::from_be_slice(&r[0][..32]);
    let sqrt = s0 & ((U256::from(1u64) << 160) - U256::from(1u64));
    let tick24 = ((s0 >> 160usize) & U256::from(0xffffffu64)).to::<u32>();
    let tick = ((tick24 << 8) as i32) >> 8;
    let proto = ((s0 >> 184usize) & U256::from(0xffffffu64)).to::<u32>();
    let lp_fee = ((s0 >> 208usize) & U256::from(0xffffffu64)).to::<u32>();
    let liq = (U256::from_be_slice(&r[1][..32]) & U256::from(u128::MAX)).to::<u128>();
    // The component's `tick_spacing` attribute: big-endian bytes, positive.
    let spacing = c
        .attrs
        .iter()
        .find(|(k, _)| *k == "tick_spacing")
        .ok_or_else(|| anyhow!("uniswap_v4 case without tick_spacing"))?
        .1
        .iter()
        .fold(0i32, |acc, b| (acc << 8) | *b as i32);
    let ticks_slot = base + U256::from(4u64);
    let bitmap_slot = base + U256::from(5u64);
    let ticks = read_ticks(
        fork,
        tick,
        spacing,
        |w| {
            load(U256::from_be_bytes(
                keccak256((I256::try_from(w as i64).unwrap(), bitmap_slot).abi_encode()).0,
            ))
        },
        |t| {
            load(U256::from_be_bytes(
                keccak256((I256::try_from(t as i64).unwrap(), ticks_slot).abi_encode()).0,
            ))
        },
        // Tick.Info word 0: liquidityGross (low 128) | liquidityNet (high 128).
        |d| Ok(i128::try_from(I256::from_be_bytes::<32>(d[..32].try_into()?).asr(128))?),
    )
    .await?;
    let fees = UniswapV4Fees::new(proto & 0xfff, proto >> 12, lp_fee);
    let state = UniswapV4State::new(liq, sqrt, fees, tick, spacing, ticks)?;
    Ok(state
        .get_amount_out(big(c.amount_in), &token(c.token_in), &token(c.token_out))?
        .amount)
}

async fn quote_kuru(fork: &Fork, c: &Case) -> Result<BigUint> {
    let m = addr(c.pool);
    let r = fork
        .multicall(vec![
            (m, IKuru::getL2BookCall {}.abi_encode()),
            (m, IKuru::getVaultParamsCall {}.abi_encode()),
            (m, IKuru::getMarketParamsCall {}.abi_encode()),
            (m, IKuru::marketStateCall {}.abi_encode()),
        ])
        .await?;
    let state = state_from_views(
        &IKuru::getL2BookCall::abi_decode_returns(&r[0])?,
        &IKuru::getVaultParamsCall::abi_decode_returns(&r[1])?,
        &IKuru::getMarketParamsCall::abi_decode_returns(&r[2])?,
        IKuru::marketStateCall::abi_decode_returns(&r[3])?,
    )
    .map_err(|e| anyhow!(e))?;
    Ok(state
        .get_amount_out(big(c.amount_in), &token(c.token_in), &token(c.token_out))?
        .amount)
}

async fn quote_curve(fork: &Fork, c: &Case) -> Result<BigUint> {
    let coins: Vec<Address> = serde_json::from_slice(
        &c.attrs
            .iter()
            .find(|(k, _)| *k == "coins")
            .unwrap()
            .1,
    )?;
    let i = coins
        .iter()
        .position(|a| *a == addr(c.token_in.0))
        .unwrap() as i128;
    let j = coins
        .iter()
        .position(|a| *a == addr(c.token_out.0))
        .unwrap() as i128;
    let r = fork
        .call(addr(c.pool), ICurve::get_dyCall { i, j, dx: c.amount_in }.abi_encode())
        .await?;
    Ok(big(ICurve::get_dyCall::abi_decode_returns(&r)?))
}

/// `singleSwap` calldata, no client fee, TransferFrom funding (native in = msg.value).
fn router_call(
    encoder: &dyn tycho_execution::encoding::tycho_encoder::TychoEncoder,
    solution: Solution,
) -> Result<(Vec<u8>, U256)> {
    let enc = encoder
        .encode_solutions(vec![solution.clone()])?
        .remove(0);
    let sig = enc.function_signature().to_string();
    if !sig.starts_with("singleSwap(") {
        bail!("unexpected strategy {sig}");
    }
    let native = |b: &Bytes| {
        let a = Address::from_slice(b);
        if a == Address::ZERO {
            Address::from_slice(&ROUTER_ETH_ADDRESS)
        } else {
            a
        }
    };
    let token_in = native(solution.token_in());
    let args = (
        u256(solution.amount_in()),
        token_in,
        native(solution.token_out()),
        u256(solution.expected_amount_out()),
        u256(solution.min_amount_out()),
        Address::from_slice(solution.receiver()),
        (0u32, Address::ZERO, U256::ZERO, U256::MAX, ABytes::new()),
        ABytes::from(enc.swaps().to_vec()),
    )
        .abi_encode_params();
    let mut data = keccak256(sig.as_bytes())[..4].to_vec();
    data.extend(args);
    let value = if Address::from_slice(solution.token_in()) == Address::ZERO {
        u256(solution.amount_in())
    } else {
        U256::ZERO
    };
    Ok((data, value))
}

fn solution(user: &Bytes, c: &Case, expected: &BigUint, min: &BigUint) -> Solution {
    let component = ProtocolComponent {
        id: c.pool.to_string(),
        protocol_system: c.system.to_string(),
        static_attributes: c
            .attrs
            .iter()
            .map(|(k, v)| (k.to_string(), Bytes::from(v.clone())))
            .collect::<HashMap<_, _>>(),
        ..Default::default()
    };
    Solution::new(
        user.clone(),
        user.clone(),
        Bytes::from_str(c.token_in.0).unwrap(),
        Bytes::from_str(c.token_out.0).unwrap(),
        big(c.amount_in),
        expected.clone(),
        min.clone(),
        vec![Swap::new(component, token(c.token_in), token(c.token_out), BigUint::ZERO)],
    )
}

fn cases() -> Vec<Case> {
    let e18 = U256::from(10u64).pow(U256::from(18u64));
    let e6 = U256::from(1_000_000u64);
    let v3_fee = |f: u32| vec![("fee", f.to_be_bytes()[1..].to_vec())];
    let coins = format!("[\"{}\",\"{}\",\"{}\"]", AUSD, USDC, USDT0).to_lowercase();
    vec![
        Case {
            name: "uniswap_v4 MON->USDC",
            system: "uniswap_v4",
            pool: "0x18a9fc874581f3ba12b7898f80a683c66fd5877fd74b26a85ba9a3a79c549954",
            token_in: (MON, "MON", 18),
            token_out: (USDC, "USDC", 6),
            amount_in: U256::from(1000u64) * e18,
            attrs: vec![
                ("key_lp_fee", vec![0x01, 0xf4]),
                ("tick_spacing", vec![0x0a]),
                ("hooks", vec![0u8; 20]),
            ],
        },
        Case {
            name: "uniswap_v3 WMON->USDC",
            system: "uniswap_v3",
            pool: "0x659bd0bc4167ba25c62e05656f78043e7ed4a9da",
            token_in: (WMON, "WMON", 18),
            token_out: (USDC, "USDC", 6),
            amount_in: U256::from(1000u64) * e18,
            attrs: v3_fee(3000),
        },
        Case {
            name: "pancakeswap_v3 WMON->USDC",
            system: "pancakeswap_v3",
            pool: "0x63e48B725540A3Db24ACF6682a29f877808C53F2",
            token_in: (WMON, "WMON", 18),
            token_out: (USDC, "USDC", 6),
            amount_in: U256::from(1000u64) * e18,
            attrs: v3_fee(500),
        },
        Case {
            name: "balancer_v3 wnUSDC->wnUSDT0 (StableSurge)",
            system: "vm:balancer_v3",
            pool: "0x2daa146dfb7eaef0038f9f15b2ec1e4de003f72b",
            token_in: (WN_USDC, "wnUSDC", 6),
            token_out: (WN_USDT0, "wnUSDT0", 6),
            amount_in: U256::from(100u64) * e6,
            attrs: vec![],
        },
        Case {
            name: "curve USDC->AUSD",
            system: "vm:curve",
            pool: "0x942644106b073e30d72c2c5d7529d5c296ea91ab",
            token_in: (USDC, "USDC", 6),
            token_out: (AUSD, "AUSD", 6),
            amount_in: U256::from(100u64) * e6,
            attrs: vec![
                ("factory", b"0x8271e06E5887FE5ba05234f5315c19f3Ec90E8aD".to_vec()),
                ("coins", coins.into_bytes()),
            ],
        },
        Case {
            name: "kuru MON->USDC",
            system: "kuru",
            pool: "0x065c9d28e428a0db40191a54d33d5b7c71a9c394",
            token_in: (MON, "MON", 18),
            token_out: (USDC, "USDC", 6),
            amount_in: U256::from(1000u64) * e18,
            attrs: vec![],
        },
    ]
}

#[tokio::main]
async fn main() -> Result<()> {
    let a = Args::parse();
    let fork = Fork {
        p: ProviderBuilder::new()
            .connect_http(a.rpc.parse()?)
            .erased(),
    };
    let user = a.user;
    if !fork
        .p
        .get_code_at(user)
        .await?
        .is_empty()
    {
        bail!("--user {user} has code");
    }

    // Fund: MON, WMON by deposit, USDC by storage (balances slot 9), wnUSDC by ERC4626 deposit.
    let e18 = U256::from(10u64).pow(U256::from(18u64));
    fork.anvil("anvil_impersonateAccount", (user,))
        .await?;
    fork.anvil("anvil_setBalance", (user, U256::from(1_000_000u64) * e18))
        .await?;
    fork.send(user, addr(WMON), U256::from(10_000u64) * e18, IWMON::depositCall {}.abi_encode())
        .await?;
    let usdc_slot = keccak256((user, U256::from(USDC_BALANCE_SLOT)).abi_encode());
    let usdc_amt = U256::from(1_000_000u64) * U256::from(1_000_000u64);
    fork.anvil(
        "anvil_setStorageAt",
        (addr(USDC), usdc_slot, alloy::primitives::B256::from(usdc_amt)),
    )
    .await?;
    if fork.balance(addr(USDC), user).await? != usdc_amt {
        bail!("USDC balance slot {USDC_BALANCE_SLOT} did not take");
    }
    let approve =
        |spender: Address| IERC20::approveCall { _0: spender, _1: U256::MAX }.abi_encode();
    fork.send(user, addr(USDC), U256::ZERO, approve(addr(WN_USDC)))
        .await?;
    fork.send(
        user,
        addr(WN_USDC),
        U256::ZERO,
        IERC4626::depositCall {
            assets: U256::from(1_000u64) * U256::from(1_000_000u64),
            receiver: user,
        }
        .abi_encode(),
    )
    .await?;
    for t in [WMON, USDC, WN_USDC] {
        fork.send(user, addr(t), U256::ZERO, approve(a.router))
            .await?;
    }

    let executors: HashMap<String, String> =
        serde_json::from_str(&std::fs::read_to_string(&a.executors)?)?;
    let registry = SwapEncoderRegistry::new(Chain::Monad)
        .add_default_encoders(Some(serde_json::json!({ "monad": executors }).to_string()))?;
    let encoder = TychoRouterEncoderBuilder::new()
        .chain(Chain::Monad)
        .swap_encoder_registry(registry)
        .router_address(Bytes::from(a.router.to_vec()))
        .build()?;
    let user_b = Bytes::from(user.to_vec());

    println!("| swap | quote src | quote | executed (uncapped) | received | Δ executed vs quote (bps) | gas |");
    println!("|---|---|---|---|---|---|---|");
    let mut failed = 0;
    for c in cases() {
        let res: Result<()> = async {
            let (src, quote) = match c.system {
                "uniswap_v3" | "pancakeswap_v3" => ("tycho-sim", Some(quote_v3(&fork, &c).await?)),
                "uniswap_v4" => ("tycho-sim", Some(quote_v4(&fork, &c).await?)),
                "kuru" => ("tycho-sim", Some(quote_kuru(&fork, &c).await?)),
                "vm:curve" => ("get_dy", Some(quote_curve(&fork, &c).await?)),
                _ => ("none (execution only)", None),
            };
            let tok_out = addr(c.token_out.0);
            // Dry run to read the executed output: floor 1 (the router rejects a zero min) and an
            // unreachable expected amount, so positive-slippage capture (if on) takes nothing.
            let (one, cap) = (BigUint::from(1u8), BigUint::from(1u8) << 128);
            let (data, value) = router_call(&*encoder, solution(&user_b, &c, &cap, &one))?;
            let dry = fork
                .dry_run(user, a.router, value, data)
                .await?;

            let expected = quote
                .clone()
                .unwrap_or_else(|| big(dry));
            let min = &expected * BigUint::from(10_000 - a.slippage_bps) / BigUint::from(10_000u64);
            let (data, value) = router_call(&*encoder, solution(&user_b, &c, &expected, &min))?;
            fork.dry_run(user, a.router, value, data.clone())
                .await?;
            let before = fork.balance(tok_out, user).await?;
            let gas = fork
                .send(user, a.router, value, data)
                .await?;
            let out = fork.balance(tok_out, user).await? - before;
            // Executed (uncapped dry run) vs quote. With positive-slippage capture on, `received`
            // cannot exceed the quote: the surplus goes to the fee receiver.
            let bps = quote.as_ref().map(|q| {
                let q = I256::from_raw(u256(q));
                (I256::from_raw(dry) - q) * I256::from_raw(U256::from(10_000u64)) / q
            });
            println!(
                "| {} | {} | {} | {} | {} | {} | {} |",
                c.name,
                src,
                quote
                    .as_ref()
                    .map(|q| q.to_string())
                    .unwrap_or("-".into()),
                dry,
                out,
                bps.map(|b| b.to_string())
                    .unwrap_or("-".into()),
                gas
            );
            if let Some(b) = bps {
                if b.abs() > I256::ONE {
                    bail!("executed {dry} is {b} bps off the quote");
                }
            }
            Ok(())
        }
        .await;
        if let Err(e) = res {
            failed += 1;
            println!("| {} | FAILED: {e:#} | | | | | |", c.name);
        }
    }
    if failed > 0 {
        bail!("{failed} swap(s) failed");
    }
    Ok(())
}
