//! Ported from include/hw/misc/sgi-odyssey.h in jamesbraid/qemu-sgi-origami.
//!
//! The Odyssey (VPro) board's constants and state. The QOM class and the
//! native-Mesa fields are gone; the interrupt packet path is the `irq` field
//! of the host interface (`OdysseyFront`, front.rs).
use super::*;

/// Distinct unimplemented offsets reported before the device falls silent
/// about them. Diagnostic bookkeeping, not guest state.
pub const SGI_ODYSSEY_UNIMP_REPORTS: u32 = 64;

/// The vertices an integer GL primitive holds while it streams: a QUADS
/// rectangle's four, or a strip's first and previous vertex.
pub const SGI_ODYSSEY_GL_HELD_VERTICES: u32 = 4;

/// The operands a register-select word's gather keeps: the arity field's
/// largest count, a 16-word burst.
pub const SGI_ODYSSEY_CMD_OPERANDS: u32 = 16;

/// The window-clip register's four rectangles, two words each.
pub const SGI_ODYSSEY_CLIP_RECTS: u32 = 4;

/// Geometry engine program state: constant-memory entries an 11-bit index
/// reaches.
pub const SGI_ODYSSEY_GL_CONSTANTS: u32 = 2048;
/// Off-screen board-memory objects the model keeps (texture levels).
pub const SGI_ODYSSEY_OBJECTS: u32 = 1024;
pub const SGI_ODYSSEY_OBJ_POOL_BYTES: u32 = 128 * 1024 * 1024;
pub const SGI_ODYSSEY_OBJ_POOL_WORDS: u32 = SGI_ODYSSEY_OBJ_POOL_BYTES / 4;
pub const SGI_ODYSSEY_OBJ_VALID_BYTES: u32 = SGI_ODYSSEY_OBJ_POOL_WORDS / 8;
/// The texture level table's entries (0x0e80 .. 0x0ebf).
pub const SGI_ODYSSEY_TEX_LEVELS: u32 = 64;
pub const SGI_ODYSSEY_TEX_COLOUR_TABLE_ENTRIES: u32 = 256;
pub const SGI_ODYSSEY_TEX_DETAIL_FUNCTION_BLOCKS: u32 = 16;
pub const SGI_ODYSSEY_TEX_DETAIL_FUNCTION_ENTRIES: u32 = 256;
pub const SGI_ODYSSEY_GL_ENTRIES: u32 = 4;
pub const SGI_ODYSSEY_GL_LIGHTS: u32 = 8;
pub const SGI_ODYSSEY_GL_LIGHT_COLOURS: u32 = 25;
pub const SGI_ODYSSEY_GL_PROGRAM_REPORTS: u32 = 16;
/// Program memory: 2048 instructions of six halfwords, of which the window
/// at 0x738 is the last 0xc7 (ODSY_PROG_WINDOW_BASE).
pub const SGI_ODYSSEY_GL_PROGRAM_ENTRIES: u32 = 2048;
pub const SGI_ODYSSEY_GL_WINDOW_ENTRIES: u32 = 0xc7;

/// The one modeled mode: the desktop target size.
pub const SGI_ODYSSEY_WIDTH: u32 = 1280;
pub const SGI_ODYSSEY_HEIGHT: u32 = 1024;

#[derive(Clone, Copy, Default)]
pub struct SGIOdysseyOpenVertex {
    pub word: u32,
    pub operands: [u32; 4],
    pub attribute: [[u32; 5]; 19],
    pub slot: u32,
}

/// A command-FIFO store waiting for the board's thread.
#[derive(Clone, Copy)]
pub struct OdysseyQueuedStore {
    pub value: u64,
    pub addr: u32,
    pub size: u8,
    /// The processor interface counted it: its credit is due.
    pub counted: bool,
    /// It waited outside the FIFO, so the node holds its credit as owed.
    pub owed: bool,
    /// When it was stored, for the trace.
    pub queued_at: std::time::Instant,
}

