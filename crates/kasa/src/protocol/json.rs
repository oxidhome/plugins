//! Lenient accessors for device JSON. Kasa replies vary per family and
//! firmware, so a wrong-typed field reads as absent instead of failing the
//! whole reply.

use serde_json::Value;

/// `o[key]` as a string.
pub fn get_str<'a>(o: &'a Value, key: &str) -> Option<&'a str> {
    o.get(key).and_then(Value::as_str)
}

/// `o[key]` as an integer (an integral float counts).
pub fn get_i64(o: &Value, key: &str) -> Option<i64> {
    let v = o.get(key)?;
    v.as_i64().or_else(|| {
        let f = v.as_f64()?;
        // Integral floats inside the i64 range only; the cast is exact there.
        #[allow(clippy::cast_possible_truncation)]
        (f.fract() == 0.0 && f.abs() < 9.0e15).then_some(f as i64)
    })
}

/// `o[key]` as a non-negative integer that fits `u32`.
pub fn get_u32(o: &Value, key: &str) -> Option<u32> {
    get_i64(o, key).and_then(|v| u32::try_from(v).ok())
}

/// `o[key]` as a number.
pub fn get_f64(o: &Value, key: &str) -> Option<f64> {
    o.get(key).and_then(Value::as_f64)
}

/// `1`/`0` (or a JSON bool) as a bool; anything else is absent.
pub fn as_bool01(v: &Value) -> Option<bool> {
    match v {
        Value::Bool(b) => Some(*b),
        Value::Number(n) => match n.as_i64() {
            Some(1) => Some(true),
            Some(0) => Some(false),
            _ => None,
        },
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn lenient_accessors() {
        let o = json!({"s":"x","i":3,"f":2.0,"g":2.5,"n":-1,"b":true});
        assert_eq!(get_str(&o, "s"), Some("x"));
        assert_eq!(get_str(&o, "i"), None);
        assert_eq!(get_i64(&o, "f"), Some(2));
        assert_eq!(get_i64(&o, "g"), None);
        assert_eq!(get_u32(&o, "n"), None);
        assert_eq!(get_f64(&o, "i"), Some(3.0));
        assert_eq!(as_bool01(&o["b"]), Some(true));
        assert_eq!(as_bool01(&json!(0)), Some(false));
        assert_eq!(as_bool01(&json!(2)), None);
        assert_eq!(as_bool01(&json!("1")), None);
    }
}
