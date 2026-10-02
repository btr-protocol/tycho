use std::{collections::HashMap, str::FromStr};

use alloy::{primitives::Address, sol_types::SolValue};
use tycho_common::{models::Chain, Bytes};

use crate::encoding::{
    errors::EncodingError,
    evm::utils::{bytes_to_address, convert_to_router_token},
    models::{EncodingContext, Swap},
    swap_encoder::SwapEncoder,
};

/// Encodes a swap through a BTR AIMM pool (`BtrAimmExecutor`).
///
/// Layout (60 bytes, packed): `tokenIn | tokenOut | pool`, the pool being the component id.
#[derive(Clone)]
pub struct BtrAimmSwapEncoder {
    executor_address: Bytes,
}

impl SwapEncoder for BtrAimmSwapEncoder {
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
        let pool = Address::from_str(&swap.component().id)
            .map_err(|_| EncodingError::FatalError("Invalid component id".to_owned()))?;
        let token_in = convert_to_router_token(bytes_to_address(&swap.token_in().address)?);
        let token_out = convert_to_router_token(bytes_to_address(&swap.token_out().address)?);

        Ok((token_in, token_out, pool).abi_encode_packed())
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

    #[test]
    fn encodes_token_in_token_out_pool_packed() {
        let component = ProtocolComponent {
            id: "0xbbbbbbb04f5b762A4CdD1d89E341e2537e3267e4".to_owned(),
            protocol_system: "btr_aimm".to_owned(),
            ..Default::default()
        };
        let token_in = Bytes::from("0x754704Bc059F8C67012fEd69BC8A327a5aafb603");
        let token_out = Bytes::from("0xEE8c0E9f1BFFb4Eb878d8f15f368A02a35481242");
        let swap = Swap::new(
            component,
            default_token(token_in.clone()),
            default_token(token_out.clone()),
            BigUint::ZERO,
        );
        let context = EncodingContext {
            router_address: Some(Bytes::zero(20)),
            group_token_in: token_in,
            group_token_out: token_out,
        };
        let encoder = BtrAimmSwapEncoder::new(Bytes::zero(20), Chain::Monad, None).unwrap();

        assert_eq!(
            encode(
                encoder
                    .encode_swap(&swap, &context)
                    .unwrap()
            ),
            concat!(
                "754704bc059f8c67012fed69bc8a327a5aafb603",
                "ee8c0e9f1bffb4eb878d8f15f368a02a35481242",
                "bbbbbbb04f5b762a4cdd1d89e341e2537e3267e4",
            )
        );
    }
}
