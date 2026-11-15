use std::{collections::HashMap, str::FromStr};

use alloy::primitives::Address;
use tycho_common::{models::Chain, Bytes};

use crate::encoding::{
    errors::EncodingError,
    evm::utils::{bytes_to_address, convert_to_router_token},
    models::{EncodingContext, Swap},
    swap_encoder::SwapEncoder,
};

/// Encodes a swap through an LFJ POE pool (`PoeExecutor`).
///
/// Layout (61 bytes, packed): `pool | tokenIn | tokenOut | swapXtoY`, the pool being the component
/// id. `swapXtoY` is 1 iff `tokenIn` is the pool's token X, which is the component's first token
/// (`tokens[0]` = X, `tokens[1]` = Y). The router pays the input inside the pool's callback.
#[derive(Clone)]
pub struct LfjPoeSwapEncoder {
    executor_address: Bytes,
}

impl SwapEncoder for LfjPoeSwapEncoder {
    fn new(
        executor_address: Bytes,
        _chain: Chain,
        _config: Option<HashMap<String, String>>,
    ) -> Result<Self, EncodingError> {
        Ok(Self { executor_address })
    }

    fn encode_swap(
        &self,
        swap: &Swap,
        _encoding_context: &EncodingContext,
    ) -> Result<Vec<u8>, EncodingError> {
        let component = swap.component();
        let pool = Address::from_str(&component.id)
            .map_err(|_| EncodingError::FatalError("Invalid component id".to_owned()))?;
        let token_in = convert_to_router_token(bytes_to_address(&swap.token_in().address)?);
        let token_out = convert_to_router_token(bytes_to_address(&swap.token_out().address)?);

        let tokens = &component.tokens;
        if tokens.len() != 2 {
            return Err(EncodingError::FatalError(
                "LFJ POE component must contain exactly two tokens (X, Y)".to_string(),
            ));
        }
        let swap_x_to_y = if swap.token_in().address == tokens[0] &&
            swap.token_out().address == tokens[1]
        {
            true
        } else if swap.token_in().address == tokens[1] && swap.token_out().address == tokens[0] {
            false
        } else {
            return Err(EncodingError::InvalidInput(format!(
                "LFJ POE token pair mismatch: {} -> {} is not {} / {}",
                swap.token_in().address,
                swap.token_out().address,
                tokens[0],
                tokens[1]
            )));
        };

        let mut encoded = Vec::with_capacity(61);
        encoded.extend_from_slice(pool.as_slice());
        encoded.extend_from_slice(token_in.as_slice());
        encoded.extend_from_slice(token_out.as_slice());
        encoded.push(u8::from(swap_x_to_y));
        Ok(encoded)
    }

    fn executor_address(&self) -> &Bytes {
        &self.executor_address
    }

    fn clone_box(&self) -> Box<dyn SwapEncoder> {
        Box::new(self.clone())
    }
}

#[cfg(test)]
mod tests {
    use alloy::hex::encode;
    use num_bigint::BigUint;
    use tycho_common::models::protocol::ProtocolComponent;

    use super::*;
    use crate::encoding::models::default_token;

    const POOL: &str = "0x02A8A16613a421EabaD6861fF6d8159f6D5EDB8f";
    const WMON: &str = "0x3bd359C1119dA7Da1D913D1C4D2B7c461115433A";
    const USDC: &str = "0x754704Bc059F8C67012fEd69BC8A327a5aafb603";

    fn encode_pair(token_in: &str, token_out: &str, tokens: Vec<Bytes>) -> Result<String, String> {
        let component = ProtocolComponent {
            id: POOL.to_owned(),
            protocol_system: "lfj_poe".to_owned(),
            tokens,
            ..Default::default()
        };
        let (t_in, t_out) = (Bytes::from(token_in), Bytes::from(token_out));
        let swap = Swap::new(
            component,
            default_token(t_in.clone()),
            default_token(t_out.clone()),
            BigUint::ZERO,
        );
        let context = EncodingContext {
            router_address: Some(Bytes::zero(20)),
            group_token_in: t_in,
            group_token_out: t_out,
        };
        let encoder = LfjPoeSwapEncoder::new(Bytes::zero(20), Chain::Monad, None).unwrap();
        encoder
            .encode_swap(&swap, &context)
            .map(encode)
            .map_err(|e| e.to_string())
    }

    fn xy() -> Vec<Bytes> {
        vec![Bytes::from(WMON), Bytes::from(USDC)]
    }

    /// `PoeExecutor._decodeData`: pool `[0:20]`, tokenIn `[20:40]`, tokenOut `[40:60]`,
    /// `swapXtoY = data[60] > 0`; same bytes as `Poe.t.sol`'s `abi.encodePacked(POOL, tIn, tOut,
    /// tIn == tokenX)`.
    #[test]
    fn encodes_pool_tokens_and_direction() {
        let x_to_y = encode_pair(WMON, USDC, xy()).unwrap();
        assert_eq!(
            x_to_y,
            concat!(
                "02a8a16613a421eabad6861ff6d8159f6d5edb8f",
                "3bd359c1119da7da1d913d1c4d2b7c461115433a",
                "754704bc059f8c67012fed69bc8a327a5aafb603",
                "01",
            )
        );
        let y_to_x = encode_pair(USDC, WMON, xy()).unwrap();
        assert_eq!(
            y_to_x,
            concat!(
                "02a8a16613a421eabad6861ff6d8159f6d5edb8f",
                "754704bc059f8c67012fed69bc8a327a5aafb603",
                "3bd359c1119da7da1d913d1c4d2b7c461115433a",
                "00",
            )
        );
    }

    #[test]
    fn refuses_a_pair_off_the_pool_or_a_missing_token_list() {
        assert!(encode_pair(WMON, "0xEE8c0E9f1BFFb4Eb878d8f15f368A02a35481242", xy()).is_err());
        assert!(encode_pair(WMON, WMON, xy()).is_err());
        assert!(encode_pair(WMON, USDC, vec![]).is_err());
    }
}
