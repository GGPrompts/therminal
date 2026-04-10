//! Defensive deserializers for MCP tool parameters.
//!
//! Some MCP clients (notably at least one version of Claude Code's stdio
//! client) serialize numeric tool arguments as JSON strings — e.g. they send
//! `{"pane_id":"1"}` instead of `{"pane_id":1}`. The stricter
//! `#[derive(Deserialize)]` we get from serde rejects this with
//! `invalid type: string "1", expected u64`, which surfaces to the user as
//! JSON-RPC error `-32602`. See tn-ad0g.
//!
//! This module provides drop-in deserializer helpers that accept *either*
//! a native JSON number *or* a stringified number, so we no longer reject
//! well-formed-but-stringified calls. It is intentionally permissive on the
//! input side and exact on the output side:
//!
//! - integers (`u64`, `usize`) reject fractional strings and values that
//!   overflow the target type.
//! - floats (`f32`) accept any parseable decimal string.
//! - whitespace is trimmed; empty strings are rejected.
//!
//! Only the tool parameter structs are affected — result types still
//! serialize as canonical JSON numbers. The JSON Schema exposed to clients
//! is unchanged and still declares `integer` / `number`; this is a
//! *server-side compatibility shim*, not a protocol change.

use std::fmt;

use serde::de::{self, Deserializer, Unexpected, Visitor};

/// Deserialize a `u64` from either a JSON number or a stringified integer.
pub(super) fn u64_flexible<'de, D>(deserializer: D) -> Result<u64, D::Error>
where
    D: Deserializer<'de>,
{
    struct V;
    impl<'de> Visitor<'de> for V {
        type Value = u64;
        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("a non-negative integer, or a string containing one")
        }
        fn visit_u64<E: de::Error>(self, v: u64) -> Result<u64, E> {
            Ok(v)
        }
        fn visit_i64<E: de::Error>(self, v: i64) -> Result<u64, E> {
            u64::try_from(v).map_err(|_| E::invalid_value(Unexpected::Signed(v), &self))
        }
        fn visit_u128<E: de::Error>(self, v: u128) -> Result<u64, E> {
            u64::try_from(v).map_err(|_| E::custom("u128 value overflows u64"))
        }
        fn visit_i128<E: de::Error>(self, v: i128) -> Result<u64, E> {
            u64::try_from(v).map_err(|_| E::custom("i128 value does not fit in u64"))
        }
        fn visit_f64<E: de::Error>(self, v: f64) -> Result<u64, E> {
            // Accept whole-number floats (e.g. `1.0`) defensively; reject
            // anything with a fractional component.
            if v.is_finite() && v.fract() == 0.0 && v >= 0.0 && v <= (u64::MAX as f64) {
                Ok(v as u64)
            } else {
                Err(E::invalid_value(Unexpected::Float(v), &self))
            }
        }
        fn visit_str<E: de::Error>(self, v: &str) -> Result<u64, E> {
            let trimmed = v.trim();
            if trimmed.is_empty() {
                return Err(E::invalid_value(Unexpected::Str(v), &self));
            }
            trimmed
                .parse::<u64>()
                .map_err(|_| E::invalid_value(Unexpected::Str(v), &self))
        }
        fn visit_string<E: de::Error>(self, v: String) -> Result<u64, E> {
            self.visit_str(&v)
        }
    }
    deserializer.deserialize_any(V)
}