pub struct SGIOdysseyState {
    /// The crosstalk widget number (the C's parent_obj.slot).
    pub slot: u32,
    /// The host interface, under its own lock (front.rs): the registers a
    /// processor may touch while the board works, the interrupt routing
    /// and the command-FIFO queue.
    pub front: std::sync::Arc<parking_lot::Mutex<OdysseyFront>>,
    pub nic_bus: Option<crate::dev::w1::Bus>,

    // Properties.
    pub revision: u32,
    /// megabytes
    pub memory_size: u32,
    pub memory_banks: u32,
    pub cas_latency: u32,
    /// bits 2:0 of 0x23380c
    pub backend_version: u32,
    pub product: String,
    pub nic_serial: u64,
    pub nic_record: String,
    /// Whether the FIFO pages' stores are posted (x-posted-fifo).
    pub posted_fifo: bool,

    // Composed at realize from the properties above.
    /// The product's record, if nic-record unset.
    pub product_record: String,
    pub memory_config: u32,

    // Modeled registers.
    pub flctl_config: u32,
    /// The memory controller's configuration word (+0x1004), as written.
    pub memory_control: u32,
    pub memory_control_reported: bool,
    pub flctl_target: u32,
    /// The FIFO set-up registers 0x6b0-0x6b3 and 0x7b5, as last written.
    pub fifo_setup: [u32; 5],
    pub flctl_target_reported: bool,

    // The DMA-sync command's host-write base (48-bit guest physical address).
    pub host_write_addr_upper: u32,
    pub host_write_addr_lower: u32,
    // The statistics buffer address (0x401c, 0x4024) and control (0x402c).
    pub stats_addr_upper: u32,
    pub stats_addr_lower: u32,
    pub stats_control: u32,
    pub stats_reported: bool,

    // The kernel driver's context switch save address (0x4004/0x400c).
    pub ctx_shadow_addr_upper: u32,
    pub ctx_shadow_addr_lower: u32,

    pub buzz: [u32; 6],
    pub nic: u32,
    pub i2c_main_cmd: u32,
    pub i2c_opt_cmd: u32,

    /// The display back end's colour lookup RAM (0x2000 entries), one 30-bit
    /// colour per entry.
    pub lut: Vec<u32>,
    /// Back-end entries 0x2000-0x2fff as last written (0x1000 entries).
    pub backend_ctl: Vec<u32>,
    pub dispmode: [u32; 32],
    /// The overlay ids' display modes, backend entries 0x2920 + id.
    pub overlay_mode: [u32; 8],
    pub overlay_mode_reported: bool,
    /// The hardware cursor's backend entries 0x2500-0x25ff.
    pub cursor: [u32; 0x100],
    pub cursor_reported: u32,
    pub backend_index: u32,
    pub backend_count: u32,

    // Command-FIFO self-test.
    pub selftest_progress: u32,
    pub cfifo_selftest_ready: bool,

    /// Colour buffer 0's RGBA8 plane over the shared board memory.
    pub mem: SGIBoardMemory,
    /// The id sgi_board_memory_add_plane returned.
    pub colour_plane: u32,

    /// The second colour buffer (buffer 1 in the displayed-buffer sense).
    pub buffer1_mem: SGIBoardMemory,
    pub buffer1_plane: u32,

    // Buffer selection.
    pub draw_mask: u32,
    pub draw_word: u32,
    pub read_word: u32,
    pub draw_mask_stereo_reported: bool,
    pub draw_mask_high_reported: bool,
    pub drawable_word_reported: bool,

