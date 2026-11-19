//! The tagged binary mirror of `toml::Value` and the helpers device
//! `save_state` and `load_state` functions build and read their tables with.

use serde::{Deserialize, Serialize};
use toml::Value;

// ---- BinValue: tagged binary mirror of toml::Value ----
//
// Postcard is non-self-describing — it cannot deserialize directly into the
// untagged `toml::Value` enum (which relies on `deserialize_any`). BinValue
// carries an explicit variant tag so postcard can round-trip it. The
// conversion to/from `toml::Value` is a single tree walk and runs in low
// milliseconds even for the largest device tables.
//
// Datetime is rare in our save_state output — encode it as an ISO-8601 string
// and reparse on the way back. If parsing fails the value falls back to a
// plain `toml::Value::String` so a malformed datetime never panics a load.

/// Tagged binary mirror of `toml::Value`. Order-preserving for tables (matches
/// `toml::Value::Table` which uses an `IndexMap` under the hood).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum BinValue {
    String(String),
    Integer(i64),
    Float(f64),
    Boolean(bool),
    Array(Vec<BinValue>),
    Table(Vec<(String, BinValue)>),
    Datetime(String),
}

impl BinValue {
    pub fn from_toml(v: &Value) -> Self {
        match v {
            Value::String(s) => BinValue::String(s.clone()),
            Value::Integer(i) => BinValue::Integer(*i),
            Value::Float(f) => BinValue::Float(*f),
            Value::Boolean(b) => BinValue::Boolean(*b),
            Value::Array(arr) => {
                BinValue::Array(arr.iter().map(BinValue::from_toml).collect())
            }
            Value::Table(tbl) => {
                let mut out = Vec::with_capacity(tbl.len());
                for (k, v) in tbl {
                    out.push((k.clone(), BinValue::from_toml(v)));
                }
                BinValue::Table(out)
            }
            Value::Datetime(dt) => BinValue::Datetime(dt.to_string()),
        }
    }

    pub fn into_toml(self) -> Value {
        match self {
            BinValue::String(s) => Value::String(s),
            BinValue::Integer(i) => Value::Integer(i),
            BinValue::Float(f) => Value::Float(f),
            BinValue::Boolean(b) => Value::Boolean(b),
            BinValue::Array(arr) => {
                Value::Array(arr.into_iter().map(BinValue::into_toml).collect())
            }
            BinValue::Table(entries) => {
                let mut tbl = toml::map::Map::new();
                for (k, v) in entries {
                    tbl.insert(k, v.into_toml());
                }
                Value::Table(tbl)
            }
            BinValue::Datetime(s) => match s.parse::<toml::value::Datetime>() {
                Ok(dt) => Value::Datetime(dt),
                Err(_) => Value::String(s),
            },
        }
    }
}

// ---- scalar hex helpers ----

/// Encode a u64 as a hex string Value (e.g. "0x000000001234abcd").
pub fn hex_u64(v: u64) -> Value { Value::String(format!("0x{:016x}", v)) }

/// Encode a u32 as a hex string Value (e.g. "0x1234abcd").
pub fn hex_u32(v: u32) -> Value { Value::String(format!("0x{:08x}", v)) }

/// Encode a u16 as a hex string Value (e.g. "0x1234").
pub fn hex_u16(v: u16) -> Value { Value::String(format!("0x{:04x}", v)) }

/// Encode a u8 as a hex string Value (e.g. "0x12").
pub fn hex_u8(v: u8)   -> Value { Value::String(format!("0x{:02x}", v)) }

// ---- TOML helpers ----

/// Build a TOML array of hex strings from a slice of u64.
pub fn u64_slice_to_toml(slice: &[u64]) -> Value {
    Value::Array(slice.iter().map(|&v| hex_u64(v)).collect())
}

/// Build a TOML array of hex strings from a slice of u32.
pub fn u32_slice_to_toml(slice: &[u32]) -> Value {
    Value::Array(slice.iter().map(|&v| hex_u32(v)).collect())
}

/// Build a TOML array of hex strings from a slice of u16.
pub fn u16_slice_to_toml(slice: &[u16]) -> Value {
    Value::Array(slice.iter().map(|&v| hex_u16(v)).collect())
}