/// Deserialize an `Option<u64>` from an integer, stringified integer, or
/// null / missing.
pub(super) fn u64_opt_flexible<'de, D>(deserializer: D) -> Result<Option<u64>, D::Error>
where
    D: Deserializer<'de>,
{
    struct V;
    impl<'de> Visitor<'de> for V {
        type Value = Option<u64>;
        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("null, a non-negative integer, or a string containing one")
        }
        fn visit_none<E>(self) -> Result<Option<u64>, E> {
            Ok(None)
        }
        fn visit_unit<E>(self) -> Result<Option<u64>, E> {
            Ok(None)
        }
        fn visit_some<D2>(self, deserializer: D2) -> Result<Option<u64>, D2::Error>
        where
            D2: Deserializer<'de>,
        {
            u64_flexible(deserializer).map(Some)
        }
        // If the client serializes the whole Option as a bare value (not
        // wrapped in Some/None), serde_json hands it to us via the scalar
        // visitors below. Forward to the u64 path.
        fn visit_u64<E: de::Error>(self, v: u64) -> Result<Option<u64>, E> {
            Ok(Some(v))
        }
        fn visit_i64<E: de::Error>(self, v: i64) -> Result<Option<u64>, E> {
            u64::try_from(v)
                .map(Some)
                .map_err(|_| E::invalid_value(Unexpected::Signed(v), &self))
        }
        fn visit_f64<E: de::Error>(self, v: f64) -> Result<Option<u64>, E> {
            if v.is_finite() && v.fract() == 0.0 && v >= 0.0 && v <= (u64::MAX as f64) {
                Ok(Some(v as u64))
            } else {
                Err(E::invalid_value(Unexpected::Float(v), &self))
            }
        }
        fn visit_str<E: de::Error>(self, v: &str) -> Result<Option<u64>, E> {
            let trimmed = v.trim();
            if trimmed.is_empty() {
                return Err(E::invalid_value(Unexpected::Str(v), &self));
            }
            trimmed
                .parse::<u64>()
                .map(Some)
                .map_err(|_| E::invalid_value(Unexpected::Str(v), &self))
        }
        fn visit_string<E: de::Error>(self, v: String) -> Result<Option<u64>, E> {
            self.visit_str(&v)
        }
    }
    deserializer.deserialize_option(V)
}

/// Deserialize an `Option<usize>` from an integer, stringified integer, or
/// null / missing. Values above `usize::MAX` are rejected on platforms where
/// `usize < u64`.
pub(super) fn usize_opt_flexible<'de, D>(deserializer: D) -> Result<Option<usize>, D::Error>
where
    D: Deserializer<'de>,
{
    let opt = u64_opt_flexible(deserializer)?;
    match opt {
        None => Ok(None),
        Some(v) => usize::try_from(v)
            .map(Some)
            .map_err(|_| de::Error::custom(format!("value {v} does not fit in usize"))),
    }
}

/// Deserialize a `u64` with a default, from an integer, stringified integer,
/// or missing field. This is the `u64_flexible` analogue used for fields like
/// `timeout_ms` that have `#[serde(default = "...")]`. Combine with
/// `default = "default_fn"` on the field as usual.
pub(super) fn u64_default_flexible<'de, D>(deserializer: D) -> Result<u64, D::Error>
where
    D: Deserializer<'de>,
{
    u64_flexible(deserializer)
}

/// Deserialize an `f32` from a JSON number or a stringified decimal.
pub(super) fn f32_flexible<'de, D>(deserializer: D) -> Result<f32, D::Error>
where
    D: Deserializer<'de>,
{
    struct V;
    impl<'de> Visitor<'de> for V {
        type Value = f32;
        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("a finite number, or a string containing one")
        }
        fn visit_f64<E: de::Error>(self, v: f64) -> Result<f32, E> {
            if v.is_finite() {
                Ok(v as f32)
            } else {
                Err(E::invalid_value(Unexpected::Float(v), &self))
            }
        }
        fn visit_f32<E: de::Error>(self, v: f32) -> Result<f32, E> {
            Ok(v)
        }
        fn visit_u64<E: de::Error>(self, v: u64) -> Result<f32, E> {
            Ok(v as f32)
        }
        fn visit_i64<E: de::Error>(self, v: i64) -> Result<f32, E> {
            Ok(v as f32)
        }
        fn visit_str<E: de::Error>(self, v: &str) -> Result<f32, E> {
            let trimmed = v.trim();
            if trimmed.is_empty() {
                return Err(E::invalid_value(Unexpected::Str(v), &self));
            }
            trimmed
                .parse::<f32>()
                .map_err(|_| E::invalid_value(Unexpected::Str(v), &self))
                .and_then(|f| {
                    if f.is_finite() {
                        Ok(f)
                    } else {
                        Err(E::invalid_value(Unexpected::Str(v), &self))
                    }
                })
        }
        fn visit_string<E: de::Error>(self, v: String) -> Result<f32, E> {
            self.visit_str(&v)
        }
    }
    deserializer.deserialize_any(V)
}

