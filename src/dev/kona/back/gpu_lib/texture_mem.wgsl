// Texture memory on the GPU: the sparse 64 KiB pages of the CPU's texture
// store, one word per texel.
//
// A plane is a table of pages; `tex_pages` holds every plane's table, plane
// `p`'s page `g` at `p * TEX_PAGES_PER_PLANE + g`, as the slot of the page in
// `tex_pool` (a slot is 16384 words), or TEX_ABSENT where the CPU has no page
// (every word of which reads as zero). The bound texture descriptors are in
// `tex_descs` (see texture.wgsl for their layout).
//
// Group 3: the kernel's batch takes groups 0 and 1 and its pixel planes group 2.
// The including module defines TEX_PLANES and TEX_PAGES_PER_PLANE (`Source::define`).

const TEX_ABSENT: u32 = 0xffffffffu;
const TEX_PAGE_SHIFT: u32 = 14u;
const TEX_PAGE_MASK: u32 = 0x3fffu;

@group(3) @binding(0) var<storage, read> tex_descs: array<u32>;
@group(3) @binding(1) var<storage, read> tex_pages: array<u32>;
@group(3) @binding(2) var<storage, read> tex_pool: array<u32>;

// Word `at` of plane `plane`; 0 where no page is, as the CPU's `Plane::get`.
fn tex_word(plane: u32, at: u32) -> u32 {
    let page = at >> TEX_PAGE_SHIFT;
    if plane >= TEX_PLANES || page >= TEX_PAGES_PER_PLANE {
        return 0u;
    }
    let slot = tex_pages[plane * TEX_PAGES_PER_PLANE + page];
    if slot == TEX_ABSENT {
        return 0u;
    }
    return tex_pool[(slot << TEX_PAGE_SHIFT) + (at & TEX_PAGE_MASK)];
}
