//! Wincode `SchemaWrite` / `SchemaRead` adapters for types that wincode does
//! not support natively.
//!
//! ## Why this module exists
//!
//! The daemon ↔ worker IPC migrated from bincode v2 to wincode.
//! Several `model::*` types embed `chrono::DateTime<Local>` fields and used to
//! ride bincode 2's `#[bincode(with_serde)]` field attribute to delegate to
//! chrono's serde impl. wincode does not delegate to serde — every IPC type
//! must implement `SchemaWrite` / `SchemaRead` directly, so this module
//! provides a hand-rolled adapter for the chrono types the facade exposes.
//!
//! ## Wire format
//!
//! `DateTimeLocalWincode` encodes a `chrono::DateTime<Local>` as `i64` Unix
//! epoch nanoseconds (UTC reference). On the receive side the i64 is
//! converted back to `DateTime<Local>` using the receiver's local timezone.
//!
//! The original `Local` timezone offset is *intentionally* dropped on the
//! wire: daemon and worker run in the same OS session and observe the same
//! `Local` reference, so the wall-clock instant survives the round-trip
//! without timezone metadata. Out-of-window values
//! (~ before 1677 or after 2262, where `timestamp_nanos_opt()` returns
//! `None`) saturate to the Unix epoch — acceptable for the file-metadata
//! fields this adapter serves.
//!
//! ## Usage
//!
//! On any IPC payload field whose type is `DateTime<Local>` or
//! `Option<DateTime<Local>>`:
//!
//! ```ignore
//! use chrono::{DateTime, Local};
//! use crate::wincode_adapters::DateTimeLocalWincode;
//!
//! #[derive(wincode::SchemaWrite, wincode::SchemaRead)]
//! struct MyPayload {
//!     #[wincode(with = "DateTimeLocalWincode")]
//!     accessed: DateTime<Local>,
//!
//!     #[wincode(with = "Option<DateTimeLocalWincode>")]
//!     deadline: Option<DateTime<Local>>,
//! }
//! ```
//!
//! The `Option<DateTimeLocalWincode>` syntax is validated by the spike
//! in `pocs/poc-ipc-bench` — wincode-derive resolves it against the
//! `SchemaWrite` / `SchemaRead` impls below whose `Src` / `Dst` is
//! `DateTime<Local>`, so a single adapter type suffices for both bare and
//! `Option`-wrapped fields.

use chrono::{DateTime, Local, TimeZone, Utc};
use core::mem::MaybeUninit;
use wincode::config::ConfigCore;
use wincode::error::{ReadResult, WriteResult};
use wincode::io::{Reader, Writer};
use wincode::{SchemaRead, SchemaWrite, TypeMeta};

/// Zero-sized adapter for `chrono::DateTime<Local>`. Implements
/// `SchemaWrite<C>` (Src = `DateTime<Local>`) and `SchemaRead<'de, C>`
/// (Dst = `DateTime<Local>`).
///
/// Encoded form: exactly an i64 (8 bytes under wincode's `FixInt +
/// LittleEndian` default configuration, which the daemon ↔ worker IPC
/// uses).
pub struct DateTimeLocalWincode;

// SAFETY: `size_of` always returns the exact 8 bytes that `write` emits
// (one i64). `zero_copy = false` because `DateTime<Local>`'s in-memory
// representation is not an i64 — readers must materialise a chrono value
// rather than transmute.
unsafe impl<C: ConfigCore> SchemaWrite<C> for DateTimeLocalWincode {
    type Src = DateTime<Local>;
    const TYPE_META: TypeMeta = TypeMeta::Static {
        size: 8,
        zero_copy: false,
    };

    fn size_of(_src: &DateTime<Local>) -> WriteResult<usize> {
        Ok(8)
    }

    fn write(writer: impl Writer, src: &DateTime<Local>) -> WriteResult<()> {
        let ns: i64 = src.timestamp_nanos_opt().unwrap_or(0);
        <i64 as SchemaWrite<C>>::write(writer, &ns)
    }
}

// SAFETY: `read` initialises `dst` only on the `Ok(())` path (after
// successfully reading the inner i64 and constructing the
// `DateTime<Local>`). On error, `dst` is left untouched.
unsafe impl<'de, C: ConfigCore> SchemaRead<'de, C> for DateTimeLocalWincode {
    type Dst = DateTime<Local>;
    const TYPE_META: TypeMeta = TypeMeta::Static {
        size: 8,
        zero_copy: false,
    };

