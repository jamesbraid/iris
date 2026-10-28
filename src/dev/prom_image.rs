// SPDX-License-Identifier: BSD-3-Clause
// Copyright (c) 2026 James Braid
//! Ported from hw/mips/sgi/prom-image.c in jamesbraid/qemu-sgi-origami.
//! Decode the supported version-six, single-segment SGI PROM container.
//! A recognized malformed container is never treated as a raw payload.

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    fn put(v: &mut [u8], off: usize, n: u64) {
        v[off..off + 8].copy_from_slice(&n.to_be_bytes());
    }
    fn container(payload: &[u8], gzip: bool) -> Vec<u8> {
        let stored = if gzip {
            let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
            enc.write_all(payload).unwrap();
            enc.finish().unwrap()
        } else {
            payload.to_vec()
        };
        let mut v = vec![0; 4096];
        put(&mut v, 0x40, 0x4a464b535743534d);
        put(&mut v, 0x48, 6);
        put(&mut v, 0x50, (4096 + stored.len()) as u64);
        v[0x7c..0x80].copy_from_slice(&1u32.to_be_bytes());
        put(&mut v, 0x90, if gzip { 0x13 } else { 0x10 });
        put(&mut v, 0x98, 4096);
        put(&mut v, 0xb0, payload.len() as u64);
        put(&mut v, 0xc0, byte_sum(payload));
        if gzip {
            put(&mut v, 0xb8, stored.len() as u64);
            put(&mut v, 0xc8, byte_sum(&stored));
        } else {
            put(&mut v, 0xd0, payload.len() as u64);
        }
        v.extend(stored);
        v
    }
    #[test]
    fn raw_stored_and_gzip() {
        assert_eq!(decode(&[1, 2], 2).unwrap(), [1, 2]);
        assert!(decode(&[1, 2], 1).is_err());
        for gzip in [false, true] {
            let v = container(&[9, 8, 7, 6], gzip);
            assert_eq!(decode(&v, 4).unwrap(), [9, 8, 7, 6]);
            assert!(decode(&v, 3).is_err());
            let mut bad = v.clone();
            bad[0xc7] ^= 1;
            assert!(decode(&bad, 4).is_err());
            assert!(decode(&v[..100], 4).is_err());
        }
    }
    #[test]
    fn rejected_headers_and_gzip_tail() {
        let original = container(&[4, 3, 2, 1], false);
        for (off, n) in [
            (0x48, 7),
            (0x50, 4096),
            (0x90, 0x14),
            (0x98, 4095),
            (0xb0, 0),
            (0xb8, 1),
            (0xc8, 1),
            (0xd0, 3),
        ] {
            let mut v = original.clone();
            put(&mut v, off, n);
            assert!(decode(&v, 4).is_err(), "field {off:x}");
        }
        let mut v = original;
        v[0x7f] = 2;
        assert!(decode(&v, 4).is_err());
        let gz = container(&[4, 3, 2, 1], true);
        for tail in [0, 1, 7, 8] {
            let mut v = gz.clone();
            v.extend(vec![0; tail]);
            let size = v.len();
            put(&mut v, 0x50, size as u64);
            assert_eq!(decode(&v, 4).is_ok(), tail <= 7);
        }
        let mut v = gz.clone();
        v.push(1);
        let size = v.len();
        put(&mut v, 0x50, size as u64);
        assert!(decode(&v, 4).is_err());
        let mut v = gz;
        v.push(0);
        let stored = (v.len() - 4096) as u64;
        put(&mut v, 0xb8, stored);
        let checksum = byte_sum(&v[4096..]);
        put(&mut v, 0xc8, checksum);
        let size = v.len();
        put(&mut v, 0x50, size as u64);
        assert!(decode(&v, 4).is_err());
    }
}

use std::io::{self, Read};

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
fn word(data: &[u8], off: usize) -> u64 {
    u64::from_be_bytes(data[off..off + 8].try_into().unwrap())
}
fn byte_sum(data: &[u8]) -> u64 {
    data.iter().map(|b| *b as u64).sum()
}

/// Accept raw payload bytes or the supported stored/gzip container subset.
/// Gzip output is bounded before decoding, and exactly one member is required.
pub fn decode(file: &[u8], payload_limit: usize) -> io::Result<Vec<u8>> {
    if file.len() < 0x48 || word(file, 0x40) != 0x4a464b535743534d {
        if file.len() > payload_limit {
            return Err(invalid("PROM image exceeds flash capacity"));
        }
        return Ok(file.to_vec());
    }
    if file.len() < 4096 {
        return Err(invalid("truncated SGI PROM container header"));
    }
    if word(file, 0x48) != 6 {
        return Err(invalid("unsupported SGI PROM container version"));
    }
    if word(file, 0x50) != file.len() as u64 {
        return Err(invalid("SGI PROM declared end does not match file size"));
    }
    if u32::from_be_bytes(file[0x7c..0x80].try_into().unwrap()) != 1 {
        return Err(invalid("unsupported SGI PROM segment count"));
    }
    if word(file, 0x98) != 4096 {
        return Err(invalid("unsupported SGI PROM payload offset"));
    }
    let flags = word(file, 0x90);
    if flags != 0x10 && flags != 0x13 {
        return Err(invalid("unsupported SGI PROM flags"));
    }
    let length = word(file, 0xb0);
    if length == 0 || length > payload_limit as u64 {
        return Err(invalid(
            "SGI PROM decoded length is zero or exceeds capacity",
        ));
    }
    let payload = if flags == 0x10 {
        if word(file, 0xb8) != 0 || word(file, 0xc8) != 0 || word(file, 0xd0) != length {
            return Err(invalid("unsupported SGI PROM uncompressed metadata"));
        }
        if length != (file.len() - 4096) as u64 {
            return Err(invalid("invalid SGI PROM stored length"));
        }
        file[4096..].to_vec()
    } else {
        let stored = word(file, 0xb8);
        if stored == 0 || stored > (file.len() - 4096) as u64 {
            return Err(invalid("invalid SGI PROM stored length"));
        }
        let end = 4096 + stored as usize;
        if file.len() - end > 7 || file[end..].iter().any(|b| *b != 0) {
            return Err(invalid("unsupported SGI PROM trailing bytes"));
        }
        if byte_sum(&file[4096..end]) != word(file, 0xc8) {
            return Err(invalid("SGI PROM stored checksum mismatch"));
        }
        let mut decoder = flate2::bufread::GzDecoder::new(&file[4096..end]);
        let mut decoded = Vec::new();
        (&mut decoder).take(length + 1).read_to_end(&mut decoded)?;
        if decoded.len() as u64 != length || !decoder.get_ref().is_empty() {
            return Err(invalid("invalid SGI PROM gzip segment or decoded length"));
        }
        decoded
    };
    if byte_sum(&payload) != word(file, 0xc0) {
        return Err(invalid("SGI PROM decoded checksum mismatch"));
    }
    Ok(payload)
}
