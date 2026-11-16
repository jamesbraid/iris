// Which rows of a composed frame differ from the frame before it: the
// change signal of an export ring. `out[0]` is the first differing row
// (0xffffffff: none), `out[1]` one past the last.

struct Params {
    width: u32,
    height: u32,
    stride: u32,
    pad: u32,
}

@group(0) @binding(0) var<uniform> p: Params;
@group(0) @binding(1) var<storage, read> now: array<u32>;
@group(0) @binding(2) var<storage, read> before: array<u32>;
@group(0) @binding(3) var<storage, read_write> out: array<atomic<u32>>;

@compute @workgroup_size(8, 8)
fn main(@builtin(global_invocation_id) g: vec3<u32>) {
    if g.x >= p.width || g.y >= p.height {
        return;
    }
    let at = g.y * p.stride + g.x;
    if now[at] != before[at] {
        atomicMin(&out[0], g.y);
        atomicMax(&out[1], g.y + 1u);
    }
}
