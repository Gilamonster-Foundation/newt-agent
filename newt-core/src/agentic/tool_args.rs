//! Numeric tool arguments as models actually send them (#2672).
//!
//! Local models emit XML-style tool calls whose values arrive as strings
//! (`offset="531"`) or floats (`531.0`). `as_u64()` returns `None` for both,
//! and a reader that treats `None` as "absent" silently serves page 1. Every
//! tool that takes a count or position parses it here: an integer, a
//! non-negative integer string, or a whole-valued float is accepted; anything
//! else fails loudly, naming the field and the accepted form.

use serde_json::Value;

/// `Ok(None)` when `field` is absent or `null`; `Ok(Some(n))` for an
/// integer, an integer string, or a whole float; otherwise `Err` naming the
/// field and what was sent.
pub(crate) fn nonnegative_usize(args: &Value, field: &str) -> Result<Option<usize>, String> {
    let invalid = |value: &Value| {
        format!("`{field}` must be a non-negative integer, e.g. {field}=531; got {value}")
    };
    let too_large = || format!("`{field}` is too large for this platform");
    match args.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(n)) => {
            if let Some(i) = n.as_u64() {
                return usize::try_from(i).map(Some).map_err(|_| too_large());
            }
            match n.as_f64() {
                Some(f) if f >= 0.0 && f.fract() == 0.0 => {
                    // 2^BITS exactly: `usize::MAX as f64` rounds UP to it, so a
                    // `>` test against that would let 2^64 saturate silently.
                    if f >= 2f64.powi(usize::BITS as i32) {
                        Err(too_large())
                    } else {
                        Ok(Some(f as usize))
                    }
                }
                _ => Err(invalid(&Value::Number(n.clone()))),
            }
        }
        Some(Value::String(s)) => s
            .trim()
            .parse::<usize>()
            .map(Some)
            .map_err(|_| invalid(&Value::String(s.clone()))),
        Some(other) => Err(invalid(other)),
    }
}

#[cfg(test)]
mod tests {
    use super::nonnegative_usize;
    use serde_json::json;

    #[test]
    fn every_shape_a_model_sends_for_531_reads_as_531() {
        for args in [
            json!({"offset": 531}),
            json!({"offset": "531"}),
            json!({"offset": " 531 "}),
            json!({"offset": 531.0}),
        ] {
            assert_eq!(nonnegative_usize(&args, "offset"), Ok(Some(531)), "{args}");
        }
    }

    #[test]
    fn a_float_at_or_past_two_to_the_bits_is_too_large_not_saturated() {
        let limit = 2f64.powi(usize::BITS as i32);
        let err = nonnegative_usize(&json!({ "offset": limit }), "offset").expect_err("2^BITS");
        assert!(err.contains("too large"), "{err}");
        // The largest f64 below 2^BITS is exactly representable and accepted.
        let below = limit - 2f64.powi(usize::BITS as i32 - 53);
        assert_eq!(
            nonnegative_usize(&json!({ "offset": below }), "offset"),
            Ok(Some(below as usize))
        );
    }

    #[test]
    fn absent_and_null_mean_not_given() {
        assert_eq!(nonnegative_usize(&json!({}), "offset"), Ok(None));
        assert_eq!(
            nonnegative_usize(&json!({"offset": null}), "offset"),
            Ok(None)
        );
    }

    #[test]
    fn unusable_values_fail_loudly_naming_the_field() {
        for args in [
            json!({"limit": "abc"}),
            json!({"limit": -5}),
            json!({"limit": 2.5}),
            json!({"limit": "-5"}),
            json!({"limit": true}),
            json!({"limit": [1]}),
        ] {
            let err = nonnegative_usize(&args, "limit").expect_err(&args.to_string());
            assert!(
                err.contains("`limit`") && err.contains("limit=531"),
                "{err}"
            );
        }
    }
}
