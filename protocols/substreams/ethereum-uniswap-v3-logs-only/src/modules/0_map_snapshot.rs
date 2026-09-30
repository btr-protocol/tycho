use anyhow::Context;
use tycho_substreams::snapshot::{fields, hex_field, rows};

use crate::pb::uniswap::v3::{Pool, SnapshotPool, SnapshotPools};

// The pools of a state snapshot, emitted on the block the stream starts at, so the stores that key
// events and ticks by pool start from the snapshot instead of from the factory's deployment. A row
// is `<pool>:<token0>:<token1>:<tick>`, addresses in hex.
tycho_substreams::snapshot_modules!(SnapshotPools, snapshot_pools);

pub(crate) fn snapshot_pools(params: &str, block: u64) -> Result<SnapshotPools, anyhow::Error> {
    let pools = rows(params, block)?
        .into_iter()
        .map(|row| {
            let [address, token0, token1, tick] = fields(row)?;
            Ok(SnapshotPool {
                pool: Some(Pool {
                    address: hex_field(address)?,
                    token0: hex_field(token0)?,
                    token1: hex_field(token1)?,
                    created_tx_hash: vec![],
                }),
                tick: tick.parse().context("tick")?,
            })
        })
        .collect::<Result<_, anyhow::Error>>()?;
    Ok(SnapshotPools { pools })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_snapshot_pools_only_on_start_block() {
        let params = "block=7&rows=0xaa:bb:cc:-5,dd:ee:ff:12";
        let pools = snapshot_pools(params, 7).unwrap().pools;
        assert_eq!(pools.len(), 2);
        assert_eq!(
            (
                pools[0].tick,
                pools[0]
                    .pool
                    .as_ref()
                    .unwrap()
                    .address
                    .clone()
            ),
            (-5, vec![0xaa])
        );
        assert!(snapshot_pools(params, 8)
            .unwrap()
            .pools
            .is_empty());
        assert!(snapshot_pools("", 7)
            .unwrap()
            .pools
            .is_empty());
        assert!(snapshot_pools("block=7&rows=aa:bb", 7).is_err());
    }
}
