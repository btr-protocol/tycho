use anyhow::{ensure, Context};
use substreams::scalar::BigInt;
use tycho_substreams::snapshot::{fields, hex_field, rows};

use crate::pb::uniswap::v4::{Pool, SnapshotPool, SnapshotPools};

// The pools of a state snapshot, emitted on the block the stream starts at, so the stores that key
// events, ticks and prices by pool start from the snapshot instead of from the PoolManager's
// deployment. A row is `<pool id>:<currency0>:<currency1>:<tick>:<sqrtPriceX96>`, ids and
// addresses in hex, numbers in decimal.
tycho_substreams::snapshot_modules!(SnapshotPools, snapshot_pools);

pub fn snapshot_pools(params: &str, block: u64) -> Result<SnapshotPools, anyhow::Error> {
    let pools = rows(params, block)?
        .into_iter()
        .map(|row| {
            let [id, currency0, currency1, tick, sqrt_price] = fields(row)?;
            ensure!(
                sqrt_price.parse::<BigInt>().is_ok(),
                "sqrtPriceX96 {sqrt_price:?} is not a decimal integer"
            );
            Ok(SnapshotPool {
                pool: Some(Pool {
                    id: hex_field(id)?,
                    currency0: hex_field(currency0)?,
                    currency1: hex_field(currency1)?,
                    created_tx_hash: vec![],
                }),
                tick: tick.parse().context("tick")?,
                sqrt_price_x96: sqrt_price.to_string(),
            })
        })
        .collect::<Result<_, anyhow::Error>>()?;
    Ok(SnapshotPools { pools })
}

impl SnapshotPool {
    /// The key the pool, tick and price stores use for this pool.
    pub fn store_key(&self) -> String {
        format!(
            "pool:0x{}",
            hex::encode(
                &self
                    .pool
                    .as_ref()
                    .expect("map_snapshot emits every pool with its id")
                    .id
            )
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_snapshot_pools_only_on_start_block() {
        let params = "block=7&rows=0xaa:bb:cc:-5:79228162514264337593543950336,dd:ee:ff:12:1";
        let pools = snapshot_pools(params, 7).unwrap().pools;
        assert_eq!(pools.len(), 2);
        assert_eq!(
            (pools[0].tick, pools[0].sqrt_price_x96.as_str(), pools[0].store_key()),
            (-5, "79228162514264337593543950336", "pool:0xaa".to_string())
        );
        assert!(snapshot_pools(params, 8)
            .unwrap()
            .pools
            .is_empty());
        assert!(snapshot_pools("", 7)
            .unwrap()
            .pools
            .is_empty());
        assert!(snapshot_pools("block=7&rows=aa:bb:cc:1", 7).is_err());
        assert!(snapshot_pools("block=7&rows=aa:bb:cc:1:0x10", 7).is_err());
    }
}
