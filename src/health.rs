//! `read5-uploader health`: is the running uploader alive? It reads the
//! status file `run` rewrites every few seconds. This is the systemd
//! watchdog's test for places with no systemd, such as a Docker HEALTHCHECK:
//! the main loop is going round and no receive thread is wedged. A stream
//! being down is not ill health: that is rtl_airband's, and it is reported
//! in the status file and the minute markers.

use serde_json::Value;

/// The status file may be this old before the uploader counts as hung: the
/// systemd unit's `WatchdogSec`, or three missed writes if that is longer.
pub fn max_age_ms(status_secs: u64) -> i64 {
    (status_secs.max(1) as i64 * 3_000).max(60_000)
}

pub fn check(status: &Value, now_ms: i64, max_age_ms: i64) -> Result<(), String> {
    let updated = status
        .get("updatedAt")
        .and_then(Value::as_str)
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|t| t.timestamp_millis())
        .ok_or("the status file has no updatedAt time")?;
    let age = now_ms - updated;
    if age > max_age_ms {
        return Err(format!("the status file was last written {} s ago; the uploader is stopped or hung", age / 1000));
    }
    if status.pointer("/rtlAirband/threadsAlive").and_then(Value::as_bool) == Some(false) {
        return Err("a receive thread has stopped going round its loop".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spool::time::iso_ms;
    use serde_json::json;

    const NOW: i64 = 1_800_000_000_000;

    #[test]
    fn a_fresh_status_is_healthy_even_with_a_stream_down() {
        let s = json!({ "state": "degraded", "updatedAt": iso_ms(NOW - 9_000),
            "rtlAirband": { "threadsAlive": true, "streams": [{ "up": false }] } });
        assert_eq!(check(&s, NOW, 60_000), Ok(()));
    }

    #[test]
    fn a_stale_status_is_not() {
        let s = json!({ "updatedAt": iso_ms(NOW - 61_000), "rtlAirband": { "threadsAlive": true } });
        assert!(check(&s, NOW, 60_000).unwrap_err().contains("61 s ago"));
    }

    #[test]
    fn a_wedged_receive_thread_is_not() {
        let s = json!({ "updatedAt": iso_ms(NOW), "rtlAirband": { "threadsAlive": false } });
        assert!(check(&s, NOW, 60_000).is_err());
    }

    #[test]
    fn a_status_without_a_time_is_not() {
        assert!(check(&json!({}), NOW, 60_000).is_err());
    }

    #[test]
    fn the_allowed_age_follows_a_slow_status_interval() {
        assert_eq!(max_age_ms(10), 60_000);
        assert_eq!(max_age_ms(30), 90_000);
    }
}
