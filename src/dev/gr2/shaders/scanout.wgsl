// GR2's scan-out: `gr2comp::compose` for one display pixel (y = 0 the top
// row), from the frame buffer and the slot the board packed
// (`scanout.rs` `Gr2Scanout::build`): cursor, then the popup and overlay
// planes and the main visual through the pixel's window ID mode word on
// the XMAP5 channel of x % 5, then the Bt457 read mask and gamma. Out:
// 0x00RRGGBB.

@group(1) @binding(0) var<storage, read> dslot: array<u32>;
// Rows are bottom-up, as the RE3 keeps them.
@group(2) @binding(0) var<storage, read> vram: array<u32>;

fn ds(k: u32) -> u32 {
    return dslot[k];
}

fn clut(phase: u32, idx: u32) -> u32 {
    return ds(S_CLUT + phase * CLUT_SIZE + idx);
}

// 3:3:2 (R 7:5, B 4:3, G 2:0) by bit replication.
fn rgb332(v: u32) -> u32 {
    let r = (((v >> 5u) & 7u) << 5u) | (((v >> 5u) & 7u) << 2u) | (((v >> 5u) & 7u) >> 1u);
    let g = ((v & 7u) << 5u) | ((v & 7u) << 2u) | ((v & 7u) >> 1u);
    let b = ((v >> 3u) & 3u) * 0x55u;
    return (r << 16u) | (g << 8u) | b;
}

// 1:2:1 (R 3, G 2:1, B 0).
fn rgb121(v: u32) -> u32 {
    let r = ((v >> 3u) & 1u) * 255u;
    let g = ((v >> 1u) & 3u) * 85u;
    let b = (v & 1u) * 255u;
    return (r << 16u) | (g << 8u) | b;
}

// One VRAM word through an XMAP5 mode word, 0x00RRGGBB.
fn pixel_rgb(p: u32, mode: u32, phase: u32) -> u32 {
    let pix_pg = (mode >> 27u) & 0x1fu;
    let pix_mode = (mode >> 24u) & 7u;
    let olayen = (mode >> 20u) & 0xfu;
    let pd_mode = (mode >> 16u) & 3u;
    let aux_pg = (mode >> 9u) & 0x1fu;
    let pou_mode = (mode >> 5u) & 3u;
    let pou_pg = mode & 0x1fu;
    let aux = (p >> 24u) & 0xfu;

    let pup = aux >> 2u;
    if pou_mode != 0u && pup != 0u {
        return clut(phase, 0x1c00u + pou_pg * 16u + pup);
    }
    let olay = aux & olayen;
    if olay != 0u {
        return clut(phase, 0x1c00u + aux_pg * 16u + olay);
    }

    let bank1 = (pix_mode & 1u) == 1u && pix_mode < 6u;
    var shift = 12u;
    if pix_mode < 2u {
        shift = 4u;
    } else if pix_mode < 4u {
        shift = 8u;
    }
    var buf = p;
    if bank1 {
        buf = p >> shift;
    }
    buf = buf & 0xfffu;

    if pd_mode == 1u {
        if pix_mode >= 6u {
            return ((p & 0xffu) << 16u) | (p & 0xff00u) | ((p >> 16u) & 0xffu);
        }
        if pix_mode >= 4u {
            let r = buf & 0xfu;
            let g = (buf >> 4u) & 0xfu;
            let b = (buf >> 8u) & 0xfu;
            return ((r * 0x11u) << 16u) | ((g * 0x11u) << 8u) | (b * 0x11u);
        }
        if pix_mode < 2u {
            return rgb121(buf & 0xfu);
        }
        return rgb332(buf & 0xffu);
    }
    if pd_mode == 3u {
        return ds(S_MISC + phase);
    }
    var idx = buf;
    if pix_mode < 2u {
        idx = pix_pg * 256u + (buf & 0xfu);
    } else if pix_mode < 4u {
        idx = pix_pg * 256u + (buf & 0xffu);
    }
    return clut(phase, idx & 0x1fffu);
}

// The window ID at x on row y: the last run that starts at or before x,
// scanning from the row's first as the CPU walks them.
fn did_at(y: u32, x: u32) -> u32 {
    let first = ds(S_ROWS + y);
    let end = ds(S_ROWS + y + 1u);
    var did = (ds(S_RUNS + first) >> 16u) & 0xffu;
    for (var k = first + 1u; k < end; k++) {
        let r = ds(S_RUNS + k);
        if (r & 0xffffu) > x {
            break;
        }
        did = (r >> 16u) & 0xffu;
    }
    return did;
}

fn scanout_pixel(x: u32, y: u32) -> u32 {
    let phase = x % 5u;
    var rgb = 0u;
    var cursor = false;
    if ds(S_CURSOR_ON) != 0u {
        let cx = i32(x) - bitcast<i32>(ds(S_CURSOR_X));
        let cy = i32(y) - bitcast<i32>(ds(S_CURSOR_Y));
        if cx >= 0 && cx < 32 && cy >= 0 && cy < 32 {
            let w = ds(S_GLYPH + u32(cy) * 2u + u32(cx) / 16u);
            let c = (w >> ((u32(cx) % 16u) * 2u)) & 3u;
            if c != 0u {
                rgb = clut(phase, 0x1c00u + c);
                cursor = true;
            }
        }
    }
    if !cursor {
        let p = vram[(FB_H - 1u - y) * FB_W + x];
        rgb = pixel_rgb(p, ds(S_MODE + phase * 32u + did_at(y, x)), phase);
    }
    let r = ds(S_DAC + ((rgb >> 16u) & 0xffu)) & 0xffu;
    let g = ds(S_DAC + 256u + ((rgb >> 8u) & 0xffu)) & 0xffu;
    let b = ds(S_DAC + 512u + (rgb & 0xffu)) & 0xffu;
    return (r << 16u) | (g << 8u) | b;
}
