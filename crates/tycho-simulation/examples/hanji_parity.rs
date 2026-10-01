//! Hanji parity: the `vm:hanji` pool state an indexer would hold vs the chain, at random blocks.
//!
//! Per market and block B:
//!   1. trace the entry points the `monad-hanji` substreams package registers, with the indexer's
//!      own DCI tracer, to get the contracts and slots the indexer tracks;
//!   2. seed the account snapshot from chain storage at B: every slot any traced call reads for
//!      full-indexed contracts, only the DCI-traced slots for ERC-20 tokens (as the DCI does);
//!   3. decode it through `TychoStreamDecoder` into an `EVMPoolState` running the Hanji adapter;
//!   4. require `get_amount_out` == the proxy's own quote (`eth_call` at B, from the adapter's
//!      address), to the wei, for random sizes on both sides — and once more with every Pyth price
//!      the market reads made older than 60 s, where the quoter drops its oracle clamp.
//!
//! Public RPCs lack `debug_storageRangeAt`, so full-indexed contracts are seeded with the slots
//! the traced calls read instead of their whole storage; any slot the simulation reads beyond
//! that would read as zero and break parity.
//!
//!   cargo run -p tycho-simulation --example hanji_parity -- --rounds 5 --samples 20
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    str::FromStr,
};

use alloy::{
    eips::BlockId,
    primitives::{Address, Bytes as ABytes, B256, U256},
    providers::{DynProvider, Provider, ProviderBuilder},
    rpc::{
        client::RpcClient,
        types::{
            state::{AccountOverride, StateOverride},
            TransactionRequest,
        },
    },
    sol,
    sol_types::{SolCall, SolValue},
    transports::layers::RetryBackoffLayer,
};
use anyhow::{anyhow, bail, Result};
use clap::Parser;
use num_bigint::BigUint;
use tycho_client::feed::{
    synchronizer::{ComponentWithState, Snapshot, StateSyncMessage},
    BlockHeader, FeedMessage,
};
use tycho_common::{
    models::{
        blockchain::{
            AccountOverrides, Block, EntryPoint, EntryPointWithTracingParams, RPCTracerParams,
            StorageOverride, TracingParams, TracingResult,
        },
        contract::Account,
        protocol::{ProtocolComponent, ProtocolComponentState},
        token::Token,
        Chain,
    },
    traits::{AccountExtractor, EntryPointTracer, StorageSnapshotRequest},
    Bytes,
};
use tycho_ethereum::{
    rpc::{
        config::{RPCBatchingConfig, RPCRetryConfig},
        EthereumRpcClient,
    },
    services::{
        account_extractor::EVMAccountExtractor, entrypoint_tracer::tracer::EVMEntrypointService,
    },
};
use tycho_simulation::evm::{
    decoder::TychoStreamDecoder,
    engine_db::{tycho_db::PreCachedDB, SHARED_TYCHO_DB},
    protocol::vm::{constants::EXTERNAL_ACCOUNT, state::EVMPoolState},
};

const PROTOCOL: &str = "vm:hanji";
/// Widest price the market accepts (FP24: 999999 * 10^15).
const MAX_PRICE: u128 = 999_999_000_000_000_000_000;
/// Pyth `PriceFeed` slots end in the publish time; anything this close to B's timestamp is one.
const PYTH_MAX_AGE: u64 = 86_400;
/// Error prefix for a block where the market quotes nothing fillable.
const NO_FILL: &str = "no fillable";
/// Age the stale case gives every Pyth price, past the quoter's 60 s window.
const STALE_AGE: u64 = 120;
/// The active fast-quoter proxies on Monad mainnet.
const PROXIES: [&str; 7] = [
    "0x1aed222dda944a87703c918745b11be13f8eef10", // MON/USDC
    "0xc4adcb94d0dce7fc84e180fae3621df13ad37e06", // MON/WETH
    "0x0acba24cecd750badb3179fb9ee3f2cd27431778", // WETH/USDC
    "0xc11805e92ca6a36ce9507902ab6bb5cd11e438bf", // cbBTC/USDC
    "0x012e06b56eee881603e47748931dc38efabf54ef", // cbBTC/MON
    "0x05e028d33fa727168bc9396bdac5cab387c8a196", // cbBTC/WETH
    "0x88687e499846ce3037e63754a6e2e7b53483c70e", // XAUt0/USDC
];

