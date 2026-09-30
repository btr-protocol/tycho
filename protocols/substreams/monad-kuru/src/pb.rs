//! prost types for `proto/kuru.proto` (kuru.v1).
use tycho_substreams::prelude::Transaction;

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct Event {
    #[prost(string, tag = "1")]
    pub market: String,
    #[prost(uint64, tag = "2")]
    pub ordinal: u64,
    #[prost(message, optional, tag = "3")]
    pub tx: Option<Transaction>,
    #[prost(uint32, tag = "4")]
    pub kind: u32,
    #[prost(uint64, tag = "5")]
    pub order_id: u64,
    #[prost(uint32, tag = "6")]
    pub price: u32,
    #[prost(string, tag = "7")]
    pub size: String,
    #[prost(bool, tag = "8")]
    pub is_buy: bool,
    #[prost(string, tag = "9")]
    pub price_1e18: String,
    #[prost(string, tag = "10")]
    pub filled: String,
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct Events {
    #[prost(message, repeated, tag = "1")]
    pub events: Vec<Event>,
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct LevelDelta {
    #[prost(string, tag = "1")]
    pub market: String,
    #[prost(uint64, tag = "2")]
    pub ordinal: u64,
    #[prost(message, optional, tag = "3")]
    pub tx: Option<Transaction>,
    #[prost(bool, tag = "4")]
    pub is_buy: bool,
    #[prost(uint32, tag = "5")]
    pub price: u32,
    #[prost(string, tag = "6")]
    pub delta: String,
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct LevelDeltas {
    #[prost(message, repeated, tag = "1")]
    pub deltas: Vec<LevelDelta>,
    /// The order each event resizes, after the event, in event order.
    #[prost(message, repeated, tag = "2")]
    pub orders: Vec<OrderUpdate>,
}

/// A resting order as the order stores keep it.
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct OrderUpdate {
    #[prost(string, tag = "1")]
    pub market: String,
    #[prost(uint64, tag = "2")]
    pub ordinal: u64,
    #[prost(message, optional, tag = "3")]
    pub tx: Option<Transaction>,
    #[prost(uint64, tag = "4")]
    pub id: u64,
    #[prost(uint32, tag = "5")]
    pub price: u32,
    #[prost(bool, tag = "6")]
    pub is_buy: bool,
    /// 0 = the order left the book.
    #[prost(string, tag = "7")]
    pub size: String,
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct SnapshotMarket {
    #[prost(string, tag = "1")]
    pub market: String,
    /// `Market::encode`
    #[prost(string, tag = "2")]
    pub params: String,
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct Snapshot {
    #[prost(message, repeated, tag = "1")]
    pub markets: Vec<SnapshotMarket>,
    /// Resting orders; `ordinal` and `tx` unset.
    #[prost(message, repeated, tag = "2")]
    pub orders: Vec<OrderUpdate>,
}
