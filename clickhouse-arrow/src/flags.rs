use std::sync::OnceLock;

use crate::constants::*;

static DEBUG_ARROW_ON: OnceLock<bool> = OnceLock::new();

pub(crate) fn conn_read_buffer_size() -> usize {
    std::env::var(CONN_READ_BUFFER_ENV_VAR)
        .ok()
        .and_then(|e| e.parse::<usize>().ok())
        .unwrap_or(CONN_READ_BUFFER_DEFAULT)
}

pub(crate) fn conn_write_buffer_size() -> usize {
    std::env::var(CONN_WRITE_BUFFER_ENV_VAR)
        .ok()
        .and_then(|e| e.parse::<usize>().ok())
        .unwrap_or(CONN_WRITE_BUFFER_DEFAULT)
}

// Returns true when the `DEBUG_ARROW_ENV_VAR` environment variable is set to a truthy value.
// Accepted truthy values (case-insensitive): "1", "true", "yes".
pub(crate) fn debug_arrow() -> bool {
    *DEBUG_ARROW_ON.get_or_init(|| match std::env::var(DEBUG_ARROW_ENV_VAR) {
        Ok(v) => {
            let v = v.trim().to_ascii_lowercase();
            matches!(v.as_str(), "1" | "true" | "yes")
        }
        Err(_) => false,
    })
}