sol! {
    function getConfig() external view returns (
        uint256 scalingX, uint256 scalingY, address tokenX, address tokenY, bool supportsNative,
        bool isTokenXWeth, address askTrie, address bidTrie, uint64 adminRate,
        uint64 aggressiveRate, uint64 passiveRate, uint64 payoutRate, bool invokeOnTrade
    );
    function lobAddress() external view returns (address);
    function lpManagerAddress() external view returns (address);
    function getWatchDogAddress() external view returns (address);
    function isChainStable() external view returns (bool);
    function placeOrder(
        bool isAsk, uint128 quantity, uint72 price, uint128 maxCommission, bool marketOnly,
        bool postOnly, bool transferExecutedTokens, uint256 expires
    ) external payable returns (uint64, uint128 shares, uint128 value, uint128 fee);
    function placeMarketOrderWithTargetValue(
        bool isAsk, uint128 targetValue, uint72 price, uint128 maxCommission,
        bool transferExecutedTokens, uint256 expires
    ) external payable returns (uint128 shares, uint128 value, uint128 fee);
    function decimals() external view returns (uint8);
    function balanceOf(address owner) external view returns (uint256);
    function allowance(address owner, address spender) external view returns (uint256);
    function symbol() external view returns (string);
}

#[derive(Parser)]
struct Args {
    /// Needs `debug_traceCall` and historical state (public: rpc-mainnet.monadinfra.com).
    #[arg(long, default_value = "https://rpc-mainnet.monadinfra.com")]
    rpc: String,
    /// Proxies to check; defaults to every active market.
    #[arg(long)]
    proxy: Vec<Address>,
    #[arg(long, default_value_t = 5)]
    rounds: usize,
    /// Fully filled sizes required per side per round.
    #[arg(long, default_value_t = 20)]
    samples: usize,
    /// How far back the checked blocks may lie.
    #[arg(long, default_value_t = 20_000)]
    lookback: u64,
    #[arg(long)]
    seed: Option<u64>,
    /// Check one block instead of random ones.
    #[arg(long)]
    at: Option<u64>,
}

/// One market's static wiring.
struct Market {
    proxy: Address,
    x: Token,
    y: Token,
    scaling_x: U256,
    scaling_y: U256,
}

/// Unverified proxy getter for the alternate fast quoter.
const ALT_QUOTER_SELECTOR: [u8; 4] = [0xf8, 0xbe, 0xd2, 0x5f];
/// Unverified fast-quoter view the proxy calls to size its quotes:
/// `(uint8 market, bool isAsk, uint128 quantity, uint128 maxValue, uint72 price)`.
const QUOTER_VIEW_SELECTOR: [u8; 4] = [0x24, 0x67, 0x5a, 0x09];
/// Pyth `getPriceNoOlderThan(bytes32,uint256)`.
const PYTH_PRICE_SELECTOR: &str = "0xa4ae35e0";

/// A quote request: sell `amount` of X (ask) or of Y (bid).
#[derive(Clone, Copy)]
struct Order {
    ask: bool,
    amount: U256,
}

struct Chain_ {
    p: DynProvider,
    rpc: EthereumRpcClient,
}

/// The adapter address `EVMPoolState` deploys `vm:hanji` at: the protocol name, left-padded.
fn adapter_address() -> Address {
    Address::from_str(&format!("{:0>40}", hex::encode("hanji"))).expect("valid address")
}

fn entry(target: Address, signature: &str, calldata: Vec<u8>) -> EntryPointWithTracingParams {
    let target = Bytes::from(target.as_slice());
    EntryPointWithTracingParams {
        entry_point: EntryPoint::new(format!("{target}:{signature}"), target, signature.into()),
        params: TracingParams::RPCTracer(RPCTracerParams::new(None, calldata.into())),
    }
}

/// Contracts an entry point set is built from; all but `watchdog` and `alt` come from the
/// proxy factory's creation event.
struct Wiring {
    proxy: Address,
    lob: Address,
    lp_manager: Address,
    tokens: [Address; 2],
    watchdog: Address,
    alt: Address,
}

/// The entry points `monad-hanji` registers for a proxy:
/// - a zero-fill order per side on the proxy: it walks the quote path (quoters, oracle, Pyth, LP
///   manager, market, tries) without moving tokens;
/// - each token's `balanceOf` the LP manager and the market, and the LP manager's allowance to the
///   market: the slots a fill moves, which the DCI indexes selectively on tokens;
/// - the market's watchdog (only called on fills) and the alternate quoter (only used for
///   whitelisted origins).
fn entry_points(w: &Wiring) -> Vec<EntryPointWithTracingParams> {
    let zero_fill = |ask: bool| {
        placeOrderCall {
            isAsk: ask,
            quantity: 1,
            price: if ask { MAX_PRICE } else { 1 }
                .try_into()
                .expect("fits uint72"),
            maxCommission: u128::MAX,
            marketOnly: true,
            postOnly: false,
            transferExecutedTokens: true,
            expires: U256::MAX,
        }
        .abi_encode()
    };
    let sig = "placeOrder(bool,uint128,uint72,uint128,bool,bool,bool,uint256)";
    let mut out = vec![entry(w.proxy, sig, zero_fill(true)), entry(w.proxy, sig, zero_fill(false))];
    for token in w.tokens {
        for owner in [w.lp_manager, w.lob] {
            out.push(entry(token, "balanceOf(address)", balanceOfCall { owner }.abi_encode()));
        }
        out.push(entry(
            token,
            "allowance(address,address)",
            allowanceCall { owner: w.lp_manager, spender: w.lob }.abi_encode(),
        ));
    }
    out.push(entry(w.watchdog, "isChainStable()", isChainStableCall {}.abi_encode()));
    out.push(entry(w.alt, "0x24675a09(uint8,bool,uint128,uint128,uint72)", {
        let words =
            [U256::ZERO, U256::ZERO, U256::from(4000u64), U256::from(u128::MAX), U256::from(1u8)];
        let mut calldata = QUOTER_VIEW_SELECTOR.to_vec();
        for w in words {
            calldata.extend(w.to_be_bytes::<32>());
        }
        calldata
    }));
    out
}

