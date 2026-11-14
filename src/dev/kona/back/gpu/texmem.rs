//! The texture store's pages and the bound textures' descriptors on the GPU (group 3 of the raster
//! kernel; the layout is `gpu_lib/texture_mem.wgsl`'s).
//!
//! The CPU keeps texture memory as sparse 64 KiB pages in the mirror's `TextureStore`. Here the
//! pages that exist sit in a pool of slots behind a table of `planes * pages_per_plane` entries.
//! A sync, made before a draw that samples, copies only what changed: a page whose stamp is past
//! the store version the last sync saw is written to its slot (a new page takes a free one), and a
//! page the store no longer holds gives its slot back and its entry reads absent, so the shader
//! reads zero there as the CPU does. The pool grows by doubling, up to what the store may hold or
//! one storage binding takes, whichever is less; a store that holds more than the pool can is
//! sampled on the CPU (`fits`).
//!
//! A descriptor (`gpu_lib::pack_desc`, `TD_WORDS` words) is written once per distinct content and
//! the slot of a draw names it by word offset. The table of descriptors is not reset by a flush:
//! its contents never change while an entry is in the map, so a batch that is still to run reads
//! what its slots named.

use std::collections::HashMap;
use std::sync::Arc;

use crate::dev::kona::back::gpu_lib::{ABSENT, TD_WORDS};
use crate::dev::kona::back::store::texture::{TextureStore, PAGE_WORDS};
use crate::wgpu::Gpu;

/// Descriptors the table holds before it is emptied.
const DESCS: u32 = 256;
/// Pages the pool starts with.
const FIRST_POOL: u32 = 16;
pub const PAGE_BYTES: u64 = PAGE_WORDS as u64 * 4;

pub struct TexMem {
    gpu: Arc<Gpu>,
    pub layout: wgpu::BindGroupLayout,
    group: Option<wgpu::BindGroup>,
    descs: wgpu::Buffer,
    table: wgpu::Buffer,
    pool: wgpu::Buffer,
    payload: wgpu::Buffer,
    pay_used: u32,
    planes: u32,
    per_plane: u32,
    /// The table as the GPU has it.
    entries: Vec<u32>,
    /// Pages the pool holds now, the most it may, and the next never-used slot.
    cap: u32,
    max: u32,
    next: u32,
    free: Vec<u32>,
    /// The store version the last sync saw.
    synced: u64,
    desc_map: HashMap<[u32; TD_WORDS], u32>,
    pub pages_written: u64,
    pub syncs: u64,
}

fn entry(binding: u32) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::COMPUTE,
        ty: wgpu::BindingType::Buffer { ty: wgpu::BufferBindingType::Storage { read_only: true }, has_dynamic_offset: false, min_binding_size: None },
        count: None,
    }
}

/// Words of the payload ring: what the records that carry host data (an image) read.
pub const PAYLOAD_WORDS: u32 = 1 << 22;

const USAGE: wgpu::BufferUsages = wgpu::BufferUsages::STORAGE.union(wgpu::BufferUsages::COPY_SRC).union(wgpu::BufferUsages::COPY_DST);

