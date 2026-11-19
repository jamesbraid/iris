// X copy area (back/x/pixel.rs `copy_write`) as two dispatches, so a copy
// is correct when source and destination overlap and when the copy is refused.
//
//   gather   reads every source word into a scratch buffer, and records in a
//            status word whether any source word (or, when the write reads its
//            destination, any destination word) is wider than the target.
//   scatter  writes each destination pixel from the scratch copy, unless the
//            status word says the whole copy is refused. The CPU path decides
//            that before it writes; here the decision never leaves the GPU.
//
// Result equals the CPU's row-by-row copy because every pixel is written once
// and reads its destination before that write (see the module note).

struct P {
    a: vec4<u32>,   // plane offset (words), width, height, status slot
    b: vec4<i32>,   // sx, sy, dx, dy (X coordinates)
    c: vec4<i32>,   // i0, i1, j0, j1 (clipped extent)
    d: vec4<u32>,   // gx, plane mask, width mask, flags (1: write reads destination)
    e: vec4<u32>,   // clip count
    clips: array<vec4<i32>, 4>, // x1, y1, x2, y2
}

@group(0) @binding(0) var<storage, read_write> planes: array<u32>;
@group(0) @binding(1) var<storage, read_write> scratch: array<u32>;
@group(0) @binding(2) var<storage, read_write> status: array<atomic<u32>>;
@group(0) @binding(3) var<uniform> p: P;

fn at(x: i32, y: i32) -> u32 {
    return p.a.x + u32(i32(p.a.z) - 1 - y) * p.a.y + u32(x);
}

fn logic(op: u32, s: u32, d: u32) -> u32 {
    switch op {
        case 0u: { return 0u; }
        case 1u: { return s & d; }
        case 2u: { return s & ~d; }
        case 3u: { return s; }
        case 4u: { return ~s & d; }
        case 5u: { return d; }
        case 6u: { return s ^ d; }
        case 7u: { return s | d; }
        case 8u: { return ~(s | d); }
        case 9u: { return ~(s ^ d); }
        case 10u: { return ~d; }
        case 11u: { return s | ~d; }
        case 12u: { return ~s; }
        case 13u: { return ~s | d; }
        case 14u: { return ~(s & d); }
        default: { return 0xffffffffu; }
    }
}

@compute @workgroup_size(8, 8)
fn gather(@builtin(global_invocation_id) g: vec3<u32>) {
    let ni = u32(p.c.y - p.c.x);
    let nj = u32(p.c.w - p.c.z);
    if g.x >= ni || g.y >= nj {
        return;
    }
    let i = p.c.x + i32(g.x);
    let j = p.c.z + i32(g.y);
    let s = planes[at(p.b.x + i, p.b.y + j)];
    scratch[g.y * ni + g.x] = s;
    if s > p.d.z {
        atomicMax(&status[p.a.w], 1u);
    }
    if (p.d.w & 1u) != 0u {
        if planes[at(p.b.z + i, p.b.w + j)] > p.d.z {
            atomicMax(&status[p.a.w], 1u);
        }
    }
}

@compute @workgroup_size(8, 8)
fn scatter(@builtin(global_invocation_id) g: vec3<u32>) {
    let ni = u32(p.c.y - p.c.x);
    let nj = u32(p.c.w - p.c.z);
    if g.x >= ni || g.y >= nj {
        return;
    }
    if atomicLoad(&status[p.a.w]) != 0u {
        return;
    }
    let x = p.b.z + p.c.x + i32(g.x);
    let y = p.b.w + p.c.z + i32(g.y);
    let n = p.e.x;
    if n > 0u {
        var inside = false;
        for (var k = 0u; k < n; k++) {
            let r = p.clips[k];
            if x >= r.x && x < r.z && y >= r.y && y < r.w {
                inside = true;
            }
        }
        if !inside {
            return;
        }
    }
    let di = at(x, y);
    let old = planes[di];
    let m = p.d.y;
    planes[di] = ((logic(p.d.x, scratch[g.y * ni + g.x], old) & m) | (old & ~m)) & p.d.z;
}