impl Chain_ {
    async fn call<C: SolCall>(&self, to: Address, call: C, block: u64) -> Result<C::Return> {
        let raw = self
            .p
            .call(
                TransactionRequest::default()
                    .to(to)
                    .input(call.abi_encode().into()),
            )
            .block(BlockId::number(block))
            .await?;
        Ok(C::abi_decode_returns(&raw)?)
    }

    async fn market(&self, proxy: Address, block: u64) -> Result<Market> {
        let c = self
            .call(proxy, getConfigCall {}, block)
            .await?;
        let token = |addr: Address| async move {
            let decimals = self
                .call(addr, decimalsCall {}, block)
                .await?;
            let symbol = self
                .call(addr, symbolCall {}, block)
                .await?;
            let addr = Bytes::from(addr.as_slice());
            Ok::<_, anyhow::Error>(Token::new(
                &addr,
                &symbol,
                decimals.into(),
                0,
                &[],
                Chain::Monad,
                100,
            ))
        };
        Ok(Market {
            proxy,
            x: token(c.tokenX).await?,
            y: token(c.tokenY).await?,
            scaling_x: c.scalingX,
            scaling_y: c.scalingY,
        })
    }

    async fn block(&self, number: u64) -> Result<Block> {
        Ok(EVMAccountExtractor::new(&self.rpc, Chain::Monad)
            .get_block_data(number)
            .await?)
    }
}

impl Market {
    /// The proxy call the adapter makes for `order`.
    fn calldata(&self, order: Order) -> Vec<u8> {
        if order.ask {
            placeOrderCall {
                isAsk: true,
                quantity: (order.amount / self.scaling_x).to(),
                price: 1u64.try_into().expect("fits uint72"),
                maxCommission: u128::MAX,
                marketOnly: true,
                postOnly: false,
                transferExecutedTokens: true,
                expires: U256::MAX,
            }
            .abi_encode()
        } else {
            placeMarketOrderWithTargetValueCall {
                isAsk: false,
                targetValue: (order.amount / self.scaling_y).to(),
                price: MAX_PRICE
                    .try_into()
                    .expect("fits uint72"),
                maxCommission: u128::MAX,
                transferExecutedTokens: true,
                expires: U256::MAX,
            }
            .abi_encode()
        }
    }

    /// The buy-token amount the adapter reports for `order` given the proxy's return data, or
    /// `None` when the book cannot fill it (the adapter reverts then).
    fn filled(&self, order: Order, raw: &[u8]) -> Result<Option<U256>> {
        if order.ask {
            let r = placeOrderCall::abi_decode_returns(raw)?;
            let want = order.amount / self.scaling_x;
            Ok((U256::from(r.shares) == want).then(|| U256::from(r.value - r.fee) * self.scaling_y))
        } else {
            let r = placeMarketOrderWithTargetValueCall::abi_decode_returns(raw)?;
            let target = order.amount / self.scaling_y;
            let spent = U256::from(r.value) + U256::from(r.fee);
            let full =
                r.shares != 0 && spent <= target && (target - spent) * U256::from(r.shares) < spent;
            Ok(full.then(|| U256::from(r.shares) * self.scaling_x))
        }
    }
}

fn addr(t: &Token) -> Address {
    Address::from_slice(&t.address)
}

/// The (account, slot) pairs holding the adapter address's balance of `token` and its allowance
/// to `spender`.
///
/// The slots come from the access list of `balanceOf`/`allowance` (the indexer's DCI tracer) and
/// are confirmed by overriding each candidate. Tycho's slot detectors read candidates from the
/// prestate tracer instead, which on Monad leaves out slots holding zero — the adapter's.
async fn funding_slots(
    c: &Chain_,
    block: &Block,
    token: Address,
    spender: Address,
) -> Result<Vec<(Bytes, Bytes)>> {
    let owner = adapter_address();
    let mut out = Vec::new();
    for calldata in
        [balanceOfCall { owner }.abi_encode(), allowanceCall { owner, spender }.abi_encode()]
    {
        out.push(overridable_slot(c, block, token, calldata).await?);
    }
    Ok(out)
}