    fn read(reader: impl Reader<'de>, dst: &mut MaybeUninit<DateTime<Local>>) -> ReadResult<()> {
        let ns: i64 = <i64 as SchemaRead<'de, C>>::get(reader)?;
        let utc: DateTime<Utc> = Utc.timestamp_nanos(ns);
        dst.write(utc.with_timezone(&Local));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wincode::config::{Configuration, PREALLOCATION_SIZE_LIMIT_DISABLED};

    fn unbounded_config() -> Configuration<true, PREALLOCATION_SIZE_LIMIT_DISABLED> {
        Configuration::new()
    }

    /// Bare `DateTime<Local>` field round-trips. A non-zero ns instant
    /// is chosen so any byte-order or sign-extension bug surfaces as a
    /// visible diff rather than a coincidental all-zero match.
    #[derive(Debug, Clone, PartialEq, Eq, wincode::SchemaWrite, wincode::SchemaRead)]
    struct BareDateTimeField {
        #[wincode(with = "DateTimeLocalWincode")]
        accessed: DateTime<Local>,
    }

    /// `Option<DateTime<Local>>` field — `#[wincode(with = "Option<...>")]`
    /// resolution against the bare adapter is the central finding
    /// that this module relies on, and it's re-verified here on the
    /// production crate so a future wincode-derive update that breaks
    /// the resolution shows up as a facade test failure.
    #[derive(Debug, Clone, PartialEq, Eq, wincode::SchemaWrite, wincode::SchemaRead)]
    struct OptionDateTimeField {
        #[wincode(with = "Option<DateTimeLocalWincode>")]
        deadline: Option<DateTime<Local>>,
    }

    #[test]
    fn datetime_local_round_trips_wincode() {
        let value = BareDateTimeField {
            accessed: Local
                .with_ymd_and_hms(2026, 5, 10, 14, 30, 45)
                .single()
                .expect("valid local time"),
        };
        let config = unbounded_config();
        let bytes = wincode::config::serialize(&value, config).expect("encode");
        // Bare DateTime field = 8 bytes (i64) flat.
        assert_eq!(bytes.len(), 8);
        let back: BareDateTimeField = wincode::config::deserialize(&bytes, config).expect("decode");
        assert_eq!(back, value);
    }

    #[test]
    fn option_datetime_local_round_trips_wincode_some_and_none() {
        let config = unbounded_config();

        let some_value = OptionDateTimeField {
            deadline: Some(
                Local
                    .with_ymd_and_hms(2030, 1, 1, 0, 0, 0)
                    .single()
                    .expect("valid local time"),
            ),
        };
        let bytes = wincode::config::serialize(&some_value, config).expect("encode some");
        let back: OptionDateTimeField =
            wincode::config::deserialize(&bytes, config).expect("decode some");
        assert_eq!(back, some_value);

        let none_value = OptionDateTimeField { deadline: None };
        let bytes = wincode::config::serialize(&none_value, config).expect("encode none");
        let back: OptionDateTimeField =
            wincode::config::deserialize(&bytes, config).expect("decode none");
        assert_eq!(back, none_value);
    }

    /// The Unix epoch (`ns = 0`) and a representative pre-epoch instant
    /// must both survive a round-trip — pre-epoch values produce
    /// negative `timestamp_nanos_opt()` so this catches any sign-bit
    /// drop during the i64 conversion.
    #[test]
    fn datetime_local_round_trips_epoch_and_pre_epoch() {
        let config = unbounded_config();
        for value in [
            BareDateTimeField {
                accessed: Utc.timestamp_nanos(0).with_timezone(&Local),
            },
            BareDateTimeField {
                accessed: Local
                    .with_ymd_and_hms(1965, 7, 4, 12, 0, 0)
                    .single()
                    .expect("valid pre-epoch local time"),
            },
        ] {
            let bytes = wincode::config::serialize(&value, config).expect("encode");
            let back: BareDateTimeField =
                wincode::config::deserialize(&bytes, config).expect("decode");
            assert_eq!(back, value);
        }
    }
}
