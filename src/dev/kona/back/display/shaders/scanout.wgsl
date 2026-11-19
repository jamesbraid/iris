// KONA's scan-out: `display::compose` for one pixel (y = 0 the top row), from
// the pixel planes and the slot the board packed (`scanout.rs`
// `GpuScanout::build`). The planes are the store's, rows bottom-up as it
// keeps them: colour, second colour buffer, window ID, overlay 0, overlay 1
// and popup. The window ID picks how the colour word becomes a colour; the
// overlay and popup lie over it; the glyph cursor and then the DAC cursor
// are drawn over the result. Colours in the slot are the CPU compose's
// opaque 0xAABBGGRR; the return is 0x00RRGGBB, as the driver wants.

@group(1) @binding(0) var<storage, read> dslot: array<u32>;
@group(2) @binding(0) var<storage, read> planes: array<u32>;

const BLACK: u32 = 0xff000000u;

fn ds(k: u32) -> u32 {
    return dslot[k];
}

fn plane(p: u32, at: u32) -> u32 {
    return planes[p * ds(H_W) * ds(H_H) + at];
}

// The window kind and parameters of display ID `id`.
fn win0(id: u32) -> u32 {
    return ds(S_WIN + id * 2u);
}

fn win_base(id: u32) -> u32 {
    return ds(S_WIN + id * 2u + 1u);
}

fn direct(w: u32) -> u32 {
    let b = ds(S_LUT + 512u + (w & 0xffu));
    let g = ds(S_LUT + 256u + ((w >> 8u) & 0xffu));
    let r = ds(S_LUT + ((w >> 16u) & 0xffu));
    return BLACK | (b << 16u) | (g << 8u) | r;
}

fn cmap(i: u32) -> u32 {
    return ds(S_CMAP + i);
}

// The overlay and the popup over a pixel, as `compose_layers` does.
fn layers(px_in: u32, at: u32) -> u32 {
    var px = px_in;
    let oid = OVERLAY_FIRST + ((plane(P_DID, at) >> 8u) & 0x1fu);
    let ow = win0(oid);
    if (ow & 0xffu) == K_LAYER {
        let planes_n = (ow >> 16u) & 0xffu;
        let opaque = ((ow >> 24u) & 1u) != 0u;
        let bank = (ow >> 8u) & 1u;
        let v = plane(P_OV0 + bank, at) & ((1u << planes_n) - 1u);
        if v != 0u || opaque {
            px = cmap(win_base(oid) + v);
        }
    }
    let pw = win0(POPUP);
    if (pw & 0xffu) == K_LAYER {
        let v = plane(P_POPUP, at) & ((1u << ((pw >> 16u) & 0xffu)) - 1u);
        if v != 0u {
            px = cmap(win_base(POPUP) + v);
        }
    }
    return px;
}

fn compose(x: u32, y: u32) -> u32 {
    let flags = ds(H_FLAGS);
    if (flags & F_BLANK) != 0u {
        return BLACK;
    }
    let at = (ds(H_H) - 1u - y) * ds(H_W) + x;
    let colour = plane(P_COLOUR, at);
    if (flags & F_DEFAULT) != 0u {
        return ds(S_MAP + (colour & 0xffu));
    }
    let id = (plane(P_DID, at) >> 2u) & 0x3fu;
    let w = win0(id);
    var px = BLACK;
    switch w & 0xffu {
        case K_TEXTPORT: {
            px = ds(S_MAP + (colour & 0xffu));
        }
        case K_INDEX8: {
            px = cmap(win_base(id) + ((colour >> 8u) & 0xffu));
        }
        case K_INDEX12: {
            px = cmap(win_base(id) + ((colour >> 4u) & 0xfffu));
        }
        case K_DIRECT: {
            // The buffer the window's swaps last showed.
            var word = colour;
            if ((w >> 8u) & 1u) != 0u {
                word = plane(P_BACK, at);
            }
            px = direct(word);
        }
        default: {}
    }
    if (flags & F_LAYERS) != 0u {
        px = layers(px, at);
    }
    return px;
}

// One bit of a glyph row of four words: source bits, then mask bits, each
// pair most significant bit leftmost.
fn glyph_bit(row: u32, pair: u32, col: u32) -> u32 {
    let base = S_GLYPH + row * 4u + pair * 2u;
    if col < 32u {
        return (ds(base) >> (31u - col)) & 1u;
    }
    return (ds(base + 1u) >> (63u - col)) & 1u;
}

fn scanout_pixel(x: u32, y: u32) -> u32 {
    var px = compose(x, y);
    if (ds(H_FLAGS) & F_BLANK) == 0u {
        if ds(C_ON) != 0u {
            let col = i32(x) - bitcast<i32>(ds(C_X));
            let row = i32(y) - bitcast<i32>(ds(C_Y));
            if col >= 0 && col < 64 && row >= 0 && row < 64 {
                let s = glyph_bit(u32(row), 0u, u32(col));
                let m = glyph_bit(u32(row), 1u, u32(col));
                if s == 1u && m == 0u {
                    px = ds(C_FG);
                } else if s == 0u && m == 1u {
                    px = ds(C_BG);
                } else if s == 1u && m == 1u {
                    px = ds(C_THIRD);
                }
            }
        }
        if ds(D_ON) != 0u {
            let col = i32(x) - bitcast<i32>(ds(D_X));
            let row = i32(y) - bitcast<i32>(ds(D_Y));
            if col >= 0 && col < 64 && row >= 0 && row < 64 {
                let word = ds(S_DAC + u32(row) * 4u + u32(col) / 16u);
                let v = (word >> ((u32(col) % 16u) * 2u)) & 3u;
                if v == 3u {
                    px = ds(D_FG);
                } else if v == 2u {
                    px = ds(D_BG);
                }
            }
        }
    }
    return ((px & 0xffu) << 16u) | (px & 0xff00u) | ((px >> 16u) & 0xffu);
}
