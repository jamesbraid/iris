//! The log names the mounted files use from iris's `devlog`.

#[derive(Clone, Copy, Debug)]
pub enum LogModule {
    Gr2,
}

/// A guest's own error (an access the board does not decode): `warn`, so a shell can log it as one.
#[macro_export]
macro_rules! dlog_guest {
    ($mod:expr, $($arg:tt)*) => {{
        let _ = $mod;
        log::warn!($($arg)*);
    }};
}

#[macro_export]
macro_rules! dlog_dev {
    ($mod:expr, $($arg:tt)*) => {{
        let _ = $mod;
        log::debug!($($arg)*);
    }};
}
