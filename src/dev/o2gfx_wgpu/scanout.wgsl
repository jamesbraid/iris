// The O2's scan-out: `Gbe::render` (o2gfx.rs) for one display pixel, from the
// frame's slot (o2gfx_wgpu.rs `O2Scanout::compose`) and the pool of 64 KB
// memory tiles at group 2. Out: 0x00RRGGBB.
//
// A tile is kept as RAM keeps it, one big-endian value per word. The slot
// holds the decoded registers, the colour and gamma maps, the window-ID runs
// of each row, and for the frame and overlay planes the pool slot of every
// tile in their lists (NO_TILE: not in memory, reads as 0).

@group(1) @binding(0) var<storage, read> dslot: array<u32>;
@group(2) @binding(0) var<storage, read> tiles: array<u32>;

fn ds(k: u32) -> u32 {
    return dslot[k];
}

fn tile_word(t: u32, o: u32) -> u32 {
    return tiles[t * TILE_WORDS + (o >> 2u)];
}

fn tile_byte(t: u32, o: u32) -> u32 {
    return (tile_word(t, o) >> (24u - 8u * (o & 3u))) & 0xffu;
}

fn tile_half(t: u32, o: u32) -> u32 {
    return (tile_word(t, o) >> (16u - 16u * ((o >> 1u) & 1u))) & 0xffffu;
}

// The normal planes as one byte stream, `s` bytes in, as `render`'s `fetch`.
fn fetch(s: u32) -> u32 {
    if ds(D_FB_DMA) == 0u {
        return 0u;
    }
    let lb = ds(D_LINE_BYTES);
    let line = s / lb;
    let col = s % lb;
    if line >= ds(D_FB_H) {
        return 0u;
    }
    let idx = (line / 128u) * ds(D_PER_ROW) + col / 512u;
    if idx >= ds(D_FT_N) {
        return 0u;
    }
    let t = ds(ds(D_FT_OFF) + idx);
    if t == NO_TILE {
        return 0u;
    }
    let o = (line % 128u) * 512u + col % 512u;
    let bpp = ds(D_BPP);
    if bpp == 1u {
        return tile_byte(t, o);
    }
    if bpp == 2u {
        return tile_half(t, o);
    }
    return tile_word(t, o);
}

// The overlay pixel at (x, y); 0 is clear.
fn overlay_at(x: u32, y: u32) -> u32 {
    if ds(D_OVR_DMA) == 0u || x >= ds(D_OVR_LINE) {
        return 0u;
    }
    let idx = (y / 128u) * ds(D_OVR_PER_ROW) + x / 512u;
    if idx >= ds(D_OT_N) {
        return 0u;
    }
    let t = ds(ds(D_OT_OFF) + idx);
    if t == NO_TILE {
        return 0u;
    }
    return tile_byte(t, (y % 128u) * 512u + x % 512u);
}

// The window ID at x on row y: the last run that starts at or before x.
fn did_at(y: u32, x: u32) -> u32 {
    let end = ds(D_ROWS + y + 1u);
    var d = 0u;
    for (var k = ds(D_ROWS + y); k < end; k++) {
        let r = ds(D_RUNS + k);
        if (r & 0xffffu) > x {
            break;
        }
        d = r >> 16u;
    }
    return d;
}

fn cmap_at(i: u32) -> u32 {
    return ds(D_CMAP + i % 6144u);
}

fn gmap_at(v: u32, shift: u32) -> u32 {
    return (ds(D_GMAP + (v & 0xffu)) >> shift) & 0xffu;
}

fn expand5(v: u32) -> u32 {
    return (v << 3u) | (v >> 2u);
}

fn scanout_pixel(x: u32, y: u32) -> u32 {
    let bpp = ds(D_BPP);
    var wid = ds(D_MODES);
    if ds(D_USE_DID) != 0u {
        wid = ds(D_MODES + (did_at(y, x) & 31u));
    }
    let typ = (wid >> 2u) & 7u;
    let cm = (wid >> 5u) & 0x1fu;
    let gamma = (wid & (1u << 10u)) == 0u;
    var p = fetch((y * ds(D_W) + x) * bpp);
    // A type narrower than the frame buffer reads one half of the word.
    if typ <= 4u && bpp > 1u {
        let half = bpp * 4u;
        let mask = (1u << half) - 1u;
        if (wid & 3u) == 2u {
            p = (p >> half) & mask;
        } else {
            p = p & mask;
        }
    }
    var r = p >> 24u;
    var g = (p >> 16u) & 0xffu;
    var b = (p >> 8u) & 0xffu;
    if typ == 0u {
        let c = cmap_at((cm << 8u) | (p & 0xffu));
        r = c >> 24u;
        g = (c >> 16u) & 0xffu;
        b = (c >> 8u) & 0xffu;
    } else if typ == 1u {
        let c = cmap_at(p & 0xfffu);
        r = c >> 24u;
        g = (c >> 16u) & 0xffu;
        b = (c >> 8u) & 0xffu;
    } else if typ == 2u {
        let r3 = (p >> 5u) & 7u;
        let g3 = (p >> 2u) & 7u;
        let b2 = p & 3u;
        r = (r3 << 5u) | (r3 << 2u) | (r3 >> 1u);
        g = (g3 << 5u) | (g3 << 2u) | (g3 >> 1u);
        b = b2 * 0x55u;
    } else if typ == 3u {
        r = ((p >> 8u) & 0xfu) * 0x11u;
        g = ((p >> 4u) & 0xfu) * 0x11u;
        b = (p & 0xfu) * 0x11u;
    } else if typ == 4u {
        r = expand5((p >> 10u) & 0x1fu);
        g = expand5((p >> 5u) & 0x1fu);
        b = expand5(p & 0x1fu);
    }
    if typ >= 2u && cm != 0u {
        r = cmap_at((cm << 8u) | r) >> 24u;
        g = (cmap_at((cm << 8u) | g) >> 16u) & 0xffu;
        b = (cmap_at((cm << 8u) | b) >> 8u) & 0xffu;
    }
    if gamma {
        r = gmap_at(r, 24u);
        g = gmap_at(g, 16u);
        b = gmap_at(b, 8u);
    }
    // Overlay pixels go through colour map entry 0x1100 + the pixel, and
    // always through the gamma map.
    let ov = overlay_at(x, y);
    if ov != 0u {
        let c = cmap_at(0x1100u + ov);
        r = gmap_at(c >> 24u, 24u);
        g = gmap_at((c >> 16u) & 0xffu, 16u);
        b = gmap_at((c >> 8u) & 0xffu, 8u);
    }
    if ds(D_CRS_ON) != 0u {
        let dx = i32(x) - bitcast<i32>(ds(D_CX));
        let dy = i32(y) - bitcast<i32>(ds(D_CY));
        if dx >= 0 && dx < 32 && dy >= 0 && dy < 32 {
            let word = ds(D_GLYPH + 2u * u32(dy) + u32(dx) / 16u);
            let v = (word >> (30u - 2u * (u32(dx) % 16u))) & 3u;
            if v != 0u {
                let c = ds(D_CRS_CMAP1 + v - 1u);
                r = c >> 24u;
                g = (c >> 16u) & 0xffu;
                b = (c >> 8u) & 0xffu;
            }
        }
    }
    return ((r & 0xffu) << 16u) | ((g & 0xffu) << 8u) | (b & 0xffu);
}
