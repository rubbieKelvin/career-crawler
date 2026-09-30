use std::time::{SystemTime, UNIX_EPOCH};

/// Current time as unix epoch milliseconds, the unit used for every timestamp column.
pub fn now_ms() -> i64 {
    return SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
}