    // The board-memory objects (ODSY_OBJ_POOL_BYTES).
    pub obj_mem: SGIBoardMemory,
    pub dest_base: u32,
    pub obj_count: u32,
    pub obj_used: u32,
    pub obj_base: [u32; SGI_ODYSSEY_OBJECTS as usize],
    /// obj_base inverted, base to index + 1 (odyssey_object_find()).
    pub obj_index: std::collections::HashMap<u32, u32>,
    pub obj_width: [u32; SGI_ODYSSEY_OBJECTS as usize],
    pub obj_offset: [u32; SGI_ODYSSEY_OBJECTS as usize],
    pub obj_rows: [u32; SGI_ODYSSEY_OBJECTS as usize],
    /// When each object was last selected or grown (obj_clock's count).
    pub obj_stamp: [u64; SGI_ODYSSEY_OBJECTS as usize],
    pub obj_clock: u64,
    pub obj_mem_saved: u32,
    /// SGI_ODYSSEY_OBJ_VALID_BYTES bytes.
    pub obj_valid: Vec<u8>,
    pub obj_full_reported: bool,
    pub obj_forget_reported: bool,
    pub board_oob_reported: bool,

    // The texture state words as last pushed.
    pub tex_mode: u32,
    pub tex_sampler: [u32; 7],
    pub tex_colour_table: [[u32; SGI_ODYSSEY_TEX_COLOUR_TABLE_ENTRIES as usize]; 2],
    pub tex_colour_table_valid: [u32; 2],
    pub tex_detail_function: [u32; SGI_ODYSSEY_TEX_DETAIL_FUNCTION_ENTRIES as usize],
    pub tex_detail_function_valid: u32,
    pub tex_env: [u32; 5],
    pub tex_levels: [u32; (SGI_ODYSSEY_TEX_LEVELS * 3) as usize],
    pub tex_slice_offset: [u32; (SGI_ODYSSEY_OBJECTS + 2) as usize],
    pub tex_slice_width: [u32; (SGI_ODYSSEY_OBJECTS + 2) as usize],
    pub tex_level0: u32,
    pub tex_lod: [u32; 4],
    /// The 0x203b post-filter scale and bias words, R, G, B, A.
    pub tex_scale_bias: [u32; 8],
    /// The 0x0c3e texture offset words.
    pub tex_offset: [u32; 3],
    pub tex_coord: [u32; 2],
    pub tex_coord_r: u32,
    pub tex_coord_q: u32,
    pub tex_linear_plane: [[u32; 4]; 2],
    pub tex_linear_plane_r: [u32; 4],
    pub tex_linear_plane_q: [u32; 4],
    pub tex_linear_valid: u32,
    pub tex_rect_reported: bool,
    pub dma_seq: u64,
    /// Monotonic for this device instance.
    pub trace_sequence: u64,

    /// Tests: the next slice on the board's thread first sleeps this long.
    #[cfg(test)]
    pub test_slice_sleep_ms: u32,

    /// The per-pixel drawable-id plane, one byte per framebuffer pixel.
    pub did: Vec<u8>,
    /// The overlay pixel byte beside each id byte; same lifetime as did.
    pub overlay: Vec<u8>,
    pub gl_aux_reported: bool,
    pub aux_mask_reported: bool,
    pub aux_image_reported: bool,

    // The command-FIFO interpreter (0x110000).
    pub query_current: [[u32; 5]; 5],
    pub query_captured: [[u32; 5]; 5],
    pub query_current_valid: u32,
    pub query_captured_valid: u32,
    pub query_stage: u32,
    /// The open query's kind (word 0x4c0 + kind).
    pub query_kind: u32,
    pub query_count: u32,
    pub query_sync_index: u32,
    pub query_epoch: u64,
    pub query_captured_epoch: u64,
    pub query_base: u64,
    pub query_sync_base: u64,

    pub cmd_state: u32,
    pub cmd_tag: u32,
    pub cmd_operand: [u32; SGI_ODYSSEY_CMD_OPERANDS as usize],
    pub cmd_operand_count: u32,
    pub cmd_operand_index: u32,

