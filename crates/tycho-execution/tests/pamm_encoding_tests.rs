//! Encodes the prop-AMM hops (`thog`, `woofi`, `lfj_poe`) through the real router encoder on
//! Monad, next to an existing venue (Kuru, Hanji), and checks the bytes against the layouts the
//! executors decode (`ThogExecutor`/`WooFiExecutor`/`PoeExecutor._decodeData`).
//!
//! Single-swap vectors are also written to `contracts/test/assets/calldata.txt`, where
//! `Thog.t.sol`/`WooFi.t.sol`/`Poe.t.sol` load them and execute them through a `LeanTychoRouter`
//! on a Monad fork.
mod common;
use std::str::FromStr;

use alloy::hex::encode;
use num_bigint::BigUint;
use tycho_common::{
    models::{protocol::ProtocolComponent, Chain},
    Bytes,
};
use tycho_execution::encoding::{
    evm::{
        encoder_builders::TychoRouterEncoderBuilder,
        swap_encoder::swap_encoder_registry::SwapEncoderRegistry, utils::write_calldata_to_file,
    },
    models::{default_token, Solution, Swap},
    tycho_encoder::TychoEncoder,
};

use crate::common::{alice_address, router_address};

// Test-only executor addresses, supplied the way a deployment supplies the real ones.
const EX_KURU: &str = "1111111111111111111111111111111111111111";
const EX_HANJI: &str = "2222222222222222222222222222222222222222";
const EX_THOG: &str = "3333333333333333333333333333333333333333";
const EX_WOOFI: &str = "4444444444444444444444444444444444444444";
const EX_POE: &str = "5555555555555555555555555555555555555555";

const KURU_MARKET: &str = "065c9d28e428a0db40191a54d33d5b7c71a9c394";
const HANJI_PROXY: &str = "1aed222dda944a87703c918745b11be13f8eef10";
const THOG_AMM: &str = "80c74517bcc2d67ffe02d3ed886796272f647210";
const WOOFI_POOL: &str = "5520385bfcf07ec87c4c53a7d8d65595dff69fa4";
const POE_POOL: &str = "02a8a16613a421eabad6861ff6d8159f6d5edb8f";

const WMON: &str = "3bd359c1119da7da1d913d1c4d2b7c461115433a";
const USDC: &str = "754704bc059f8c67012fed69bc8a327a5aafb603";
const WETH: &str = "ee8c0e9f1bffb4eb878d8f15f368a02a35481242";
const WBTC: &str = "0555e30da8f98308edb960aa94c0db47230d2b9c";

const SEQUENTIAL_SIG: &str = "sequentialSwap(uint256,address,address,uint256,uint256,address,\
(uint32,address,uint256,uint256,bytes),bytes)";

fn registry() -> SwapEncoderRegistry {
    let json = format!(
        r#"{{"monad":{{"kuru":"0x{EX_KURU}","vm:hanji":"0x{EX_HANJI}","thog":"0x{EX_THOG}","woofi":"0x{EX_WOOFI}","lfj_poe":"0x{EX_POE}"}}}}"#
    );
    SwapEncoderRegistry::new(Chain::Monad)
        .add_default_encoders(Some(json))
        .unwrap()
}

fn encoder() -> Box<dyn TychoEncoder> {
    TychoRouterEncoderBuilder::new()
        .chain(Chain::Monad)
        .swap_encoder_registry(registry())
        .router_address(router_address())
        .build()
        .expect("Failed to build encoder")
}

fn b(hex: &str) -> Bytes {
    Bytes::from_str(&format!("0x{hex}")).unwrap()
}

/// A hop of `protocol` on the venue `id`, selling `t_in` for `t_out`; `tokens` = the component's
/// token list (for POE: X, Y).
fn hop(protocol: &str, id: &str, tokens: [&str; 2], t_in: &str, t_out: &str) -> Swap {
    let component = ProtocolComponent {
        id: format!("0x{id}"),
        protocol_system: protocol.to_owned(),
        tokens: tokens.iter().map(|t| b(t)).collect(),
        ..Default::default()
    };
    Swap::new(component, default_token(b(t_in)), default_token(b(t_out)), BigUint::ZERO)
}

