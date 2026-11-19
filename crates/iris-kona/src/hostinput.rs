//! The part of iris's `hostinput` the monitor's `shot` command names.

pub fn save_png(path: &str, _px: &[u32], _w: usize, _h: usize, _crop: Option<(usize, usize, usize, usize)>) -> Result<(usize, usize), String> {
    Err(format!("{path}: no PNG writer in the engine crate"))
}
