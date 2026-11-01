//! Minute identifiers. Everything is UTC.

use chrono::{DateTime, NaiveDateTime, TimeZone, Utc};

pub const MINUTE_MS: i64 = 60_000;

pub fn now_ms() -> i64 {
    Utc::now().timestamp_millis()
}

pub fn floor_minute(ms: i64) -> i64 {
    ms.div_euclid(MINUTE_MS) * MINUTE_MS
}

/// `YYYYMMDDTHHMM` for the minute containing `ms`.
pub fn minute_key(ms: i64) -> String {
    Utc.timestamp_millis_opt(floor_minute(ms))
        .single()
        .map(|d| d.format("%Y%m%dT%H%M").to_string())
        .unwrap_or_else(|| "00000000T0000".into())
}

/// Inverse of [`minute_key`]: the minute's start in epoch milliseconds.
pub fn minute_start_ms(key: &str) -> Option<i64> {
    let dt = NaiveDateTime::parse_from_str(key, "%Y%m%dT%H%M").ok()?;
    Some(dt.and_utc().timestamp_millis())
}

/// ISO-8601 to the minute, for manifests: `2026-09-05T02:28Z`.
pub fn minute_iso(key: &str) -> String {
    match minute_start_ms(key) {
        Some(ms) => Utc
            .timestamp_millis_opt(ms)
            .single()
            .map(|d| d.format("%Y-%m-%dT%H:%MZ").to_string())
            .unwrap_or_default(),
        None => String::new(),
    }
}

pub fn iso_ms(ms: i64) -> String {
    let dt: DateTime<Utc> = Utc.timestamp_millis_opt(ms).single().unwrap_or_default();
    dt.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minute_roundtrip() {
        let ms = 1_788_699_827_002; // 2026-09-06 13:03:47.002Z
        let k = minute_key(ms);
        assert_eq!(k, "20260906T1303");
        assert_eq!(minute_start_ms(&k), Some(1_788_699_780_000));
        assert_eq!(minute_iso(&k), "2026-09-06T13:03Z");
    }

}