/// `[u16 len][executor][venueData]` for each hop, the router program of a sequential swap.
fn program(hops: &[(&str, &str)]) -> String {
    hops.iter()
        .map(|(executor, data)| {
            let body = format!("{executor}{data}");
            format!("{:04x}{body}", body.len() / 2)
        })
        .collect()
}

fn solve(swaps: Vec<Swap>) -> (String, String) {
    let token_in = swaps[0].token_in().address.clone();
    let token_out = swaps[swaps.len() - 1]
        .token_out()
        .address
        .clone();
    let solution = Solution::new(
        alice_address(),
        alice_address(),
        token_in,
        token_out,
        BigUint::from_str("1000000000000000000").unwrap(),
        BigUint::from(1000u32),
        BigUint::from(1u32),
        swaps,
    );
    let encoded = encoder()
        .encode_solutions(vec![solution])
        .unwrap()
        .remove(0);
    (encode(encoded.swaps()), encoded.function_signature().to_owned())
}

/// The `venueData` of a single swap through `executor` (`singleSwap` carries `executor ||
/// venueData`).
fn single(executor: &str, swap: Swap) -> String {
    let (data, sig) = solve(vec![swap]);
    assert!(sig.starts_with("singleSwap("), "{sig}");
    let rest = data
        .strip_prefix(executor)
        .expect("single swap does not start with its executor");
    rest.to_owned()
}

/// `pool | tokenIn | tokenOut`, 60 bytes (`ThogExecutor._decodeData`).
#[test]
fn thog_single_swap() {
    let usdc_wmon = single(EX_THOG, hop("thog", THOG_AMM, [WMON, USDC], USDC, WMON));
    assert_eq!(usdc_wmon, format!("{THOG_AMM}{USDC}{WMON}"));
    assert_eq!(usdc_wmon.len(), 120);
    write_calldata_to_file("test_encode_thog_usdc_wmon", &usdc_wmon);
    let wmon_usdc = single(EX_THOG, hop("thog", THOG_AMM, [WMON, USDC], WMON, USDC));
    assert_eq!(wmon_usdc, format!("{THOG_AMM}{WMON}{USDC}"));
}

/// `pool | tokenIn | tokenOut`, 60 bytes (`WooFiExecutor._decodeData`); base -> base is one swap.
#[test]
fn woofi_single_swap() {
    let wmon_usdc = single(EX_WOOFI, hop("woofi", WOOFI_POOL, [WMON, USDC], WMON, USDC));
    assert_eq!(wmon_usdc, format!("{WOOFI_POOL}{WMON}{USDC}"));
    assert_eq!(wmon_usdc.len(), 120);
    write_calldata_to_file("test_encode_woofi_wmon_usdc", &wmon_usdc);
    let usdc_weth = single(EX_WOOFI, hop("woofi", WOOFI_POOL, [WETH, USDC], USDC, WETH));
    assert_eq!(usdc_weth, format!("{WOOFI_POOL}{USDC}{WETH}"));
}

/// `pool | tokenIn | tokenOut | swapXtoY`, 61 bytes (`PoeExecutor._decodeData`), X = WMON.
#[test]
fn poe_single_swap() {
    let wmon_usdc = single(EX_POE, hop("lfj_poe", POE_POOL, [WMON, USDC], WMON, USDC));
    assert_eq!(wmon_usdc, format!("{POE_POOL}{WMON}{USDC}01"));
    assert_eq!(wmon_usdc.len(), 122);
    write_calldata_to_file("test_encode_poe_wmon_usdc", &wmon_usdc);
    let usdc_wmon = single(EX_POE, hop("lfj_poe", POE_POOL, [WMON, USDC], USDC, WMON));
    assert_eq!(usdc_wmon, format!("{POE_POOL}{USDC}{WMON}00"));
    write_calldata_to_file("test_encode_poe_usdc_wmon", &usdc_wmon);
}

#[test]
fn poe_refuses_a_hop_off_the_pool() {
    let swap = hop("lfj_poe", POE_POOL, [WMON, USDC], WMON, WETH);
    let solution = Solution::new(
        alice_address(),
        alice_address(),
        b(WMON),
        b(WETH),
        BigUint::from(1000u32),
        BigUint::from(1000u32),
        BigUint::from(1u32),
        vec![swap],
    );
    assert!(encoder()
        .encode_solutions(vec![solution])
        .is_err());
}