/// Overrides setting every slot in `slots` to `amount`.
fn funded(slots: &[(Bytes, Bytes)], amount: U256) -> BTreeMap<Bytes, AccountOverrides> {
    let value = Bytes::from(amount.to_be_bytes::<32>().to_vec());
    let mut diffs: BTreeMap<Bytes, BTreeMap<Bytes, Bytes>> = BTreeMap::new();
    for (at, slot) in slots {
        diffs
            .entry(at.clone())
            .or_default()
            .insert(slot.clone(), value.clone());
    }
    diffs
        .into_iter()
        .map(|(at, d)| {
            (
                at,
                AccountOverrides {
                    slots: Some(StorageOverride::Diff(d)),
                    native_balance: None,
                    code: None,
                },
            )
        })
        .collect()
}

/// The storage slot whose value a `uint256` view on `token` returns.
async fn overridable_slot(
    c: &Chain_,
    block: &Block,
    token: Address,
    calldata: Vec<u8>,
) -> Result<(Bytes, Bytes)> {
    let traced = EVMEntrypointService::new_with_config(&c.rpc, 10, 2000)
        .trace(
            block.hash.clone(),
            vec![EntryPointWithTracingParams {
                entry_point: EntryPoint::new("slot".into(), token.as_slice().into(), "".into()),
                params: TracingParams::RPCTracer(RPCTracerParams::new(
                    None,
                    calldata.clone().into(),
                )),
            }],
        )
        .await
        .remove(0)
        .map_err(|e| anyhow!("tracing a view on {token}: {e:?}"))?;
    let marker = U256::from(0x5eed_u64) << 100usize;
    for (at, slots) in traced.tracing_result.accessed_slots {
        for slot in slots {
            let diff =
                BTreeMap::from([(slot.clone(), Bytes::from(marker.to_be_bytes::<32>().to_vec()))]);
            let o = BTreeMap::from([(
                at.clone(),
                AccountOverrides {
                    slots: Some(StorageOverride::Diff(diff)),
                    native_balance: None,
                    code: None,
                },
            )]);
            let raw =
                c.p.call(
                    TransactionRequest::default()
                        .to(token)
                        .input(calldata.clone().into()),
                )
                .block(BlockId::number(block.number))
                .overrides(to_alloy(&o))
                .await;
            if raw.is_ok_and(|r| U256::abi_decode(&r).is_ok_and(|v| v == marker)) {
                return Ok((at, slot));
            }
        }
    }
    bail!("no storage slot of {token} controls the view result")
}

fn to_alloy(overrides: &BTreeMap<Bytes, AccountOverrides>) -> StateOverride {
    let mut state = StateOverride::default();
    for (a, o) in overrides {
        let state_diff = match &o.slots {
            Some(StorageOverride::Diff(slots)) => Some(
                slots
                    .iter()
                    .map(|(k, v)| (B256::from_slice(k), B256::from_slice(v)))
                    .collect(),
            ),
            _ => None,
        };
        state.insert(
            Address::from_slice(a),
            AccountOverride {
                state_diff,
                code: o
                    .code
                    .as_ref()
                    .map(|c| ABytes::from(c.to_vec())),
                ..Default::default()
            },
        );
    }
    state
}

/// A proxy call as the adapter makes it inside the simulation: from a contract (the adapter's
/// address, running a forwarder) under a different `tx.origin` (`EXTERNAL_ACCOUNT`). The proxy
/// prices contract callers whose origin it has not whitelisted differently from EOAs, so an
/// `eth_call` straight from an EOA is not the quote the adapter gets.
fn as_adapter(
    proxy: Address,
    calldata: Vec<u8>,
    mut overrides: BTreeMap<Bytes, AccountOverrides>,
) -> RPCTracerParams {
    // CALLDATACOPY the input, CALL the proxy with it, return or revert with what it returned.
    let forwarder = [
        &hex::decode("365f5f375f5f365f5f73").expect("hex")[..],
        proxy.as_slice(),
        &hex::decode("5af13d5f5f3e602a573d5ffd5b3d5ff3").expect("hex")[..],
    ]
    .concat();
    overrides.insert(
        Bytes::from(adapter_address().as_slice()),
        AccountOverrides { slots: None, native_balance: None, code: Some(forwarder.into()) },
    );
    RPCTracerParams::new(Some(Bytes::from(EXTERNAL_ACCOUNT.as_slice())), calldata.into())
        .with_state_overrides(overrides)
}