    // Where the current context's window is.
    pub win_origin: [i32; 2],
    pub read_origin: [i32; 2],
    pub clip_words: [u32; (2 * SGI_ODYSSEY_CLIP_RECTS) as usize],
    /// The second operand of the last 0x840 (enable, mask and value).
    pub id_compare: u32,
    pub clip_kept: u32,
    pub clip_rect: [SGIClipRect; SGI_ODYSSEY_CLIP_RECTS as usize],
    // The window as the kernel last pushed it on its own port.
    pub win_committed_origin: [i32; 2],
    pub win_committed_clip: [u32; (2 * SGI_ODYSSEY_CLIP_RECTS) as usize],
    pub win_committed_id_compare: u32,
    pub win_prior_origin: [i32; 2],
    pub win_origin_pending: bool,
    pub win_after_tail: bool,
    pub cmd_from_kernel: bool,

    // The 2D stream's GL immediate-mode subset.
    pub gl_primitive: u32,
    pub gl_color: u32,
    pub gl_vertex_x: [u32; SGI_ODYSSEY_GL_HELD_VERTICES as usize],
    pub gl_vertex_y: [u32; SGI_ODYSSEY_GL_HELD_VERTICES as usize],
    pub gl_vertex_count: u32,
    pub line_stipple: [u32; 3],
    /// One byte per framebuffer pixel; crossing marks of an open POLYGON.
    pub poly_marks: Vec<u8>,
    pub poly_row_lo: u32,
    pub poly_row_hi: u32,
    pub poly_col_lo: u32,

    // The true-colour OpenGL path.
    pub gl_ctx: SGIGLContext,
    /// The drawable odyssey_gl_bind() last built for the shared context.
    pub gl_bind_drawable: SGIGLDrawable,
    /// Host-only: GL draws may run in bands of rows (x-raster-bands); the
    /// bands run serially here.
    pub raster_bands: bool,
    /// Host-only qtest band threshold (x-test-raster-band-pixels).
    pub raster_band_pixels: u32,
    pub gl_in_prim: bool,
    pub gl_begin_open: bool,
    pub gl_integer_prim: bool,
    pub gl_open_r: u32,
    pub gl_open_c: u32,
    pub gl_open_m3: u32,
    pub gl_open_m4: u32,
    pub gl_open_held: u32,
    pub gl_open_vertex: [SGIOdysseyOpenVertex; 3],
    pub gl_restart_type: u32,
    pub gl_restart_select: u32,
    pub gl_restart_absorb: u32,
    pub gl_restart_total: u32,
    pub gl_restart_c: u32,
    pub gl_restart_m3: u32,
    pub gl_restart_m4: u32,
    pub gl_restart_armed: bool,
    pub gl_restart_vertex: [[f32; SGI_GL_VERTEX_FLOATS as usize]; 3],
    pub gl_matrix_slot: u32,
    pub gl_matrix: [u32; 16],
    pub gl_texture_matrix: [u32; 16],
    pub gl_rect: [u32; 10],

    // Depth.
    pub gl_depth_func: u32,
    pub gl_write_mask: u32,
    pub gl_combined: [u32; 16],
    pub gl_block16: [u32; 4],
    pub gl_rect_depth_logged: bool,
    pub gl_rect_stencil_logged: bool,
    pub gl_rect_pattern_logged: bool,
    pub gl_index_mask_logged: bool,
    pub depth_mem: SGIBoardMemory,
    pub depth_plane: u32,
    pub stencil_mem: SGIBoardMemory,
    pub stencil_plane: u32,

    /// The scissor and window clip box, 0x081d, as last written.
    pub gl_clip_box: [u32; 2],
    pub gl_fog: [u32; 6],

