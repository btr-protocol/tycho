use anyhow::{anyhow, bail, Context};
use substreams::pb::substreams::Clock;

use crate::pb::uniswap::v3::{Pool, SnapshotPool, SnapshotPools};

/// Emits the pools of a state snapshot on the block the stream starts at, so the stores that key
/// events and ticks by pool start from the snapshot instead of from the factory's deployment.
///
/// `params` is empty for a replay from deployment. An indexer that bootstraps from a snapshot at
/// block N passes `block=<N+1>&pools=<pool>:<token0>:<token1>:<tick>,...`, addresses in hex.
#[substreams::handlers::map]
pub fn map_snapshot(params: String, clock: Clock) -> Result<SnapshotPools, anyhow::Error> {
    snapshot_pools(&params, clock.number)
}

pub(crate) fn snapshot_pools(params: &str, block: u64) -> Result<SnapshotPools, anyhow::Error> {
    if params.is_empty() {
        return Ok(SnapshotPools::default());
    }
    let (mut start, mut pools) = (None, "");
    for pair in params.split('&') {
        match pair.split_once('=') {
            Some(("block", value)) => start = Some(value.parse::<u64>().context("block")?),
            Some(("pools", value)) => pools = value,
            _ => bail!("unexpected snapshot parameter {pair:?}"),
        }
    }
    if start.ok_or_else(|| anyhow!("snapshot parameters without block"))? != block {
        return Ok(SnapshotPools::default());
    }
    let pools = pools
        .split(',')
        .filter(|pool| !pool.is_empty())
        .map(|pool| {
            let fields: Vec<&str> = pool.split(':').collect();
            let [address, token0, token1, tick] = fields[..] else {
                bail!("snapshot pool {pool:?} is not <pool>:<token0>:<token1>:<tick>");
            };
            let hex = |value: &str| hex::decode(value.trim_start_matches("0x")).context("hex");
            Ok(SnapshotPool {
                pool: Some(Pool {
                    address: hex(address)?,
                    token0: hex(token0)?,
                    token1: hex(token1)?,
                    created_tx_hash: vec![],
                }),
                tick: tick.parse().context("tick")?,
            })
        })
        .collect::<Result<_, _>>()?;
    Ok(SnapshotPools { pools })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_snapshot_pools_only_on_start_block() {
        let params = "block=7&pools=0xaa:bb:cc:-5,dd:ee:ff:12";
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
        assert!(snapshot_pools("block=7&pools=aa:bb", 7).is_err());
    }
}