/// Every Pyth `PriceFeed` slot the traces read, rewritten `STALE_AGE` seconds old.
fn stale_pyth(
    accessed: &HashMap<Bytes, HashSet<Bytes>>,
    storage: &HashMap<Bytes, Account>,
    ts: u64,
) -> BTreeMap<Bytes, AccountOverrides> {
    let mut out = BTreeMap::new();
    for (a, slots) in accessed {
        let Some(acc) = storage.get(a) else { continue };
        let mut diff = BTreeMap::new();
        for s in slots {
            let Some(v) = acc.slots.get(s) else { continue };
            let word = U256::from_be_slice(v.as_ref());
            let publish = (word & U256::from(u64::MAX)).to::<u64>();
            if publish > ts.saturating_sub(PYTH_MAX_AGE) &&
                publish <= ts &&
                word > U256::from(u64::MAX)
            {
                let stale: U256 = (word >> 64usize << 64usize) | U256::from(ts - STALE_AGE);
                diff.insert(s.clone(), Bytes::from(stale.to_be_bytes::<32>().to_vec()));
            }
        }
        if !diff.is_empty() && is_pyth(acc) {
            out.insert(
                a.clone(),
                AccountOverrides {
                    slots: Some(StorageOverride::Diff(diff)),
                    native_balance: None,
                    code: None,
                },
            );
        }
    }
    out
}

/// Marked by `seed`, which resolves the Pyth contract from the quote path.
fn is_pyth(acc: &Account) -> bool {
    acc.title == "pyth"
}

/// `n` log-uniform sizes in `[lo, hi]`.
fn sizes(ask: bool, lo: U256, hi: U256, n: usize, rng: &mut u64) -> Vec<Order> {
    let span = (u256_f64(hi) / u256_f64(lo))
        .log10()
        .max(0.0);
    (0..n)
        .map(|_| {
            *rng ^= *rng << 13;
            *rng ^= *rng >> 7;
            *rng ^= *rng << 17;
            let f = 10f64.powf((*rng % 1_000_000) as f64 / 1e6 * span);
            let amount = lo * U256::from((f * 1e6) as u128) / U256::from(1_000_000u64);
            Order { ask, amount: amount.max(lo) }
        })
        .collect()
}

fn u256_f64(v: U256) -> f64 {
    v.to_string().parse().expect("decimal")
}

struct Outcome {
    stale: bool,
    matched: usize,
    unfillable: usize,
}