/// Build a TOML array of hex strings from a slice of u8.
pub fn u8_slice_to_toml(slice: &[u8]) -> Value {
    Value::Array(slice.iter().map(|&v| hex_u8(v)).collect())
}

/// Parse a hex string or integer TOML value as u64.
pub fn toml_u64(v: &Value) -> Option<u64> {
    match v {
        Value::String(s) => u64::from_str_radix(s.trim_start_matches("0x"), 16).ok(),
        Value::Integer(i) => Some(*i as u64),
        _ => None,
    }
}

/// Parse a hex string or integer TOML value as u32.
pub fn toml_u32(v: &Value) -> Option<u32> {
    match v {
        Value::String(s) => u64::from_str_radix(s.trim_start_matches("0x"), 16).ok().map(|x| x as u32),
        Value::Integer(i) => Some(*i as u32),
        _ => None,
    }
}

/// Parse a hex string or integer TOML value as u16.
pub fn toml_u16(v: &Value) -> Option<u16> {
    match v {
        Value::String(s) => u64::from_str_radix(s.trim_start_matches("0x"), 16).ok().map(|x| x as u16),
        Value::Integer(i) => Some(*i as u16),
        _ => None,
    }
}

/// Parse a hex string or integer TOML value as u8.
pub fn toml_u8(v: &Value) -> Option<u8> {
    match v {
        Value::String(s) => u64::from_str_radix(s.trim_start_matches("0x"), 16).ok().map(|x| x as u8),
        Value::Integer(i) => Some(*i as u8),
        _ => None,
    }
}

/// Extract a bool from a TOML Value::Boolean.
pub fn toml_bool(v: &Value) -> Option<bool> {
    v.as_bool()
}

/// Load a u64 slice from a TOML array, filling as many entries as available.
pub fn load_u64_slice(v: &Value, dst: &mut [u64]) {
    if let Value::Array(arr) = v {
        for (i, item) in arr.iter().enumerate() {
            if i >= dst.len() { break; }
            if let Some(x) = toml_u64(item) { dst[i] = x; }
        }
    }
}

/// Load a u32 slice from a TOML array.
pub fn load_u32_slice(v: &Value, dst: &mut [u32]) {
    if let Value::Array(arr) = v {
        for (i, item) in arr.iter().enumerate() {
            if i >= dst.len() { break; }
            if let Some(x) = toml_u32(item) { dst[i] = x; }
        }
    }
}

/// Load a u16 slice from a TOML array.
pub fn load_u16_slice(v: &Value, dst: &mut [u16]) {
    if let Value::Array(arr) = v {
        for (i, item) in arr.iter().enumerate() {
            if i >= dst.len() { break; }
            if let Some(x) = toml_u16(item) { dst[i] = x; }
        }
    }
}

/// Load a u8 slice from a TOML array.
pub fn load_u8_slice(v: &Value, dst: &mut [u8]) {
    if let Value::Array(arr) = v {
        for (i, item) in arr.iter().enumerate() {
            if i >= dst.len() { break; }
            if let Some(x) = toml_u8(item) { dst[i] = x; }
        }
    }
}

/// Get a field from a TOML table by key.
pub fn get_field<'a>(table: &'a Value, key: &str) -> Option<&'a Value> {
    table.as_table()?.get(key)
}

// ---- required fields ----
//
// For state with no sensible default: a snapshot missing one of these is
// refused with the field's name, not loaded with a zero in its place.

pub fn field<'a>(table: &'a Value, key: &str) -> Result<&'a Value, String> {
    get_field(table, key).ok_or_else(|| format!("missing {key}"))
}

pub fn field_u64(table: &Value, key: &str) -> Result<u64, String> {
    toml_u64(field(table, key)?).ok_or_else(|| format!("{key}: not a number"))
}

/// A count stored as a number, refused above `limit`: the size of whatever
/// array it indexes.
pub fn usize_field(v: &Value, key: &str, limit: usize) -> Result<usize, String> {
    let n = field_u64(v, key)? as usize;
    if n > limit {
        return Err(format!("{key}: {n} is more than {limit}"));
    }
    Ok(n)
}

pub fn field_u32(table: &Value, key: &str) -> Result<u32, String> {
    toml_u32(field(table, key)?).ok_or_else(|| format!("{key}: not a 32-bit number"))
}