/// Deserialize an `Option<f32>` from a JSON number, stringified decimal,
/// or null/missing. Mirrors `u64_opt_flexible` for the float case.
pub(super) fn f32_opt_flexible<'de, D>(deserializer: D) -> Result<Option<f32>, D::Error>
where
    D: Deserializer<'de>,
{
    struct V;
    impl<'de> Visitor<'de> for V {
        type Value = Option<f32>;
        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("a finite number, a string containing one, or null")
        }
        fn visit_none<E: de::Error>(self) -> Result<Option<f32>, E> {
            Ok(None)
        }
        fn visit_unit<E: de::Error>(self) -> Result<Option<f32>, E> {
            Ok(None)
        }
        fn visit_some<D2: Deserializer<'de>>(self, d: D2) -> Result<Option<f32>, D2::Error> {
            f32_flexible(d).map(Some)
        }
        fn visit_f64<E: de::Error>(self, v: f64) -> Result<Option<f32>, E> {
            if v.is_finite() {
                Ok(Some(v as f32))
            } else {
                Err(E::invalid_value(Unexpected::Float(v), &self))
            }
        }
        fn visit_f32<E: de::Error>(self, v: f32) -> Result<Option<f32>, E> {
            Ok(Some(v))
        }
        fn visit_u64<E: de::Error>(self, v: u64) -> Result<Option<f32>, E> {
            Ok(Some(v as f32))
        }
        fn visit_i64<E: de::Error>(self, v: i64) -> Result<Option<f32>, E> {
            Ok(Some(v as f32))
        }
        fn visit_str<E: de::Error>(self, v: &str) -> Result<Option<f32>, E> {
            let trimmed = v.trim();
            if trimmed.is_empty() {
                return Err(E::invalid_value(Unexpected::Str(v), &self));
            }
            trimmed
                .parse::<f32>()
                .map_err(|_| E::invalid_value(Unexpected::Str(v), &self))
                .and_then(|f| {
                    if f.is_finite() {
                        Ok(Some(f))
                    } else {
                        Err(E::invalid_value(Unexpected::Str(v), &self))
                    }
                })
        }
        fn visit_string<E: de::Error>(self, v: String) -> Result<Option<f32>, E> {
            self.visit_str(&v)
        }
    }
    deserializer.deserialize_any(V)
}

#[cfg(test)]
mod tests {
    use serde::Deserialize;

    #[derive(Debug, Deserialize)]
    struct U64Param {
        #[serde(deserialize_with = "super::u64_flexible")]
        value: u64,
    }

    #[derive(Debug, Deserialize)]
    struct U64OptParam {
        #[serde(default, deserialize_with = "super::u64_opt_flexible")]
        value: Option<u64>,
    }

    #[derive(Debug, Deserialize)]
    struct UsizeOptParam {
        #[serde(default, deserialize_with = "super::usize_opt_flexible")]
        value: Option<usize>,
    }

    #[derive(Debug, Deserialize)]
    struct F32Param {
        #[serde(deserialize_with = "super::f32_flexible")]
        value: f32,
    }

