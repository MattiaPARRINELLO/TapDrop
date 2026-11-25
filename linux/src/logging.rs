//! Verbose tracing, silent unless `TAPDROP_DEBUG` is set.
//!
//! A tap writes about thirty light readings, and a batch carries the name of every file: useful when
//! something is wrong, far too loud for a service that runs all day.

use std::sync::OnceLock;

pub fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("TAPDROP_DEBUG").is_some())
}

#[macro_export]
macro_rules! trace_debug {
    ($($arg:tt)*) => {
        if $crate::logging::enabled() {
            eprintln!($($arg)*);
        }
    };
}