pub fn field_u16(table: &Value, key: &str) -> Result<u16, String> {
    toml_u16(field(table, key)?).ok_or_else(|| format!("{key}: not a 16-bit number"))
}

pub fn field_u8(table: &Value, key: &str) -> Result<u8, String> {
    toml_u8(field(table, key)?).ok_or_else(|| format!("{key}: not a byte"))
}

pub fn field_bool(table: &Value, key: &str) -> Result<bool, String> {
    toml_bool(field(table, key)?).ok_or_else(|| format!("{key}: not a boolean"))
}

/// A list of exactly `N` numbers, each converted by `get`.
fn field_list<T: Copy + Default, const N: usize>(
    table: &Value, key: &str, get: fn(&Value) -> Option<T>,
) -> Result<[T; N], String> {
    let items = field(table, key)?.as_array().ok_or_else(|| format!("{key}: not a list"))?;
    if items.len() != N {
        return Err(format!("{key}: {} entries, expected {N}", items.len()));
    }
    let mut out = [T::default(); N];
    for (o, item) in out.iter_mut().zip(items) {
        *o = get(item).ok_or_else(|| format!("{key}: an entry is out of range"))?;
    }
    Ok(out)
}

pub fn field_u64s<const N: usize>(table: &Value, key: &str) -> Result<[u64; N], String> {
    field_list(table, key, toml_u64)
}

pub fn field_u32s<const N: usize>(table: &Value, key: &str) -> Result<[u32; N], String> {
    field_list(table, key, toml_u32)
}

pub fn field_u8s<const N: usize>(table: &Value, key: &str) -> Result<[u8; N], String> {
    field_list(table, key, toml_u8)
}

/// A byte list of any length.
pub fn field_bytes(table: &Value, key: &str) -> Result<Vec<u8>, String> {
    field(table, key)?.as_array().ok_or_else(|| format!("{key}: not a list"))?
        .iter().map(|v| toml_u8(v).ok_or_else(|| format!("{key}: an entry is not a byte"))).collect()
}

/// A list of 64-bit numbers of any length.
pub fn field_u64_vec(table: &Value, key: &str) -> Result<Vec<u64>, String> {
    field(table, key)?.as_array().ok_or_else(|| format!("{key}: not a list"))?
        .iter().map(|v| toml_u64(v).ok_or_else(|| format!("{key}: an entry is not a number"))).collect()
}

/// A table of state, as save_state functions build them.
#[derive(Default)]
pub struct StateTable(toml::map::Map<String, Value>);

impl StateTable {
    pub fn new() -> Self { Self::default() }
    pub fn u64(&mut self, key: &str, v: u64) -> &mut Self { self.0.insert(key.into(), hex_u64(v)); self }
    pub fn u32(&mut self, key: &str, v: u32) -> &mut Self { self.0.insert(key.into(), hex_u32(v)); self }
    pub fn u16(&mut self, key: &str, v: u16) -> &mut Self { self.0.insert(key.into(), hex_u16(v)); self }
    pub fn u8(&mut self, key: &str, v: u8) -> &mut Self { self.0.insert(key.into(), hex_u8(v)); self }
    pub fn bool(&mut self, key: &str, v: bool) -> &mut Self { self.0.insert(key.into(), Value::Boolean(v)); self }
    pub fn u64s(&mut self, key: &str, v: &[u64]) -> &mut Self { self.0.insert(key.into(), u64_slice_to_toml(v)); self }
    pub fn u32s(&mut self, key: &str, v: &[u32]) -> &mut Self { self.0.insert(key.into(), u32_slice_to_toml(v)); self }
    pub fn u16s(&mut self, key: &str, v: &[u16]) -> &mut Self { self.0.insert(key.into(), u16_slice_to_toml(v)); self }
    pub fn u8s(&mut self, key: &str, v: &[u8]) -> &mut Self { self.0.insert(key.into(), u8_slice_to_toml(v)); self }
    pub fn value(&mut self, key: &str, v: Value) -> &mut Self { self.0.insert(key.into(), v); self }
    pub fn build(&mut self) -> Value { Value::Table(std::mem::take(&mut self.0)) }
}