    #[test]
    fn u64_accepts_native_integer() {
        let p: U64Param = serde_json::from_str(r#"{"value":42}"#).unwrap();
        assert_eq!(p.value, 42);
    }

    #[test]
    fn u64_accepts_stringified_integer() {
        let p: U64Param = serde_json::from_str(r#"{"value":"42"}"#).unwrap();
        assert_eq!(p.value, 42);
    }

    #[test]
    fn u64_accepts_whole_float() {
        let p: U64Param = serde_json::from_str(r#"{"value":1.0}"#).unwrap();
        assert_eq!(p.value, 1);
    }

    #[test]
    fn u64_rejects_fractional_float() {
        let err = serde_json::from_str::<U64Param>(r#"{"value":1.5}"#);
        assert!(err.is_err(), "expected error for fractional float");
    }

    #[test]
    fn u64_rejects_negative_string() {
        let err = serde_json::from_str::<U64Param>(r#"{"value":"-1"}"#);
        assert!(err.is_err());
    }

    #[test]
    fn u64_rejects_garbage_string() {
        let err = serde_json::from_str::<U64Param>(r#"{"value":"abc"}"#);
        assert!(err.is_err());
    }

    #[test]
    fn u64_rejects_empty_string() {
        let err = serde_json::from_str::<U64Param>(r#"{"value":""}"#);
        assert!(err.is_err());
    }

    #[test]
    fn u64_trims_whitespace_in_string() {
        let p: U64Param = serde_json::from_str(r#"{"value":" 42 "}"#).unwrap();
        assert_eq!(p.value, 42);
    }

    #[test]
    fn u64_opt_accepts_integer() {
        let p: U64OptParam = serde_json::from_str(r#"{"value":7}"#).unwrap();
        assert_eq!(p.value, Some(7));
    }

    #[test]
    fn u64_opt_accepts_stringified() {
        let p: U64OptParam = serde_json::from_str(r#"{"value":"7"}"#).unwrap();
        assert_eq!(p.value, Some(7));
    }

    #[test]
    fn u64_opt_accepts_null() {
        let p: U64OptParam = serde_json::from_str(r#"{"value":null}"#).unwrap();
        assert_eq!(p.value, None);
    }

    #[test]
    fn u64_opt_accepts_missing() {
        let p: U64OptParam = serde_json::from_str(r#"{}"#).unwrap();
        assert_eq!(p.value, None);
    }

    #[test]
    fn u64_opt_rejects_empty_string_present() {
        // If the client explicitly sent `""` as the value of an optional
        // numeric field, that's user error — we reject it with a clear
        // deserialize error rather than silently treating it as None.
        // Missing-field and `null` remain None (covered above).
        let err = serde_json::from_str::<U64OptParam>(r#"{"value":""}"#);
        assert!(err.is_err(), "empty string should not silently become None");
    }

    #[test]
    fn usize_opt_accepts_stringified() {
        let p: UsizeOptParam = serde_json::from_str(r#"{"value":"5"}"#).unwrap();
        assert_eq!(p.value, Some(5));
    }

    #[test]
    fn usize_opt_accepts_integer() {
        let p: UsizeOptParam = serde_json::from_str(r#"{"value":5}"#).unwrap();
        assert_eq!(p.value, Some(5));
    }

    #[test]
    fn usize_opt_missing_is_none() {
        let p: UsizeOptParam = serde_json::from_str(r#"{}"#).unwrap();
        assert_eq!(p.value, None);
    }

    #[test]
    fn f32_accepts_number() {
        let p: F32Param = serde_json::from_str(r#"{"value":50.0}"#).unwrap();
        assert!((p.value - 50.0).abs() < 1e-6);
    }

    #[test]
    fn f32_accepts_integer() {
        let p: F32Param = serde_json::from_str(r#"{"value":50}"#).unwrap();
        assert!((p.value - 50.0).abs() < 1e-6);
    }

    #[test]
    fn f32_accepts_stringified() {
        let p: F32Param = serde_json::from_str(r#"{"value":"50.5"}"#).unwrap();
        assert!((p.value - 50.5).abs() < 1e-6);
    }
}