/// The proxy's own quote for `order` at `block`, as the adapter would get it: the buy-token
/// amount, or `None` when the book cannot fill it.
async fn reference(
    c: &Chain_,
    m: &Market,
    block: u64,
    funding: &[(Bytes, Bytes)],
    order: Order,
    extra: &BTreeMap<Bytes, AccountOverrides>,
) -> Result<Option<U256>> {
    let mut o = funded(funding, order.amount);
    o.extend(extra.clone());
    let call = as_adapter(m.proxy, m.calldata(order), o);
    let raw =
        c.p.call(
            TransactionRequest::default()
                .from(*EXTERNAL_ACCOUNT)
                .to(adapter_address())
                .input(ABytes::from(call.calldata.to_vec()).into()),
        )
        .block(BlockId::number(block))
        .overrides(to_alloy(
            call.state_overrides
                .as_ref()
                .expect("set"),
        ))
        .await;
    match raw {
        Ok(raw) => m.filled(order, &raw),
        // The proxy reverted: the order cannot be placed.
        Err(e) if e.as_error_resp().is_some() => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Checks one market at one block, as indexed and with every Pyth price made stale.
async fn check(
    c: &Chain_,
    m: &Market,
    block: &Block,
    samples: usize,
    rng: &mut u64,
) -> Result<Vec<Outcome>> {
    let b = block.number;
    let lob = c
        .call(m.proxy, lobAddressCall {}, b)
        .await?;
    let watchdog = c
        .call(lob, getWatchDogAddressCall {}, b)
        .await?;
    let lp_manager = c
        .call(m.proxy, lpManagerAddressCall {}, b)
        .await?;
    let alt = Address::abi_decode(
        &c.p.call(
            TransactionRequest::default()
                .to(m.proxy)
                .input(ALT_QUOTER_SELECTOR.to_vec().into()),
        )
        .block(BlockId::number(b))
        .await?,
    )?;
    // The package registers the watchdog and alternate quoters from its params, not per proxy.
    let params =
        include_str!("../../../protocols/substreams/monad-hanji/monad-hanji.yaml").to_lowercase();
    for (what, a) in [("watchdog", watchdog), ("alternate quoter", alt)] {
        if !params.contains(&format!("{a:#x}")) {
            bail!("{what} {a} is missing from the monad-hanji params");
        }
    }
    let tracer = EVMEntrypointService::new_with_config(&c.rpc, 10, 2000);
    let dci: Vec<(EntryPointWithTracingParams, TracingResult)> = tracer
        .trace(
            block.hash.clone(),
            entry_points(&Wiring {
                proxy: m.proxy,
                lob,
                lp_manager,
                tokens: [addr(&m.x), addr(&m.y)],
                watchdog,
                alt,
            }),
        )
        .await
        .into_iter()
        .map(|r| r.map(|t| (t.entry_point_with_params, t.tracing_result)))
        .collect::<Result<_, _>>()
        .map_err(|e| anyhow!("DCI trace failed: {e:?}"))?;
    let mut accessed: HashMap<Bytes, HashSet<Bytes>> = HashMap::new();
    for (_, r) in &dci {
        for (a, s) in &r.accessed_slots {
            accessed
                .entry(a.clone())
                .or_default()
                .extend(s.iter().cloned());
        }
    }
    let tokens: HashSet<Bytes> = [m.x.address.clone(), m.y.address.clone()].into();

    // The adapter's getLimits probe sizes, then the sampled ones; seed from what each reads.
    // getLimits' depth probe: one order per side for far more than the book holds.
    let probes = [
        Order { ask: true, amount: m.scaling_x << 96 },
        Order { ask: false, amount: m.scaling_y << 96 },
    ];
    let funding_x = funding_slots(c, block, addr(&m.x), m.proxy).await?;
    let funding_y = funding_slots(c, block, addr(&m.y), m.proxy).await?;
    let overrides =
        |order: &Order| funded(if order.ask { &funding_x } else { &funding_y }, order.amount);

    // Sizes: log-uniform over each side's fillable range (first to last fillable power of ten
    // above one scaling unit), stretched 3x past the top so some sizes run out of book.
    let mut orders = Vec::new();
    for ask in [true, false] {
        let (unit, funding) =
            if ask { (m.scaling_x, &funding_x) } else { (m.scaling_y, &funding_y) };
        let mut fillable = Vec::new();
        for k in 0..24u64 {
            let order = Order { ask, amount: unit * U256::from(10u64).pow(U256::from(k)) };
            if reference(c, m, b, funding, order, &BTreeMap::new())
                .await?
                .is_some()
            {
                fillable.push(order.amount);
            } else if !fillable.is_empty() {
                break;
            }
        }
        let (Some(lo), Some(hi)) = (fillable.first(), fillable.last()) else {
            bail!("{NO_FILL} {} size", if ask { "sell" } else { "buy" });
        };
        orders.extend(sizes(ask, *lo, *hi * U256::from(3u8), samples * 2, rng));
    }

    // What the adapter's own calls read: the proxy views it resolves the market with, and every
    // order it may place (getLimits probes and the sampled sizes).
    let mut calls: Vec<RPCTracerParams> = [
        getConfigCall {}.abi_encode(),
        lobAddressCall {}.abi_encode(),
        lpManagerAddressCall {}.abi_encode(),
    ]
    .into_iter()
    .map(|d| as_adapter(m.proxy, d, BTreeMap::new()))
    .collect();
    for order in probes.iter().chain(&orders) {
        calls.push(as_adapter(m.proxy, m.calldata(*order), overrides(order)));
    }
    let mut reads: HashMap<Bytes, HashSet<Bytes>> = HashMap::new();
    let traced = tracer
        .trace(
            block.hash.clone(),
            calls
                .into_iter()
                .map(|p| EntryPointWithTracingParams {
                    entry_point: EntryPoint::new(
                        "adapter".into(),
                        adapter_address().as_slice().into(),
                        "".into(),
                    ),
                    params: TracingParams::RPCTracer(p),
                })
                .collect(),
        )
        .await;
    // Calls that revert on chain (orders too small or too large to fill) trace nothing; the
    // simulation reverts on them too.
    for t in traced.into_iter().flatten() {
        for (a, slots) in t.tracing_result.accessed_slots {
            reads
                .entry(a)
                .or_default()
                .extend(slots);
        }
    }
    // Every contract a real quote reads must be one the DCI tracks (or a token, or the caller).
    let adapter = Bytes::from(adapter_address().as_slice());
    for a in reads.keys() {
        if !accessed.contains_key(a) && !tokens.contains(a) && *a != adapter {
            bail!("quote path reads {a}, which no registered entry point reaches");
        }
    }

    let fresh = seed(c, block, m, &accessed, &reads, &tokens).await?;
    let mut outcomes = Vec::new();
    for stale in [false, true] {
        let mut storage = fresh.clone();
        let mut stale_overrides = BTreeMap::new();
        if stale {
            stale_overrides = stale_pyth(&reads, &storage, block_ts(block));
            if stale_overrides.is_empty() {
                bail!("no Pyth price slot found to make stale");
            }
            for (a, ov) in &stale_overrides {
                let Some(StorageOverride::Diff(d)) = &ov.slots else { continue };
                let acc = storage.get_mut(a).expect("seeded");
                acc.slots.extend(
                    d.iter()
                        .map(|(k, v)| (k.clone(), v.clone())),
                );
            }
        }

        let mut expected = Vec::new();
        for order in &orders {
            let funding = if order.ask { &funding_x } else { &funding_y };
            expected.push((*order, reference(c, m, b, funding, *order, &stale_overrides).await?));
        }
        let per_side = |ask: bool| {
            expected
                .iter()
                .filter(|(o, e)| o.ask == ask && e.is_some())
                .count()
        };
        if per_side(true) < samples || per_side(false) < samples {
            bail!(
                "stale={stale}: only {}/{} fillable sizes (ask/bid), need {samples}",
                per_side(true),
                per_side(false)
            );
        }

        let state = decode(m, block, dci.clone(), storage).await?;
        let mut out = Outcome { stale, matched: 0, unfillable: 0 };
        for (order, want) in expected {
            let (sell, buy) = if order.ask { (&m.x, &m.y) } else { (&m.y, &m.x) };
            let amount = BigUint::from_bytes_be(&order.amount.to_be_bytes::<32>());
            match (want, state.get_amount_out(amount, sell, buy)) {
                (Some(w), Ok(g)) if g.amount == BigUint::from_bytes_be(&w.to_be_bytes::<32>()) => {
                    out.matched += 1
                }
                (None, Err(_)) => out.unfillable += 1,
                (w, g) => bail!(
                    "stale={stale} sell {} {}: chain {:?} != sim {:?}",
                    order.amount,
                    sell.symbol,
                    w,
                    g.map(|r| r.amount)
                ),
            }
        }
        outcomes.push(out);
    }
    Ok(outcomes)
}

fn block_ts(block: &Block) -> u64 {
    block.ts.and_utc().timestamp() as u64
}

/// The account snapshot at B: DCI-traced tokens keep only their traced slots; every other traced
/// contract gets each slot any traced call read (standing in for its full storage).
async fn seed(
    c: &Chain_,
    block: &Block,
    m: &Market,
    accessed: &HashMap<Bytes, HashSet<Bytes>>,
    reads: &HashMap<Bytes, HashSet<Bytes>>,
    tokens: &HashSet<Bytes>,
) -> Result<HashMap<Bytes, Account>> {
    let mut requests = Vec::new();
    for (a, slots) in accessed {
        let mut slots = slots.clone();
        if !tokens.contains(a) {
            slots.extend(
                reads
                    .get(a)
                    .into_iter()
                    .flatten()
                    .cloned(),
            );
        }
        requests.push(StorageSnapshotRequest {
            address: a.clone(),
            slots: Some(slots.into_iter().collect()),
        });
    }
    let pyth = pyth_address(c, m, block.number).await?;
    Ok(EVMAccountExtractor::new(&c.rpc, Chain::Monad)
        .get_accounts_at_block(block, &requests)
        .await?
        .into_iter()
        .map(|(a, delta)| {
            let mut acc = delta.into_account_without_tx();
            if Some(&a) == pyth.as_ref() {
                acc.title = "pyth".into();
            }
            (a, acc)
        })
        .collect())
}

/// The Pyth contract: the callee of `getPriceNoOlderThan` on the proxy's quote path.
async fn pyth_address(c: &Chain_, m: &Market, block: u64) -> Result<Option<Bytes>> {
    let zero_fill = placeOrderCall {
        isAsk: true,
        quantity: 1,
        price: MAX_PRICE
            .try_into()
            .expect("fits uint72"),
        maxCommission: u128::MAX,
        marketOnly: true,
        postOnly: false,
        transferExecutedTokens: true,
        expires: U256::MAX,
    }
    .abi_encode();
    let trace: serde_json::Value =
        c.p.raw_request(
            "debug_traceCall".into(),
            (
                serde_json::json!({"to": m.proxy, "data": format!("0x{}", hex::encode(zero_fill))}),
                format!("0x{block:x}"),
                serde_json::json!({"tracer": "callTracer"}),
            ),
        )
        .await?;
    fn find(v: &serde_json::Value) -> Option<String> {
        if v["type"] == "STATICCALL" &&
            v["input"]
                .as_str()
                .is_some_and(|i| i.starts_with(PYTH_PRICE_SELECTOR))
        {
            return v["to"].as_str().map(str::to_owned);
        }
        v["calls"]
            .as_array()?
            .iter()
            .find_map(find)
    }
    find(&trace)
        .map(|a| Bytes::from_str(&a).map_err(|e| anyhow!("bad address {a}: {e}")))
        .transpose()
}

/// The `vm:hanji` snapshot at B, through the same decoder a Tycho client uses.
async fn decode(
    m: &Market,
    block: &Block,
    entrypoints: Vec<(EntryPointWithTracingParams, TracingResult)>,
    vm_storage: HashMap<Bytes, Account>,
) -> Result<Box<dyn tycho_common::simulation::protocol_sim::ProtocolSim>> {
    SHARED_TYCHO_DB.clear()?;
    let mut decoder = TychoStreamDecoder::<BlockHeader>::new(Chain::Monad);
    decoder.register_decoder_with_context::<EVMPoolState<PreCachedDB>>(
        PROTOCOL,
        tycho_simulation::protocol::models::DecoderContext::new()
            .vm_traces(std::env::var("HANJI_TRACE").is_ok()),
    );
    decoder
        .set_tokens(HashMap::from([
            (m.x.address.clone(), m.x.clone()),
            (m.y.address.clone(), m.y.clone()),
        ]))
        .await;
    let id = format!("0x{}", hex::encode(m.proxy));
    let component = ProtocolComponent {
        id: id.clone(),
        protocol_system: PROTOCOL.into(),
        protocol_type_name: "hanji_market".into(),
        chain: Chain::Monad,
        tokens: vec![m.x.address.clone(), m.y.address.clone()],
        ..Default::default()
    };
    let header = BlockHeader {
        hash: block.hash.clone(),
        number: block.number,
        parent_hash: block.parent_hash.clone(),
        revert: false,
        timestamp: block_ts(block),
        partial_block_index: None,
    };
    let msg = FeedMessage {
        state_msgs: HashMap::from([(
            PROTOCOL.to_string(),
            StateSyncMessage {
                header,
                snapshots: Snapshot {
                    states: HashMap::from([(
                        id.clone(),
                        ComponentWithState {
                            state: ProtocolComponentState::new(&id, HashMap::new(), HashMap::new()),
                            component,
                            component_tvl: None,
                            entrypoints,
                        },
                    )]),
                    vm_storage,
                },
                deltas: None,
                removed_components: HashMap::new(),
            },
        )]),
        sync_states: HashMap::new(),
    };
    decoder
        .decode(&msg)
        .await?
        .states
        .remove(&id)
        .ok_or_else(|| anyhow!("decoder produced no state for {id}"))
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    // Public endpoints rate-limit: small batches, patient retries.
    let rpc = EthereumRpcClient::new(&args.rpc)?
        .with_batching(RPCBatchingConfig::Enabled {
            max_batch_size: 10,
            storage_slot_max_batch_size_override: Some(25),
        })
        .with_retry(RPCRetryConfig {
            max_retries: 10,
            initial_backoff_ms: 500,
            max_backoff_ms: 8000,
        });
    let client = RpcClient::builder()
        .layer(RetryBackoffLayer::new(10, 500, 50))
        .http(args.rpc.parse()?);
    let c = Chain_ {
        p: ProviderBuilder::new()
            .connect_client(client)
            .erased(),
        rpc,
    };
    let head = c.p.get_block_number().await?;
    let mut rng = args.seed.unwrap_or(head) | 1;
    let proxies: Vec<Address> = if args.proxy.is_empty() {
        PROXIES
            .iter()
            .map(|p| p.parse().expect("valid proxy"))
            .collect()
    } else {
        args.proxy.clone()
    };
    let (mut matched, mut unfillable) = (0, 0);
    for proxy in proxies {
        let m = c.market(proxy, head).await?;
        for round in 0..args.rounds {
            // A block where the market quotes nothing fillable is redrawn.
            let mut attempt = 0;
            let (b, outcomes) = loop {
                rng ^= rng << 13;
                rng ^= rng >> 7;
                rng ^= rng << 17;
                let b = args
                    .at
                    .unwrap_or(head - 5 - rng % args.lookback);
                let block = c.block(b).await?;
                match check(&c, &m, &block, args.samples, &mut rng).await {
                    Ok(o) => break (b, o),
                    Err(e) if e.to_string().starts_with(NO_FILL) && attempt < 3 => {
                        println!("{}/{} block {b}: {e}, redrawn", m.x.symbol, m.y.symbol);
                        attempt += 1;
                    }
                    Err(e) => {
                        bail!("{}/{} round {round} block {b}: {e}", m.x.symbol, m.y.symbol)
                    }
                }
            };
            for o in outcomes {
                println!(
                    "{}/{} block {b} stale={}: {} exact, {} unfillable on both",
                    m.x.symbol, m.y.symbol, o.stale, o.matched, o.unfillable
                );
                matched += o.matched;
                unfillable += o.unfillable;
            }
        }
    }
    println!("parity OK: {matched} exact quotes, {unfillable} unfillable on both sides");
    Ok(())
}
