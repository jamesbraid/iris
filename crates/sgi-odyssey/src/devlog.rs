//! The names the mounted files use from iris's `devlog`.

#[derive(Clone, Copy, Debug)]
pub enum LogModule {
    Odyssey,
}

#[macro_export]
macro_rules! dlog {
    ($mod:expr, $($arg:tt)*) => {{
        let _ = $mod;
        log::debug!($($arg)*);
    }};
}

#[macro_export]
macro_rules! dlog_dev {
    ($mod:expr, $($arg:tt)*) => {{
        let _ = $mod;
    }};
}