    // Lighting: the geometry engine's program state.
    pub gl_const: [u32; SGI_ODYSSEY_GL_CONSTANTS as usize],
    pub gl_const_index: u32,
    pub gl_entry: [u32; SGI_ODYSSEY_GL_ENTRIES as usize],
    pub gl_light_colour: [u32; (SGI_ODYSSEY_GL_LIGHT_COLOURS * 4) as usize],
    pub gl_light_vec: [u32; (SGI_ODYSSEY_GL_LIGHTS * 16) as usize],
    pub gl_light_colour_sel: u32,
    pub gl_light_vec_sel: u32,
    pub gl_material: [u32; 16],
    pub gl_shininess_sqrt: u32,
    /// Raw back material retained for idle context save, not lighting.
    pub gl_back_material: [u32; 16],
    pub gl_back_shininess_sqrt: u32,
    /// The class 0x35 colour-material word's eight bits.
    pub gl_colour_material: u32,
    pub gl_normal: [u32; 3],
    pub gl_normal_matrix: [u32; 16],
    pub gl_colour: [u32; 4],
    /// The model-view (slot 0x24), row-major float bits as sent.
    pub gl_modelview: [u32; 16],
    /// The user clip planes in eye coordinates, four floats' bits a plane.
    pub gl_clip_eye: [u32; (SGI_GL_CLIP_PLANES * 4) as usize],
    pub gl_lit: bool,
    pub gl_local_viewer: bool,
    pub gl_colour_overridden: bool,
    pub gl_program_logged: [u32; (SGI_ODYSSEY_GL_PROGRAM_REPORTS * 3) as usize],
    pub gl_program_logged_count: u32,
    /// Program memory as uploaded: SGI_ODYSSEY_GL_PROGRAM_ENTRIES * 6
    /// halfwords.
    pub gl_program_mem: Vec<u16>,
    pub gl_program_addr: u32,
    /// The program-sequencer registers, classes 0x20 to 0x35, as written.
    pub gl_seq: [u32; 22],
    pub gl_program_half: u32,
    pub gl_program_mem_reported: bool,
    pub gl_two_sided_logged: bool,
    pub gl_texgen_logged: bool,
    /// Derived from the retained program list.
    pub gl_linear_texgen: u32,
    pub gl_clip_logged: bool,
    pub gl_texture_stage_logged: bool,
    pub gl_block_logged: bool,
    pub ctx_open_prim_logged: bool,
    pub ctx_shadow_fail_reported: bool,
    /// GL_DITHER, 0x0404 bit 18, as last pushed.
    pub gl_dither: bool,
    pub gl_dither_reported: bool,
    pub scanout_group25_reported: bool,

    // The current fill's colour source.
    pub gl_color_index: u32,
    pub gl_color_is_index: bool,

    /// The colour-mode words as last sent, class 0x22 then 0x23.
    pub colour_mode: [u32; 2],
    pub colour_mode_reported: bool,
    pub gl_index_lit_reported: bool,

    pub dest_buffer_word: u32,
    pub plane_write_mask: u32,

    /// The colour buffer's base: the first operand of the last colour 0x1017
    /// block (0x2000 for the screen).
    pub dma_colour_base: u32,

    // odyssey_copyrect()'s fixed literal template's three runtime slots.
    pub copy_src: u32,
    pub copy_size: u32,
    pub copy_dst: u32,

    // The one-bit pattern the greeter draws its text with.
    pub pattern_buffer: [u32; 32],
    pub pattern_shape: u32,
    pub pattern_mode: u32,
    pub pattern_load_offset: u32,
    /// 0x0c21 operands 1 and 2, the constant colour.
    pub const_colour: [u32; 2],
    /// 0x0c21 operand 0, the factors and equation, as last written.
    pub blend_word: u32,

    // The BEGIN-0xa image raster path.
    pub image_active: bool,
    pub image_x1: u32,
    pub image_y1: u32,
    pub image_x2: u32,
    pub image_y2: u32,
    pub image_cur_x: u32,
    pub image_cur_y: u32,
    pub image_fmt: u32,
    pub image_group_len: u32,
    pub image_group_painted: u32,
    pub image_code: u32,
    pub image_bytes: u32,
    pub image_nbytes: u32,
    pub image_code_reported: bool,

