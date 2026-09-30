//! Seeding a package's stores from a state snapshot, so an indexer that bootstrapped from a
//! snapshot at block N can stream the package from N+1 without replaying earlier blocks.
//!
//! The indexer passes the snapshot as rows in the parameters of the chunk modules
//! `map_snapshot_0..map_snapshot_7`, each `block=<N+1>&rows=<row>,<row>,...` and at most 64 KiB.
//! [`snapshot_modules!`] declares them plus `map_snapshot`, which merges the chunks. A chunk emits
//! its rows on block N+1 only; empty parameters, the manifest default, emit nothing, so a replay
//! from deployment is unchanged. Each package defines its row format and the stores it seeds.

use anyhow::{anyhow, bail, Context};

/// Returns the rows of a snapshot chunk's `params` on the block they apply to, and none on any
/// other block or for empty parameters.
pub fn rows(params: &str, block: u64) -> Result<Vec<&str>, anyhow::Error> {
    if params.is_empty() {
        return Ok(vec![]);
    }
    let (mut start, mut rows) = (None, "");
    for pair in params.split('&') {
        match pair.split_once('=') {
            Some(("block", value)) => start = Some(value.parse::<u64>().context("block")?),
            Some(("rows", value)) => rows = value,
            _ => bail!("unexpected snapshot parameter {pair:?}"),
        }
    }
    if start.ok_or_else(|| anyhow!("snapshot parameters without block"))? != block {
        return Ok(vec![]);
    }
    Ok(rows
        .split(',')
        .filter(|row| !row.is_empty())
        .collect())
}

/// Splits a row into exactly `N` `:`-separated fields.
pub fn fields<const N: usize>(row: &str) -> Result<[&str; N], anyhow::Error> {
    let fields: Vec<&str> = row.split(':').collect();
    fields
        .try_into()
        .map_err(|_| anyhow!("snapshot row {row:?} does not have {N} fields"))
}

/// Decodes a hex field, with or without `0x`.
pub fn hex_field(value: &str) -> Result<Vec<u8>, anyhow::Error> {
    hex::decode(value.trim_start_matches("0x")).with_context(|| format!("hex field {value:?}"))
}

/// Declares the snapshot chunk modules `map_snapshot_0..map_snapshot_7`, which parse their
/// parameters with `$parse(params: &str, block: u64) -> Result<$ty, anyhow::Error>`, and
/// `map_snapshot`, which concatenates the chunks in order. `$ty` is the package's snapshot
/// message, named by a plain identifier in scope (the handler macro cannot parse a type path);
/// its fields must be repeated so that merging concatenates them.
#[macro_export]
macro_rules! snapshot_modules {
    ($ty:ident, $parse:ident) => {
        $crate::snapshot_modules!(@chunks $ty, $parse,
            map_snapshot_0 map_snapshot_1 map_snapshot_2 map_snapshot_3
            map_snapshot_4 map_snapshot_5 map_snapshot_6 map_snapshot_7);

        #[substreams::handlers::map]
        pub fn map_snapshot(
            c0: $ty, c1: $ty, c2: $ty, c3: $ty, c4: $ty, c5: $ty, c6: $ty, c7: $ty,
        ) -> Result<$ty, substreams::errors::Error> {
            let mut merged = c0;
            for chunk in [c1, c2, c3, c4, c5, c6, c7] {
                ::prost::Message::merge(
                    &mut merged,
                    ::prost::Message::encode_to_vec(&chunk).as_slice(),
                )?;
            }
            Ok(merged)
        }
    };
    (@chunks $ty:ident, $parse:ident, $($name:ident)*) => {
        $(
            #[substreams::handlers::map]
            pub fn $name(
                params: String,
                clock: substreams::pb::substreams::Clock,
            ) -> Result<$ty, substreams::errors::Error> {
                $parse(&params, clock.number)
            }
        )*
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_rows_only_on_start_block() {
        let params = "block=7&rows=aa:bb,cc:dd";
        assert_eq!(rows(params, 7).unwrap(), vec!["aa:bb", "cc:dd"]);
        assert!(rows(params, 8).unwrap().is_empty());
        assert!(rows("", 7).unwrap().is_empty());
        assert!(rows("block=7&rows=", 7)
            .unwrap()
            .is_empty());
        assert!(rows("rows=aa", 7).is_err());
        assert!(rows("block=7&pools=aa", 7).is_err());
    }

    #[test]
    fn test_fields() {
        assert_eq!(fields::<2>("aa:-5").unwrap(), ["aa", "-5"]);
        assert!(fields::<3>("aa:-5").is_err());
        assert_eq!(hex_field("0x0a").unwrap(), vec![10]);
    }
}