impl TexMem {
    /// Texture memory for `planes` planes of `per_plane` pages, holding at most `max_pages` pages at once.
    pub fn new(gpu: Arc<Gpu>, planes: u32, per_plane: u32, max_pages: u32) -> TexMem {
        let layout = gpu.device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor { label: Some("kona texture"), entries: &[entry(0), entry(1), entry(2), entry(3)] });
        let n = (planes * per_plane) as usize;
        let table = gpu.buffer("kona texture table", n as u64 * 4, USAGE);
        gpu.write(&table, 0, bytemuck::cast_slice(&vec![ABSENT; n]));
        let cap = FIRST_POOL.min(max_pages.max(1));
        TexMem {
            layout,
            group: None,
            descs: gpu.buffer("kona texture descriptors", DESCS as u64 * TD_WORDS as u64 * 4, USAGE),
            table,
            pool: gpu.buffer("kona texture pool", cap as u64 * PAGE_BYTES, USAGE),
            payload: gpu.buffer("kona payload", PAYLOAD_WORDS as u64 * 4, USAGE),
            pay_used: 0,
            planes,
            per_plane,
            entries: vec![ABSENT; n],
            cap,
            max: max_pages,
            next: 0,
            free: Vec::new(),
            synced: 0,
            desc_map: HashMap::new(),
            pages_written: 0,
            syncs: 0,
            gpu,
        }
    }

    /// The bind group for group 3.
    pub fn group(&mut self) -> &wgpu::BindGroup {
        if self.group.is_none() {
            fn e(binding: u32, b: &wgpu::Buffer) -> wgpu::BindGroupEntry<'_> {
                wgpu::BindGroupEntry { binding, resource: b.as_entire_binding() }
            }
            self.group = Some(self.gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("kona texture"),
                layout: &self.layout,
                entries: &[e(0, &self.descs), e(1, &self.table), e(2, &self.pool), e(3, &self.payload)],
            }));
        }
        self.group.as_ref().expect("made above")
    }

    /// Whether the pool can hold every page the store holds.
    pub fn fits(&self, store: &TextureStore) -> bool {
        store.resident_pages() <= self.max as usize
    }

    /// Let the pool hold at most `pages`, as a small device would.
    #[cfg(test)]
    pub fn limit_pages(&mut self, pages: u32) {
        self.max = pages;
    }

    /// Whether the store has changed since the last sync.
    pub fn stale(&self, store: &TextureStore) -> bool {
        store.version() != self.synced
    }

    fn alloc(&mut self) -> u32 {
        if let Some(s) = self.free.pop() {
            return s;
        }
        if self.next == self.cap {
            // Grow: a new pool with the old pages copied across, after the work already submitted.
            let cap = (self.cap * 2).min(self.max).max(self.next + 1);
            let pool = self.gpu.buffer("kona texture pool", cap as u64 * PAGE_BYTES, USAGE);
            let mut enc = self.gpu.encoder("kona texture pool grow");
            enc.copy_buffer_to_buffer(&self.pool, 0, &pool, 0, self.cap as u64 * PAGE_BYTES);
            self.gpu.submit(enc);
            self.pool = pool;
            self.cap = cap;
            self.group = None;
        }
        self.next += 1;
        self.next - 1
    }

    /// Bring the GPU's copy to the store's present words. The caller has submitted every record that
    /// reads the old words (a record still open would read the new ones).
    pub fn sync(&mut self, store: &TextureStore) {
        let (mut lo, mut hi) = (usize::MAX, 0usize);
        let mut touched = |i: usize| {
            lo = lo.min(i);
            hi = hi.max(i);
        };
        // Pages the store no longer holds.
        for i in 0..self.entries.len() {
            if self.entries[i] != ABSENT && store.page((i as u32 / self.per_plane) as u16, (i as u32 % self.per_plane) as usize).is_none() {
                self.free.push(self.entries[i]);
                self.entries[i] = ABSENT;
                touched(i);
            }
        }
        // Pages that are new or changed.
        for (plane, len) in store.used() {
            if plane as u32 >= self.planes {
                continue;
            }
            for pg in 0..len.min(self.per_plane as usize) {
                let Some(page) = store.page(plane, pg) else { continue };
                let i = (plane as u32 * self.per_plane) as usize + pg;
                if self.entries[i] != ABSENT && store.page_version(plane, pg) <= self.synced {
                    continue;
                }
                if self.entries[i] == ABSENT {
                    self.entries[i] = self.alloc();
                    touched(i);
                }
                self.gpu.write(&self.pool, self.entries[i] as u64 * PAGE_BYTES, bytemuck::cast_slice(page));
                self.pages_written += 1;
            }
        }
        if lo <= hi {
            self.gpu.write(&self.table, lo as u64 * 4, bytemuck::cast_slice(&self.entries[lo..=hi]));
        }
        self.synced = store.version();
        self.syncs += 1;
    }

    /// The word offset of the descriptor `words`, written when it is new. None when the table is
    /// full: the caller submits the open batch, calls `reset_descs` and asks again.
    pub fn desc(&mut self, words: &[u32; TD_WORDS]) -> Option<u32> {
        if let Some(&o) = self.desc_map.get(words) {
            return Some(o);
        }
        let n = self.desc_map.len() as u32;
        if n == DESCS {
            return None;
        }
        let at = n * TD_WORDS as u32;
        self.gpu.write(&self.descs, at as u64 * 4, bytemuck::cast_slice(words));
        self.desc_map.insert(*words, at);
        Some(at)
    }

    /// Whether the ring has room for `n` words past what the open batch's records use.
    pub fn payload_room(&self, n: usize) -> bool {
        self.pay_used as usize + n <= PAYLOAD_WORDS as usize
    }

    /// Put `words` in the ring (the caller has made room) and say where. The write lands before the
    /// next submission and after the earlier ones, so a batch already submitted reads what it did.
    pub fn payload_put(&mut self, words: &[u32]) -> u32 {
        let at = self.pay_used;
        self.gpu.write(&self.payload, at as u64 * 4, bytemuck::cast_slice(words));
        self.pay_used += words.len() as u32;
        at
    }

    /// Start the ring over: for after a submission, which has every record that read it.
    pub fn payload_reset(&mut self) {
        self.pay_used = 0;
    }

    /// Forget the descriptors: for after a submission, which has them all.
    pub fn reset_descs(&mut self) {
        self.desc_map.clear();
    }
}