    // Host-only qtest introspection.
    pub test_pixel_x: u32,
    pub test_pixel_y: u32,
    pub test_pixel_buffer: u32,
    pub test_pixel_count: u32,
    pub test_object_base: u32,

    /// The shared scanout; resolves the colour plane through
    /// odyssey_head_ops.resolve_row.
    pub scanout: SGIScanout,

    // Diagnostic bookkeeping, not guest state.
    pub reported_addr: [u32; SGI_ODYSSEY_UNIMP_REPORTS as usize],
    pub reported_addr_count: u32,
    pub reported_cmd: [u32; SGI_ODYSSEY_UNIMP_REPORTS as usize],
    pub reported_cmd_count: u32,
    pub line_stipple_reported: bool,
    pub dma_bpp_reported: bool,
    pub dma_readback_reported: bool,
    pub dma_code_reported: bool,
    /// One bit per ODSY_DMA_REPORT_* reason.
    pub dma_read_reported: u32,
}

impl Default for SGIOdysseyState {
    fn default() -> Self {
        SGIOdysseyState {
            slot: 0,
            front: Default::default(),
            nic_bus: None,
            revision: 0,
            memory_size: 0,
            memory_banks: 0,
            cas_latency: 0,
            backend_version: 0,
            product: String::new(),
            nic_serial: 0,
            nic_record: String::new(),
            posted_fifo: false,
            product_record: String::new(),
            memory_config: 0,
            flctl_config: 0,
            memory_control: 0,
            memory_control_reported: false,
            flctl_target: 0,
            fifo_setup: [0; 5],
            flctl_target_reported: false,
            host_write_addr_upper: 0,
            host_write_addr_lower: 0,
            stats_addr_upper: 0,
            stats_addr_lower: 0,
            stats_control: 0,
            stats_reported: false,
            ctx_shadow_addr_upper: 0,
            ctx_shadow_addr_lower: 0,
            buzz: [0; 6],
            nic: 0,
            i2c_main_cmd: 0,
            i2c_opt_cmd: 0,
            lut: vec![0; 0x2000],
            backend_ctl: vec![0; 0x1000],
            dispmode: [0; 32],
            overlay_mode: [0; 8],
            overlay_mode_reported: false,
            cursor: [0; 0x100],
            cursor_reported: 0,
            backend_index: 0,
            backend_count: 0,
            selftest_progress: 0,
            cfifo_selftest_ready: false,
            mem: Default::default(),
            colour_plane: 0,
            buffer1_mem: Default::default(),
            buffer1_plane: 0,
            draw_mask: 0,
            draw_word: 0,
            read_word: 0,
            draw_mask_stereo_reported: false,
            draw_mask_high_reported: false,
            drawable_word_reported: false,
            obj_mem: Default::default(),
            dest_base: 0,
            obj_count: 0,
            obj_used: 0,
            obj_base: [0; SGI_ODYSSEY_OBJECTS as usize],
            obj_index: Default::default(),
            obj_width: [0; SGI_ODYSSEY_OBJECTS as usize],
            obj_offset: [0; SGI_ODYSSEY_OBJECTS as usize],
            obj_rows: [0; SGI_ODYSSEY_OBJECTS as usize],
            obj_stamp: [0; SGI_ODYSSEY_OBJECTS as usize],
            obj_clock: 0,
            obj_mem_saved: 0,
            obj_valid: vec![0; SGI_ODYSSEY_OBJ_VALID_BYTES as usize],
            obj_full_reported: false,
            obj_forget_reported: false,
            board_oob_reported: false,
            tex_mode: 0,
            tex_sampler: [0; 7],
            tex_colour_table: [[0; SGI_ODYSSEY_TEX_COLOUR_TABLE_ENTRIES as usize]; 2],
            tex_colour_table_valid: [0; 2],
            tex_detail_function: [0; SGI_ODYSSEY_TEX_DETAIL_FUNCTION_ENTRIES as usize],
            tex_detail_function_valid: 0,
            tex_env: [0; 5],
            tex_levels: [0; (SGI_ODYSSEY_TEX_LEVELS * 3) as usize],
            tex_slice_offset: [0; (SGI_ODYSSEY_OBJECTS + 2) as usize],
            tex_slice_width: [0; (SGI_ODYSSEY_OBJECTS + 2) as usize],
            tex_level0: 0,
            tex_lod: [0; 4],
            tex_scale_bias: [0; 8],
            tex_offset: [0; 3],
            tex_coord: [0; 2],
            tex_coord_r: 0,
            tex_coord_q: 0,
            tex_linear_plane: [[0; 4]; 2],
            tex_linear_plane_r: [0; 4],
            tex_linear_plane_q: [0; 4],
            tex_linear_valid: 0,
            tex_rect_reported: false,
            dma_seq: 0,
            trace_sequence: 0,
            #[cfg(test)]
            test_slice_sleep_ms: 0,
            did: Vec::new(),
            overlay: Vec::new(),
            gl_aux_reported: false,
            aux_mask_reported: false,
            aux_image_reported: false,
            query_current: [[0; 5]; 5],
            query_captured: [[0; 5]; 5],
            query_current_valid: 0,
            query_captured_valid: 0,
            query_stage: 0,
            query_kind: 0,
            query_count: 0,
            query_sync_index: 0,
            query_epoch: 0,
            query_captured_epoch: 0,
            query_base: 0,
            query_sync_base: 0,
            cmd_state: 0,
            cmd_tag: 0,
            cmd_operand: [0; SGI_ODYSSEY_CMD_OPERANDS as usize],
            cmd_operand_count: 0,
            cmd_operand_index: 0,
            win_origin: [0; 2],
            read_origin: [0; 2],
            clip_words: [0; (2 * SGI_ODYSSEY_CLIP_RECTS) as usize],
            id_compare: 0,
            clip_kept: 0,
            clip_rect: std::array::from_fn(|_| Default::default()),
            win_committed_origin: [0; 2],
            win_committed_clip: [0; (2 * SGI_ODYSSEY_CLIP_RECTS) as usize],
            win_committed_id_compare: 0,
            win_prior_origin: [0; 2],
            win_origin_pending: false,
            win_after_tail: false,
            cmd_from_kernel: false,
            gl_primitive: 0,
            gl_color: 0,
            gl_vertex_x: [0; SGI_ODYSSEY_GL_HELD_VERTICES as usize],
            gl_vertex_y: [0; SGI_ODYSSEY_GL_HELD_VERTICES as usize],
            gl_vertex_count: 0,
            line_stipple: [0; 3],
            poly_marks: Vec::new(),
            poly_row_lo: 0,
            poly_row_hi: 0,
            poly_col_lo: 0,
            gl_ctx: Default::default(),
            gl_bind_drawable: Default::default(),
            raster_bands: false,
            raster_band_pixels: 0,
            gl_in_prim: false,
            gl_begin_open: false,
            gl_integer_prim: false,
            gl_open_r: 0,
            gl_open_c: 0,
            gl_open_m3: 0,
            gl_open_m4: 0,
            gl_open_held: 0,
            gl_open_vertex: std::array::from_fn(|_| Default::default()),
            gl_restart_type: 0,
            gl_restart_select: 0,
            gl_restart_absorb: 0,
            gl_restart_total: 0,
            gl_restart_c: 0,
            gl_restart_m3: 0,
            gl_restart_m4: 0,
            gl_restart_armed: false,
            gl_restart_vertex: [[0.0; SGI_GL_VERTEX_FLOATS as usize]; 3],
            gl_matrix_slot: 0,
            gl_matrix: [0; 16],
            gl_texture_matrix: [0; 16],
            gl_rect: [0; 10],
            gl_depth_func: 0,
            gl_write_mask: 0,
            gl_combined: [0; 16],
            gl_block16: [0; 4],
            gl_rect_depth_logged: false,
            gl_rect_stencil_logged: false,
            gl_rect_pattern_logged: false,
            gl_index_mask_logged: false,
            depth_mem: Default::default(),
            depth_plane: 0,
            stencil_mem: Default::default(),
            stencil_plane: 0,
            gl_clip_box: [0; 2],
            gl_fog: [0; 6],
            gl_const: [0; SGI_ODYSSEY_GL_CONSTANTS as usize],
            gl_const_index: 0,
            gl_entry: [0; SGI_ODYSSEY_GL_ENTRIES as usize],
            gl_light_colour: [0; (SGI_ODYSSEY_GL_LIGHT_COLOURS * 4) as usize],
            gl_light_vec: [0; (SGI_ODYSSEY_GL_LIGHTS * 16) as usize],
            gl_light_colour_sel: 0,
            gl_light_vec_sel: 0,
            gl_material: [0; 16],
            gl_shininess_sqrt: 0,
            gl_back_material: [0; 16],
            gl_back_shininess_sqrt: 0,
            gl_colour_material: 0,
            gl_normal: [0; 3],
            gl_normal_matrix: [0; 16],
            gl_colour: [0; 4],
            gl_modelview: [0; 16],
            gl_clip_eye: [0; (SGI_GL_CLIP_PLANES * 4) as usize],
            gl_lit: false,
            gl_local_viewer: false,
            gl_colour_overridden: false,
            gl_program_logged: [0; (SGI_ODYSSEY_GL_PROGRAM_REPORTS * 3) as usize],
            gl_program_logged_count: 0,
            gl_program_mem: vec![0; (SGI_ODYSSEY_GL_PROGRAM_ENTRIES * 6) as usize],
            gl_program_addr: 0,
            gl_seq: [0; 22],
            gl_program_half: 0,
            gl_program_mem_reported: false,
            gl_two_sided_logged: false,
            gl_texgen_logged: false,
            gl_linear_texgen: 0,
            gl_clip_logged: false,
            gl_texture_stage_logged: false,
            gl_block_logged: false,
            ctx_open_prim_logged: false,
            ctx_shadow_fail_reported: false,
            gl_dither: false,
            gl_dither_reported: false,
            scanout_group25_reported: false,
            gl_color_index: 0,
            gl_color_is_index: false,
            colour_mode: [0; 2],
            colour_mode_reported: false,
            gl_index_lit_reported: false,
            dest_buffer_word: 0,
            plane_write_mask: 0,
            dma_colour_base: 0,
            copy_src: 0,
            copy_size: 0,
            copy_dst: 0,
            pattern_buffer: [0; 32],
            pattern_shape: 0,
            pattern_mode: 0,
            pattern_load_offset: 0,
            const_colour: [0; 2],
            blend_word: 0,
            image_active: false,
            image_x1: 0,
            image_y1: 0,
            image_x2: 0,
            image_y2: 0,
            image_cur_x: 0,
            image_cur_y: 0,
            image_fmt: 0,
            image_group_len: 0,
            image_group_painted: 0,
            image_code: 0,
            image_bytes: 0,
            image_nbytes: 0,
            image_code_reported: false,
            test_pixel_x: 0,
            test_pixel_y: 0,
            test_pixel_buffer: 0,
            test_pixel_count: 0,
            test_object_base: 0,
            scanout: Default::default(),
            reported_addr: [0; SGI_ODYSSEY_UNIMP_REPORTS as usize],
            reported_addr_count: 0,
            reported_cmd: [0; SGI_ODYSSEY_UNIMP_REPORTS as usize],
            reported_cmd_count: 0,
            line_stipple_reported: false,
            dma_bpp_reported: false,
            dma_readback_reported: false,
            dma_code_reported: false,
            dma_read_reported: 0,
        }
    }
}