/// Kuru -> Thog and Thog -> Kuru: the existing venue's hop is untouched, the Thog hop carries
/// its own executor and 60 data bytes.
#[test]
fn thog_sequential_with_kuru() {
    let (swaps, sig) = solve(vec![
        hop("kuru", KURU_MARKET, [WMON, USDC], WMON, USDC),
        hop("thog", THOG_AMM, [USDC, WBTC], USDC, WBTC),
    ]);
    assert_eq!(sig, SEQUENTIAL_SIG);
    assert_eq!(
        swaps,
        program(&[
            (EX_KURU, &format!("{KURU_MARKET}{WMON}{USDC}")),
            (EX_THOG, &format!("{THOG_AMM}{USDC}{WBTC}")),
        ])
    );
    let (swaps, _) = solve(vec![
        hop("thog", THOG_AMM, [USDC, WBTC], WBTC, USDC),
        hop("kuru", KURU_MARKET, [WMON, USDC], USDC, WMON),
    ]);
    assert_eq!(
        swaps,
        program(&[
            (EX_THOG, &format!("{THOG_AMM}{WBTC}{USDC}")),
            (EX_KURU, &format!("{KURU_MARKET}{USDC}{WMON}")),
        ])
    );
}

/// Hanji -> WOOFi and WOOFi -> Hanji. Funding the pool is the router's job (`fundsExpectedAddress`
/// of the WOOFi executor); the program is just the two hops.
#[test]
fn woofi_sequential_with_hanji() {
    let (swaps, sig) = solve(vec![
        hop("vm:hanji", HANJI_PROXY, [WMON, USDC], WMON, USDC),
        hop("woofi", WOOFI_POOL, [WETH, USDC], USDC, WETH),
    ]);
    assert_eq!(sig, SEQUENTIAL_SIG);
    assert_eq!(
        swaps,
        program(&[
            (EX_HANJI, &format!("{HANJI_PROXY}{WMON}{USDC}")),
            (EX_WOOFI, &format!("{WOOFI_POOL}{USDC}{WETH}")),
        ])
    );
    let (swaps, _) = solve(vec![
        hop("woofi", WOOFI_POOL, [WETH, USDC], WETH, USDC),
        hop("vm:hanji", HANJI_PROXY, [WMON, USDC], USDC, WMON),
    ]);
    assert_eq!(
        swaps,
        program(&[
            (EX_WOOFI, &format!("{WOOFI_POOL}{WETH}{USDC}")),
            (EX_HANJI, &format!("{HANJI_PROXY}{USDC}{WMON}")),
        ])
    );
}

/// Kuru -> POE (Y -> X) and POE (X -> Y) -> Hanji.
#[test]
fn poe_sequential_with_kuru_and_hanji() {
    let (swaps, sig) = solve(vec![
        hop("kuru", KURU_MARKET, [WMON, USDC], WETH, WMON),
        hop("lfj_poe", POE_POOL, [WMON, USDC], WMON, USDC),
    ]);
    assert_eq!(sig, SEQUENTIAL_SIG);
    assert_eq!(
        swaps,
        program(&[
            (EX_KURU, &format!("{KURU_MARKET}{WETH}{WMON}")),
            (EX_POE, &format!("{POE_POOL}{WMON}{USDC}01")),
        ])
    );
    let (swaps, _) = solve(vec![
        hop("lfj_poe", POE_POOL, [WMON, USDC], USDC, WMON),
        hop("vm:hanji", HANJI_PROXY, [WMON, USDC], WMON, WETH),
    ]);
    assert_eq!(
        swaps,
        program(&[
            (EX_POE, &format!("{POE_POOL}{USDC}{WMON}00")),
            (EX_HANJI, &format!("{HANJI_PROXY}{WMON}{WETH}")),
        ])
    );
}

/// The shipped Monad config holds no address for the three venues: they are supplied after the
/// owner deploys the executors, never defaulted.
#[test]
fn default_monad_config_leaves_the_pamm_executors_unset() {
    let registry = SwapEncoderRegistry::new_with_defaults(Chain::Monad).unwrap();
    for protocol in ["thog", "woofi", "lfj_poe"] {
        assert!(registry.get_encoder(protocol).is_none(), "{protocol} has a default address");
    }
}
