//! ## Logic for interfacing between internal 'native' types and `ClickHouse`
pub mod block;
pub mod block_info;
pub(crate) mod client_info;
pub(crate) mod coerce;
pub mod convert;
pub mod error_codes;
pub mod progress;
pub mod sync;
pub(crate) mod protocol;
#[cfg(test)]
pub(crate) mod test_helpers;
pub mod types;
pub mod values;

pub use self::error_codes::{ServerError, Severity};
pub use self::protocol::{CompressionMethod, LogData, ProfileInfo};
