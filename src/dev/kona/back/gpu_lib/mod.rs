//! WGSL libraries for the KONA raster kernel's texture stage, with the host
//! side that feeds them.
//!
//! Nothing here is wired into a kernel yet: these are the pieces the raster
//! kernel will include, each held to the CPU's arithmetic by a parity test
//! (`tests`).
//!
//! - `lodmath.wgsl`: `lod_log2` and `lod_exp2`, correctly rounded f32 log2 and
//!   exp2 on integer halves, for the level of detail and the detail and
//!   sharpen weight. A GPU's own are good to a few ulp, not rounded.
//! - `texture_mem.wgsl`: the texture store's sparse 64 KiB pages, one word per
//!   texel, as a page table over a pool of pages.
//! - `texture.wgsl`: `tex_sample`, `tex_sample3`, `tex_sample_enhanced` and
//!   `tex_environment`, ports of the functions of the same names in
//!   `raster::texel`, over a bound texture packed by `pack_desc`.

// Not wired into a kernel yet.
#![allow(dead_code)]

use crate::dev::kona::back::raster::texel::{Combine, EnvMode, Filter, TexDesc, TexLevel, Wrap, CURVE_NODES, MAX_LEVELS};
use crate::dev::kona::back::store::texture::{TextureStore, PAGE_WORDS};
use crate::wgpu::{library, Fragment, Source};

pub const LOD_MATH: Fragment = Fragment::new("kona_lodmath.wgsl", include_str!("lodmath.wgsl"));
pub const TEXTURE_MEM: Fragment = Fragment::new("kona_texture_mem.wgsl", include_str!("texture_mem.wgsl"));
pub const TEXTURE: Fragment = Fragment::new("kona_texture.wgsl", include_str!("texture.wgsl"));

/// Words of a packed descriptor (`TD_WORDS` in texture.wgsl).
pub const TD_WORDS: usize = 166;
const TD_LEVELS: usize = 82;
const LEVEL_WORDS: usize = 6;

/// A kernel source with every library the texture stage needs, for `body` to
/// `#include "kona_texture.wgsl"`. `planes` and `pages_per_plane` size the page table.
pub fn texture_source(label: &str, planes: u32, pages_per_plane: u32) -> Source {
    Source::new(label)
        .define("TEX_PLANES", planes)
        .define("TEX_PAGES_PER_PLANE", pages_per_plane)
        .library(library::I64)
        .library(library::WIDE)
        .library(library::F64)
        .library(library::FLOAT)
        .library(LOD_MATH)
        .library(TEXTURE_MEM)
        .library(TEXTURE)
}

fn filter_code(f: Filter) -> u32 {
    match f {
        Filter::Nearest => 0,
        Filter::Linear => 1,
        Filter::Mip { within_linear, between_linear } => 2 | (within_linear as u32) << 2 | (between_linear as u32) << 3,
    }
}

fn pack_level(out: &mut [u32], l: &TexLevel) {
    out[0] = l.plane as u32;
    out[1] = l.off;
    out[2] = l.pitch as u32 | (l.ox as u32) << 16;
    out[3] = l.oy as u32 | (l.w as u32) << 16;
    out[4] = l.h as u32 | (l.d as u32) << 16;
    out[5] = l.slice;
}

/// A bound texture as the words texture.wgsl reads (`TD_WORDS` of them).
pub fn pack_desc(d: &TexDesc) -> [u32; TD_WORDS] {
    let mut w = [0u32; TD_WORDS];
    w[0] = d.count as u32;
    w[1] = filter_code(d.min);
    w[2] = filter_code(d.mag);
    w[3] = d.wrap.iter().enumerate().fold(0, |a, (i, &x)| a | ((x == Wrap::Clamp) as u32) << i);
    w[4] = d.lod_scale[0] as u32 | (d.lod_scale[1] as u32) << 8 | (d.lod_scale[2] as u32) << 16;
    let env = match d.env {
        EnvMode::Modulate => 0,
        EnvMode::Decal => 1,
        EnvMode::Blend => 2,
        EnvMode::Replace => 3,
    };
    w[5] = d.class as u32 | env << 8 | (d.volume as u32) << 16;
    w[6..10].copy_from_slice(&d.env_colour);
    let weights: Option<&[f32; CURVE_NODES]> = match &d.combine {
        Combine::None => None,
        Combine::Detail(c) => {
            w[10] = 1;
            w[11] = c.modulate as u32 | (c.scale as u32) << 8 | (c.channels as u32) << 16;
            pack_level(&mut w[12..12 + LEVEL_WORDS], &c.image);
            Some(&c.weights)
        }
        Combine::Sharpen(c) => {
            w[10] = 2;
            w[11] = (c.channels as u32) << 16;
            Some(&c.weights)
        }
    };
    if let Some(weights) = weights {
        for (i, v) in weights.iter().enumerate() {
            w[18 + i] = v.to_bits();
        }
    }
    for (i, l) in d.levels.iter().enumerate().take(MAX_LEVELS) {
        pack_level(&mut w[TD_LEVELS + i * LEVEL_WORDS..TD_LEVELS + (i + 1) * LEVEL_WORDS], l);
    }
    w
}

/// The slot a page table holds for a page the CPU does not have.
pub const ABSENT: u32 = u32::MAX;

/// A texture store's pages in the form texture_mem.wgsl reads: a page table
/// of `planes * pages_per_plane` slots and the pool of the pages that exist.
pub struct TexMemory {
    pub pages: Vec<u32>,
    pub pool: Vec<u32>,
}

impl TexMemory {
    /// Every page the store holds in planes `0..planes`. Panics on a page past
    /// `pages_per_plane`, which the shader could not reach.
    pub fn from_store(store: &TextureStore, planes: u32, pages_per_plane: u32) -> TexMemory {
        let mut pages = vec![ABSENT; (planes * pages_per_plane) as usize];
        let mut pool = Vec::new();
        for (plane, len) in store.used() {
            if plane as u32 >= planes {
                continue;
            }
            for pg in 0..len {
                if let Some(page) = store.page(plane, pg) {
                    assert!((pg as u32) < pages_per_plane, "page {pg} of plane {plane} is past the table");
                    pages[(plane as u32 * pages_per_plane) as usize + pg] = (pool.len() / PAGE_WORDS) as u32;
                    pool.extend_from_slice(page);
                }
            }
        }
        TexMemory { pages, pool }
    }
}

#[cfg(test)]
mod tests;
