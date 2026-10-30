//! Ported from include/hw/display/sgi/rasterizer.h in
//! jamesbraid/qemu-sgi-origami.
//!
//! The software rasterizer the SGI graphics heads share: GL draws turned
//! into fragments and stored through the family's planes.
//!
//! Not ported (host renderer and test hooks only): the sgi_render_* draw,
//! raster, signature and resource-commit entry points,
//! SGIRasterTriangle/SGIRasterLineSpan and their helpers. Callback
//! `Error **errp` parameters are dropped. The fixed-point texture path is in
//! rasterizer_fixed.rs.

use super::*;

// The rules a head's rasterizer follows where OpenGL leaves a choice.

pub type SGIRounding = u32;
pub const SGI_ROUND_FLOOR: SGIRounding = 0;
pub const SGI_ROUND_HALF_UP: SGIRounding = 1;
pub const SGI_ROUND_HALF_EVEN: SGIRounding = 2;
pub const SGI_ROUND_HALF_AWAY: SGIRounding = 3;

pub type SGIArith = u32;
pub const SGI_ARITH_FIXED16: SGIArith = 0;
pub const SGI_ARITH_DOUBLE: SGIArith = 1;

pub type SGITieRule = u32;
pub const SGI_TIE_TOP_LEFT: SGITieRule = 0;
pub const SGI_TIE_TOP_LEFT_BOTTOM_ORIGIN: SGITieRule = 1;

pub type SGILineRule = u32;
pub const SGI_LINE_BRESENHAM_DIAMOND: SGILineRule = 0;
pub const SGI_LINE_COVERAGE_DOUBLE: SGILineRule = 1;

pub type SGIDither = u32;
pub const SGI_DITHER_NONE: SGIDither = 0;
pub const SGI_DITHER_BAYER4_TOP_ROWS: SGIDither = 1;

pub type SGIDitherAnchor = u32;
pub const SGI_DITHER_SCREEN: SGIDitherAnchor = 0;
pub const SGI_DITHER_WINDOW: SGIDitherAnchor = 1;

pub type SGISmoothRule = u32;
pub const SGI_SMOOTH_AREA: SGISmoothRule = 0;
pub const SGI_SMOOTH_TWO_PIXEL_FILTER: SGISmoothRule = 1;

// SGIRounding: how a window coordinate or depth is rounded to its grid
// (SGI_ROUND_HALF_AWAY is llround()). SGITieRule: which pixel centres on a
// shared triangle edge the edge owns, rows counted from the top or from the
// bottom. SGILineRule: which pixels a width-1 segment covers. SGIDither:
// BAYER4_TOP_ROWS is a 4x4 ordered dither, floor(c * max + B / 16) with B
// from the Bayer index matrix at (column, row) mod 4, rows from the top.
// SGIDitherAnchor: the display or the window. SGISmoothRule: AREA is the
// exact box filter; TWO_PIXEL_FILTER the separable tent filter.

#[derive(Clone, Default)]
pub struct SGIRasterRules {
    /// window coordinates snap to 2^-n pixel
    pub subpixel_bits: u8,
    /// per channel of a stored colour
    pub color_bits: u8,
    /// of a stored depth
    pub depth_bits: u8,
    /// of a stored stencil value, 0 for planes with no stencil buffer
    pub stencil_bits: u8,
    /// SGIRounding, for snapping and depth
    pub rounding: u8,
    /// SGITieRule
    pub tie_rule: u8,
    /// SGIArith
    pub arith: u8,
    /// SGILineRule
    pub line_rule: u8,
    /// SGIDither
    pub dither: u8,
    /// SGIDitherAnchor
    pub dither_anchor: u8,
    /// stored A width, zero means no alpha
    pub alpha_bits: u8,
    /// Zero derives precision from color/alpha bits.
    pub alpha_test_bits: u8,
    /// Smooth points, lines and polygons as their enables ask; without it
    /// a smoothed primitive is drawn aliased and reported.
    pub smooth_primitives: bool,
    /// SGISmoothRule
    pub smooth_rule: u8,
    /// Depth linear in eye distance (a w-buffer): a vertex's window depth
    /// is depth_row . e for its eye coordinates e, perspective-corrected
    /// per fragment; a depth outside [0, 1] is dropped.
    pub depth_row_given: bool,
    pub depth_row: [f64; 4],
    /// With depth_row, a second row whose product with the eye coordinates
    /// divides the first's. Not given, the divisor is one.
    pub depth_divisor_given: bool,
    pub depth_divisor: [f64; 4],
    /// With depth_row, how far outside [0, 1] a depth may lie and be
    /// clamped rather than dropped.
    pub depth_slack: f64,
    /// Stored depth is the IEEE single bits of the window depth shifted
    /// right by 30 - depth_bits (sgi_raster_depth_code()); needs depth_bits
    /// 10 to 30.
    pub depth_float: bool,
}

/// A vertex in clip coordinates, as view-volume clipping takes and makes it.
#[derive(Clone)]
pub struct SGIVolumeVertex {
    pub clip: [f64; 4],
    pub eye: [f64; 4],
    pub attr: [f32; SGI_GL_VERTEX_FLOATS],
}

impl Default for SGIVolumeVertex {
    fn default() -> Self {
        SGIVolumeVertex { clip: [0.0; 4], eye: [0.0; 4], attr: [0.0; SGI_GL_VERTEX_FLOATS] }
    }
}

/// The most vertices clipping a triangle by the six planes leaves. usize
/// (the C #define) so it can size arrays.
pub const SGI_VOLUME_TRIANGLE_MAX: usize = 3 + 6;

pub const SGI_GL_TEX_LEVELS_MAX: u32 = 31;

/// Absolute level identities in a borrowed, immutable normalized image view.
/// `rgba` / `component` out-parameters are `&mut`.
#[derive(Clone, Default)]
pub struct SGIGLTexLevel {
    pub level: u32,
    pub width: u32,
    pub height: u32,
    pub depth: u32,
    pub base_format: u32,
    pub internal_format: u32,
    pub border: u32,
    pub read_texel: Option<fn(Opaque, u32, u32, u32, &mut u32) -> bool>,
    /// Border indices range from -1 through width/height, inclusive.
    pub read_border_texel: Option<fn(Opaque, u32, i32, i32, &mut u32) -> bool>,
    pub read_texel_3d: Option<fn(Opaque, u32, u32, u32, u32, &mut u32) -> bool>,
    pub opaque: Opaque,
    /// Each signed axis ranges from -1 through its interior dimension.
    pub read_border_texel_3d: Option<fn(Opaque, u32, i32, i32, i32, &mut u32) -> bool>,
    /// Raw readers use base-format slots and per-channel widths.
    pub component_bits: [u8; 4],
    pub read_texel_components: Option<fn(Opaque, u32, u32, u32, &mut [u16; 4]) -> bool>,
    pub read_border_texel_components: Option<fn(Opaque, u32, i32, i32, &mut [u16; 4]) -> bool>,
    pub read_texel_3d_components: Option<fn(Opaque, u32, u32, u32, u32, &mut [u16; 4]) -> bool>,
    pub read_border_texel_3d_components: Option<fn(Opaque, u32, i32, i32, i32, &mut [u16; 4]) -> bool>,
}

#[derive(Clone, Default)]
pub struct SGIGLTexPyramid {
    /// `count` levels.
    pub levels: P<SGIGLTexLevel>,
    pub count: u32,
    pub base_level: u32,
    pub max_level: u32,
    pub min_lod: f64,
    pub max_lod: f64,
}

/// One family-owned level-zero image and its texture object's sampler.
/// volume selects 3D sampling, one_dimensional S-only sampling; they are
/// mutually exclusive. Dimensions count interior texels. Packed callbacks
/// return canonical RRGGBBAA; raw-component callbacks return base-format
/// channels normalized by component_bits. Borrowed, immutable through each
/// synchronous draw; no pointer is retained.
#[derive(Clone, Default)]
pub struct SGIGLTexImage {
    pub width: u32,
    pub height: u32,
    pub depth: u32,
    pub base_format: u32,
    pub border: u32,
    pub min_filter: u32,
    pub mag_filter: u32,
    pub wrap_s: u32,
    pub wrap_t: u32,
    pub wrap_r: u32,
    pub border_color: [f32; 4],
    pub read_texel: Option<fn(Opaque, u32, u32, &mut u32) -> bool>,
    pub read_texel_3d: Option<fn(Opaque, u32, u32, u32, &mut u32) -> bool>,
    pub volume: bool,
    pub one_dimensional: bool,
    pub opaque: Opaque,
    /// NULL selects the level-zero image fields.
    pub pyramid: P<SGIGLTexPyramid>,
    /// Required for a 2D image with border == 1; outside the interior only.
    pub read_border_texel: Option<fn(Opaque, i32, i32, &mut u32) -> bool>,
    /// Positive per-axis derivative factors; zero preserves identity.
    pub lod_scale_s: f64,
    pub lod_scale_t: f64,
    /// Expanded RGBA after filtering, before the texture environment.
    pub post_filter: Option<fn(Opaque, &mut [f64; 4])>,
    /// Required for a volume with border == 1, including depth one.
    pub read_border_texel_3d: Option<fn(Opaque, i32, i32, i32, &mut u32) -> bool>,
    /// Positive volume R derivative factor; zero preserves identity.
    pub lod_scale_r: f64,
    /// NULL selects the public default curve; lambda is already clamped.
    pub read_filter_weight: Option<fn(Opaque, f64, &mut f64) -> bool>,
    /// Borrowed level-zero detail image; L <= 0, mode ADD or MODULATE.
    pub detail_image: P<SGIGLTexLevel>,
    pub detail_level: i32,
    pub detail_mode: u32,
    /// Zero disables comparison. SGIX operators compare depth to projected R.
    pub shadow_compare: u32,
    pub shadow_ambient: f64,
    pub component_bits: [u8; 4],
    pub read_texel_components: Option<fn(Opaque, u32, u32, &mut [u16; 4]) -> bool>,
    pub read_texel_3d_components: Option<fn(Opaque, u32, u32, u32, &mut [u16; 4]) -> bool>,
    pub read_border_texel_components: Option<fn(Opaque, i32, i32, &mut [u16; 4]) -> bool>,
    pub read_border_texel_3d_components: Option<fn(Opaque, i32, i32, i32, &mut [u16; 4]) -> bool>,
    /// GL_SGIX_texture_coordinate_clamp: with max_clamp set, a CLAMP axis
    /// clamps to [0, max_clamp_*] instead of [0, 1].
    pub max_clamp: bool,
    pub max_clamp_s: f64,
    pub max_clamp_t: f64,
    pub max_clamp_r: f64,
}

/// Quantized scalar channels, each normalized by its own declared maximum.
#[derive(Clone, Copy, Default)]
pub struct SGIRasterColour {
    /// R, G, B, A; absent alpha is canonical zero
    pub component: [u32; 4],
}

pub type SGIRenderResult = u32;
pub const SGI_RENDER_OK: SGIRenderResult = 0;
pub const SGI_RENDER_OOM: SGIRenderResult = 1;
pub const SGI_RENDER_UNSUPPORTED: SGIRenderResult = 2;
pub const SGI_RENDER_RESOURCE_ERROR: SGIRenderResult = 3;
pub const SGI_RENDER_HOST_ERROR: SGIRenderResult = 4;

pub type SGIRenderRole = u32;
pub const SGI_RENDER_COLOUR: SGIRenderRole = 0;
pub const SGI_RENDER_INDEX: SGIRenderRole = 1;
pub const SGI_RENDER_DEPTH: SGIRenderRole = 2;
pub const SGI_RENDER_STENCIL: SGIRenderRole = 3;
pub const SGI_RENDER_OVERLAY: SGIRenderRole = 4;
pub const SGI_RENDER_WID: SGIRenderRole = 5;
pub const SGI_RENDER_TAG: SGIRenderRole = 6;
pub const SGI_RENDER_ACCUM: SGIRenderRole = 7;
pub const SGI_RENDER_TEXTURE: SGIRenderRole = 8;

pub type SGIRenderBuffer = u32;
pub const SGI_RENDER_BUFFER_NONE: SGIRenderBuffer = 0;
pub const SGI_RENDER_FRONT: SGIRenderBuffer = 1;
pub const SGI_RENDER_BACK: SGIRenderBuffer = 2;
pub const SGI_RENDER_AUX: SGIRenderBuffer = 3;

pub type SGIRenderEye = u32;
pub const SGI_RENDER_MONO: SGIRenderEye = 0;
pub const SGI_RENDER_LEFT: SGIRenderEye = 1;
pub const SGI_RENDER_RIGHT: SGIRenderEye = 2;

pub type SGIRenderNumeric = u32;
pub const SGI_RENDER_UINT: SGIRenderNumeric = 0;
pub const SGI_RENDER_SINT: SGIRenderNumeric = 1;
pub const SGI_RENDER_UNORM: SGIRenderNumeric = 2;
pub const SGI_RENDER_SNORM: SGIRenderNumeric = 3;
pub const SGI_RENDER_FLOAT: SGIRenderNumeric = 4;

pub type SGIRenderChannel = u32;
pub const SGI_RENDER_R: SGIRenderChannel = 0;
pub const SGI_RENDER_G: SGIRenderChannel = 1;
pub const SGI_RENDER_B: SGIRenderChannel = 2;
pub const SGI_RENDER_A: SGIRenderChannel = 3;
pub const SGI_RENDER_SCALAR: SGIRenderChannel = 4;

#[derive(Clone, Copy, Default)]
pub struct SGIRenderComponent {
    pub channel: SGIRenderChannel,
    pub numeric: SGIRenderNumeric,
    pub storage_bits: u8,
}

#[derive(Clone, Copy, Default)]
pub struct SGIRenderResource {
    pub backing_id: u64,
    pub backing_generation: u64,
    pub view_id: u64,
    pub view_generation: u64,
    pub role: SGIRenderRole,
    pub target: SGIColourTarget,
    pub buffer: SGIRenderBuffer,
    pub auxiliary_index: u32,
    pub eye: SGIRenderEye,
    pub width: u32,
    pub height: u32,
    pub depth: u32,
    pub texture_level: u32,
    pub border: [u8; 3],
    pub bottom_origin: bool,
    pub component_count: u8,
    pub component: [SGIRenderComponent; 4],
}

#[derive(Clone, Copy, Default)]
pub struct SGIRenderRect {
    pub x0: u32,
    pub y0: u32,
    pub x1: u32,
    pub y1: u32,
}

#[derive(Clone, Copy, Default)]
pub struct SGIRenderCommitResult {
    pub status: SGIRenderResult,
    pub complete_pixels: u64,
    pub partial_pixel: bool,
    pub extent_known: bool,
}

/// One byte per pixel, in the resource's row order, borrowed for one call.
#[derive(Clone, Copy, Default)]
pub struct SGIRenderWriteCoverage {
    pub pixel: P<u8>,
    pub stride_bytes: usize,
    pub capacity_bytes: usize,
}

/// The resource callbacks a family gives its planes. `slots` are the raw
/// slot words; `stride_slots`/`capacity_slots` are as the C passes them.
/// The C `Error **errp` is dropped.
#[derive(Clone, Default)]
pub struct SGIRenderResourceOps {
    pub snapshot: Option<fn(Opaque, &SGIRenderResource, u32, &SGIRenderRect, &mut [u32], usize, usize) -> SGIRenderResult>,
    pub commit: Option<fn(Opaque, &SGIRenderResource, u32, &SGIRenderRect, &[u32], usize, usize) -> SGIRenderCommitResult>,
    /// (opaque, resource, slice, rect, slots, stride_slots, capacity_slots, writes)
    pub commit_covered: Option<fn(Opaque, &SGIRenderResource, u32, &SGIRenderRect, &[u32], usize, usize, Option<&SGIRenderWriteCoverage>) -> SGIRenderCommitResult>,
    /// An index resource's commit: slots hold final indices already merged
    /// under effective_mask; uncovered pixels must keep their state.
    /// (..., writes, effective_mask)
    pub commit_index_covered: Option<fn(Opaque, &SGIRenderResource, u32, &SGIRenderRect, &[u32], usize, usize, Option<&SGIRenderWriteCoverage>, u32) -> SGIRenderCommitResult>,
}

/// The family's planes as the rasterizer reaches them, in plane pixels.
/// target is one of the drawable's colour targets, which only the family
/// interprets; a colour is the rules' color_bits a channel, red highest.
#[derive(Clone, Default)]
pub struct SGIRasterTarget {
    pub store_colour: Option<fn(Opaque, SGIColourTarget, i32, i32, u32)>,
    /// The colour stored at (x, y), or in colour-index mode the index;
    /// None for planes that cannot be read back.
    pub load_colour: Option<fn(Opaque, SGIColourTarget, i32, i32) -> u32>,
    /// (opaque, target, x, y, index, mask)
    pub store_index: Option<fn(Opaque, SGIColourTarget, i32, i32, u32, u32)>,
    pub load_depth: Option<fn(Opaque, i32, i32) -> u32>,
    pub store_depth: Option<fn(Opaque, i32, i32, u32)>,
    /// The stencil buffer; None for planes without one.
    pub load_stencil: Option<fn(Opaque, i32, i32) -> u32>,
    pub store_stencil: Option<fn(Opaque, i32, i32, u32)>,
    /// A fog factor the family supplies (opaque, window_z, eye_z); per
    /// draw, not GL state. None selects the built-in modes.
    pub fog_factor: Option<fn(Opaque, f32, f32) -> f32>,
    pub opaque: Opaque,
    /// Plane row of the display's top line, for a screen-anchored dither.
    pub screen_top: i32,
    /// Whether plane rows below the top line have larger y.
    pub screen_rows_down: bool,
    /// Current binding; resolved once by the family before drawing.
    pub texture: P<SGIGLTexImage>,
    /// An opaque polygon stipple: a fragment whose stipple bit is clear
    /// takes stipple_background (or stipple_background_index).
    pub stipple_opaque: bool,
    pub stipple_background: [f32; 4],
    pub stipple_background_index: u32,
    /// Optional complete RGBA stores and reads. Positive alpha_bits
    /// requires the store.
    pub store_rgba: Option<fn(Opaque, SGIColourTarget, i32, i32, &SGIRasterColour)>,
    pub load_rgba: Option<fn(Opaque, SGIColourTarget, i32, i32, &mut SGIRasterColour) -> bool>,
    /// `resource_count` resources, borrowed until the draw returns.
    pub resources: P<SGIRenderResource>,
    pub resource_count: usize,
    pub resource_ops: P<SGIRenderResourceOps>,
    /// Kept for the C initializer; bands run serially here.
    pub concurrent_pixels: bool,
    pub band_min_pixels: u32,
    pub mixed_frame: bool,
}

pub type SGIRasterTexturePath = u32;
pub const SGI_RASTER_TEXTURE_SPANS: SGIRasterTexturePath = 0;
pub const SGI_RASTER_TEXTURE_FIXED: SGIRasterTexturePath = 1;
pub const SGI_RASTER_TEXTURE_DOUBLE: SGIRasterTexturePath = 2;

/// Why a checked draw or raster operation was refused (host renderer
/// feature rows; the Odyssey model only names and counts them).
pub type SGIRenderRefusal = u32;
pub const SGI_RENDER_REFUSAL_NONE: SGIRenderRefusal = 0;
pub const SGI_RENDER_REFUSAL_RULES: SGIRenderRefusal = 1;
pub const SGI_RENDER_REFUSAL_DITHER: SGIRenderRefusal = 2;
pub const SGI_RENDER_REFUSAL_PRIMITIVE: SGIRenderRefusal = 3;
pub const SGI_RENDER_REFUSAL_SMOOTHING: SGIRenderRefusal = 4;
pub const SGI_RENDER_REFUSAL_COLOUR_INDEX: SGIRenderRefusal = 5;
pub const SGI_RENDER_REFUSAL_INDEX_CLIPPED: SGIRenderRefusal = 6;
pub const SGI_RENDER_REFUSAL_BUFFERS: SGIRenderRefusal = 7;
pub const SGI_RENDER_REFUSAL_FRAGMENT_TESTS: SGIRenderRefusal = 8;
pub const SGI_RENDER_REFUSAL_FOG: SGIRenderRefusal = 9;
pub const SGI_RENDER_REFUSAL_FACING: SGIRenderRefusal = 10;
pub const SGI_RENDER_REFUSAL_CLIP_PLANES: SGIRenderRefusal = 11;
pub const SGI_RENDER_REFUSAL_LOGIC_OP: SGIRenderRefusal = 12;
pub const SGI_RENDER_REFUSAL_POLYGON_OFFSET: SGIRenderRefusal = 13;
pub const SGI_RENDER_REFUSAL_WINDOW: SGIRenderRefusal = 14;
pub const SGI_RENDER_REFUSAL_GEOMETRY: SGIRenderRefusal = 15;
pub const SGI_RENDER_REFUSAL_LINE_SHAPE: SGIRenderRefusal = 16;
pub const SGI_RENDER_REFUSAL_RESOURCE: SGIRenderRefusal = 17;
pub const SGI_RENDER_REFUSAL_STORE_WIDTH: SGIRenderRefusal = 18;
pub const SGI_RENDER_REFUSAL_BLEND: SGIRenderRefusal = 19;
pub const SGI_RENDER_REFUSAL_BLEND_OVERLAP: SGIRenderRefusal = 20;
pub const SGI_RENDER_REFUSAL_TEXTURE: SGIRenderRefusal = 21;
pub const SGI_RENDER_REFUSAL_PROVIDER: SGIRenderRefusal = 22;
pub const SGI_RENDER_REFUSAL_RASTER_OP: SGIRenderRefusal = 23;
pub const SGI_RENDER_REFUSAL_MIXED_FRAME: SGIRenderRefusal = 24;
pub const SGI_RENDER_REFUSAL_HOST_VIEWPORT: SGIRenderRefusal = 25;
pub const SGI_RENDER_REFUSAL_HOST_POINT_SIZE: SGIRenderRefusal = 26;
pub const SGI_RENDER_REFUSAL_HOST_PROGRAM: SGIRenderRefusal = 27;
pub const SGI_RENDER_REFUSAL_HOST_PROBE: SGIRenderRefusal = 28;
pub const SGI_RENDER_REFUSAL_HOST_STAGING: SGIRenderRefusal = 29;
pub const SGI_RENDER_REFUSAL_HOST_DRIVER: SGIRenderRefusal = 30;
pub const SGI_RENDER_REFUSAL_SNAPSHOT: SGIRenderRefusal = 31;
pub const SGI_RENDER_REFUSAL_PRECISION: SGIRenderRefusal = 32;
pub const SGI_RENDER_REFUSAL__MAX: SGIRenderRefusal = 33;

/// A stable lowercase name for counters and traces, such as "fog".
pub fn sgi_render_refusal_name(reason: SGIRenderRefusal) -> &'static str {
    // hw/display/sgi/renderer.c, sgi_render_refusals[].name.
    const NAMES: [&str; SGI_RENDER_REFUSAL__MAX as usize] = [
        "none", "rules", "dither", "primitive", "smoothing", "colour-index", "index-clipped", "buffers", "fragment-tests", "fog", "facing", "clip-planes", "logic-op", "polygon-offset", "window", "geometry", "line-shape", "resource", "store-width", "blend", "blend-overlap", "texture", "provider", "raster-op", "mixed-frame", "host-viewport", "host-point-size", "host-program", "host-probe", "host-staging", "host-driver", "snapshot", "precision"
    ];
    NAMES.get(reason as usize).copied().unwrap_or("unknown")
}

/// A prepared texture sampler (private to rasterizer.c). Borrowed state and image stay immutable until it is
/// dropped.
#[derive(Clone, Default)]
pub struct SGIRasterTexture {
    pub target: SGIRasterTarget,
    pub draw: SGISoftDraw,
    pub reports: u32,
}

pub type SGIRasterTextureResult = u32;
pub const SGI_RASTER_TEXTURE_READY: SGIRasterTextureResult = 0;
pub const SGI_RASTER_TEXTURE_DISABLED: SGIRasterTextureResult = 1;
pub const SGI_RASTER_TEXTURE_INCOMPLETE: SGIRasterTextureResult = 2;
pub const SGI_RASTER_TEXTURE_UNSUPPORTED: SGIRasterTextureResult = 3;
pub const SGI_RASTER_TEXTURE_OOM: SGIRasterTextureResult = 4;

// ---------------------------------------------------------------------
// Ported from hw/display/sgi/rasterizer.c lines 1-3145 (the fixed-point
// texture path, lines 3146-4089, is not ported).
// ---------------------------------------------------------------------

pub fn fmax(a: f64, b: f64) -> f64 {
    if a > b { a } else { b }
}
pub fn fmin(a: f64, b: f64) -> f64 {
    if a < b { a } else { b }
}

/// The drawable's window: its origin and the bounds of the pixels it owns,
/// in plane pixels, and the rectangles themselves.
#[derive(Clone, Copy, Default)]
pub struct SGISoftWindow {
    pub ox: i32,
    pub oy: i32,
    pub x0: i32,
    pub y0: i32,
    pub x1: i32,
    pub y1: i32,
    pub clip: P<SGIClipList>,
}

/// One draw's fixed inputs.
#[derive(Clone, Copy, Default)]
pub struct SGISoftDraw {
    pub rules: P<SGIRasterRules>,
    pub t: P<SGIRasterTarget>,
    pub st: P<SGIGLDrawState>,
    pub win: SGISoftWindow,
    pub targets: [SGIColourTarget; SGI_GL_MAX_TARGETS as usize],
    pub ntargets: u32,
    /// Back-facing polygons take the vertices' back colours.
    pub back_colors: bool,
    /// Fragments blend with the colour they replace.
    pub blend: bool,
    /// Fragments combine with it by the logical operation instead.
    pub logic_op: bool,
    /// In RGBA mode, the packed colour's bits glColorMask lets through.
    pub colour_mask: u32,
    /// The stored colour is dithered: GL_DITHER and the rules' dither.
    pub dither: bool,
    pub texture: P<SGIGLTexImage>,
    pub texture_base: P<SGIGLTexLevel>,
    pub texture_last: u32,
    pub texture_reports: P<u32>,
    pub point_size: u32,
    pub line_width: u32,
    pub point_smooth: bool,
    pub line_smooth: bool,
    pub smooth_point_size: f64,
    pub smooth_line_width: f64,
    pub polygon_smooth: bool,
    pub polygon_stipple: P<u32>,
    pub rgba_mask: u32,
    pub rgba_read: bool,
    pub rgba_reported: P<bool>,
    /// Texels already read in this draw, or NULL (sgi_soft_texel_cache).
    pub texels: P<SGISoftTexelCache>,
    /// Filled triangles sample in fixed point, or NULL (sgi_soft_fixed).
    pub fixed: P<SGISoftFixedTex>,
    /// Of bands > 1, the rows this pass draws (sgi_soft_row_in_band()).
    pub band: u32,
    pub bands: u32,
}

/// A vertex after transformation, ready to rasterize.
#[derive(Clone, Copy)]
pub struct SGISoftVertex {
    /// window position in subpixels
    pub x: i64,
    pub y: i64,
    /// normalized depth; [0, 1] inside the view volume
    pub z: f64,
    /// 1 / clip w, for perspective-correct colour
    pub inv_w: f64,
    /// the rules' depth divisor over clip w
    pub depth_w: f64,
    pub rgba: [f64; 4],
    pub texcoord: [f64; 4],
    /// the colour index, in colour-index mode
    pub index: f64,
    /// the fog coordinate, |z| in eye coordinates
    pub fog: f64,
    /// Each client clip plane's value at the vertex, kept where >= 0.
    pub clip: [f64; SGI_GL_CLIP_PLANES as usize],
}

impl Default for SGISoftVertex {
    fn default() -> Self {
        SGISoftVertex {
            x: 0,
            y: 0,
            z: 0.0,
            inv_w: 0.0,
            depth_w: 0.0,
            rgba: [0.0; 4],
            texcoord: [0.0; 4],
            index: 0.0,
            fog: 0.0,
            clip: [0.0; SGI_GL_CLIP_PLANES as usize],
        }
    }
}

/// A fragment's values as rasterization leaves them (OpenGL 1.2.1, 3.1),
/// before the per-fragment operations of chapter 4: its colour, each
/// component in [0, 1], and in colour-index mode its index.
#[derive(Clone, Copy, Default)]
pub struct SGISoftFrag {
    pub rgba: [f64; 4],
    pub texcoord: [f64; 4],
    pub texture_rho: f64,
    /// With fixed set, the texel the fixed-point sampler filtered for the
    /// fragment, which the texture environment applies in place of
    /// sampling texcoord.
    pub fixed: bool,
    pub texel: [f64; 4],
    pub index: f64,
    /// the fog coordinate (3.10)
    pub fog: f64,
}

pub const SGI_RASTER_PARALLEL_MAX_BANDS: u32 = 16;
pub const SGI_RASTER_PARALLEL_DEFAULT_BANDS: u32 = 4;
pub const SGI_RASTER_PARALLEL_MIN_PIXELS: u64 = 2048;

pub const SGI_SOFT_BAND_ROWS_SHIFT: u32 = 3; // rows interleave in runs of 8

pub static sgi_soft_bands_wanted: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
pub static sgi_soft_bands_min_pixels: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(SGI_RASTER_PARALLEL_MIN_PIXELS);
/// The host's processors, read once (sgi_soft_pool_init()): every draw and
/// clear chooses its bands. A later change to the host's processors is not
/// seen.
pub static sgi_soft_host_processors: std::sync::OnceLock<u32> = std::sync::OnceLock::new();

thread_local! {
    /// A band other than the first repeats the primitive work: say it once.
    pub static sgi_soft_band_quiet: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

pub type SGISoftBandFn = fn(Opaque, u32, u32);

/// The pool's shared state, under sgi_soft_pool.lock. The job's opaque
/// pointer is a P, which is Send: a band's data is the caller's, which the
/// caller keeps alive until every band is done.
#[derive(Default)]
pub struct SGISoftPoolState {
    pub threads: u32,
    pub generation: u64,
    pub fn_: Option<SGISoftBandFn>,
    pub opaque: Opaque,
    pub bands: u32,
    pub pending: u32,
    /// A band panicked: the caller, once all are done, panics too.
    pub panicked: bool,
}

pub struct SGISoftPool {
    /// one band-parallel call at a time
    pub run: std::sync::Mutex<()>,
    pub lock: std::sync::Mutex<SGISoftPoolState>,
    pub start: std::sync::Condvar,
    pub done: std::sync::Condvar,
}

pub static sgi_soft_pool: SGISoftPool = SGISoftPool {
    run: std::sync::Mutex::new(()),
    lock: std::sync::Mutex::new(SGISoftPoolState {
        threads: 0,
        generation: 0,
        fn_: None,
        opaque: P(std::ptr::null_mut()),
        bands: 0,
        pending: 0,
        panicked: false,
    }),
    start: std::sync::Condvar::new(),
    done: std::sync::Condvar::new(),
};

/// Count the host's processors, once, on the first draw or clear that
/// chooses its bands. Workers start only when a draw first runs in bands.
pub fn sgi_soft_pool_init() {
    sgi_soft_host_processors.get_or_init(|| {
        std::cmp::max(std::thread::available_parallelism().map_or(1, |n| n.get() as u32), 1)
    });
}

/// A band worker: waits for each generation of the pool's job, runs its
/// band of it if it has one, and counts itself done.
pub fn sgi_soft_band_worker(band: u32) {
    let mut seen: u64 = 0;
    let mut g = sgi_soft_pool.lock.lock().unwrap_or_else(|e| e.into_inner());

    loop {
        while g.generation == seen {
            g = sgi_soft_pool.start.wait(g).unwrap_or_else(|e| e.into_inner());
        }
        seen = g.generation;
        if band >= g.bands {
            continue;
        }
        let fn_ = g.fn_.unwrap();
        let job = g.opaque;
        let bands = g.bands;
        drop(g);
        let ok = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| fn_(job, band, bands))).is_ok();
        g = sgi_soft_pool.lock.lock().unwrap_or_else(|e| e.into_inner());
        if !ok {
            g.panicked = true;
        }
        g.pending -= 1;
        if g.pending == 0 {
            sgi_soft_pool.done.notify_one();
        }
    }
}

pub fn sgi_soft_bands() -> u32 {
    sgi_soft_pool_init();
    let mut bands = sgi_soft_bands_wanted.load(std::sync::atomic::Ordering::Relaxed);

    if bands == 0 {
        bands = std::cmp::min(SGI_RASTER_PARALLEL_DEFAULT_BANDS, *sgi_soft_host_processors.get().unwrap());
    }
    std::cmp::min(bands, SGI_RASTER_PARALLEL_MAX_BANDS)
}

/// fn(opaque, band, bands) for each band, band 0 on this thread, the rest
/// on workers, returning when all have; false, having run none, when
/// another thread is using the workers.
pub fn sgi_soft_bands_run(f: SGISoftBandFn, opaque: Opaque, bands: u32) -> bool {
    sgi_soft_pool_init();
    let _run = match sgi_soft_pool.run.try_lock() {
        Ok(g) => g,
        Err(std::sync::TryLockError::Poisoned(e)) => e.into_inner(),
        Err(std::sync::TryLockError::WouldBlock) => return false,
    };
    let mut g = sgi_soft_pool.lock.lock().unwrap_or_else(|e| e.into_inner());
    while g.threads < bands - 1 {
        let n = g.threads + 1;

        let spawned = std::thread::Builder::new()
            .name(format!("sgi-raster-{}", n))
            .spawn(move || sgi_soft_band_worker(n));
        if spawned.is_err() {
            // No thread to run that band: draw on fewer threads is not an
            // option once the bands are chosen, so run none.
            return false;
        }
        g.threads += 1;
    }
    g.fn_ = Some(f);
    g.opaque = opaque;
    g.bands = bands;
    g.pending = bands - 1;
    g.panicked = false;
    g.generation += 1;
    sgi_soft_pool.start.notify_all();
    drop(g);

    let own = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(opaque, 0, bands)));

    let mut g = sgi_soft_pool.lock.lock().unwrap_or_else(|e| e.into_inner());
    while g.pending != 0 {
        g = sgi_soft_pool.done.wait(g).unwrap_or_else(|e| e.into_inner());
    }
    let panicked = g.panicked;
    drop(g);
    drop(_run);
    if let Err(e) = own {
        std::panic::resume_unwind(e);
    }
    if panicked {
        panic!("sgi-raster: a band panicked");
    }
    true
}

/// The pixel count from which a draw or clear into @t runs in bands.
pub fn sgi_soft_band_min_pixels(t: &SGIRasterTarget) -> u64 {
    if t.band_min_pixels != 0 {
        t.band_min_pixels as u64
    } else {
        sgi_soft_bands_min_pixels.load(std::sync::atomic::Ordering::Relaxed)
    }
}

pub fn sgi_soft_row_in_band(r: &SGISoftDraw, py: i32) -> bool {
    r.bands <= 1 || ((py as u32) >> SGI_SOFT_BAND_ROWS_SHIFT) % r.bands == r.band
}

pub fn sgi_soft_window(st: &SGIGLDrawState, win: &mut SGISoftWindow) -> bool {
    let d = &st.drawable;

    if d.bound == 0 {
        crate::qlog!("sgi-gl: draw with no window bound dropped");
        return false;
    }
    if d.clip.n == 0 {
        // Other windows cover all of this one: nothing to draw.
        return false;
    }
    win.ox = d.origin[0];
    win.oy = d.origin[1];
    win.x0 = d.clip.r[0].x0;
    win.y0 = d.clip.r[0].y0;
    win.x1 = d.clip.r[0].x1;
    win.y1 = d.clip.r[0].y1;
    for i in 1..std::cmp::min(d.clip.n as usize, SGI_CLIP_MAX) {
        win.x0 = win.x0.min(d.clip.r[i].x0);
        win.y0 = win.y0.min(d.clip.r[i].y0);
        win.x1 = win.x1.max(d.clip.r[i].x1);
        win.y1 = win.y1.max(d.clip.r[i].y1);
    }
    win.clip = P::from_ref(&d.clip);
    true
}

pub fn sgi_raster_rules_supported(r: &SGIRasterRules) -> bool {
    r.subpixel_bits >= 1 && r.subpixel_bits <= 12 &&
        r.color_bits >= 1 && r.color_bits <= 10 &&
        r.depth_bits >= 1 && r.depth_bits <= 32 &&
        (!r.depth_float || (r.depth_bits >= 10 && r.depth_bits <= 30)) &&
        (!r.depth_row_given ||
         (r.depth_row[0].is_finite() && r.depth_row[1].is_finite() &&
          r.depth_row[2].is_finite() && r.depth_row[3].is_finite())) &&
        (!r.depth_divisor_given ||
         (r.depth_row_given &&
          r.depth_divisor[0].is_finite() && r.depth_divisor[1].is_finite() &&
          r.depth_divisor[2].is_finite() && r.depth_divisor[3].is_finite())) &&
        r.depth_slack >= 0.0 && r.depth_slack < 1.0 &&
        r.stencil_bits <= 16 && r.alpha_bits <= 10 &&
        r.alpha_test_bits <= 16 &&
        (r.rounding as u32) <= SGI_ROUND_HALF_AWAY &&
        (r.tie_rule as u32) == SGI_TIE_TOP_LEFT_BOTTOM_ORIGIN &&
        (r.arith as u32) == SGI_ARITH_DOUBLE &&
        (r.line_rule as u32) == SGI_LINE_COVERAGE_DOUBLE &&
        (r.dither as u32) <= SGI_DITHER_BAYER4_TOP_ROWS &&
        (r.smooth_rule as u32) <= SGI_SMOOTH_TWO_PIXEL_FILTER &&
        (r.dither as u32 == SGI_DITHER_NONE ||
         r.dither_anchor as u32 == SGI_DITHER_SCREEN)
}

pub fn sgi_soft_check_rules(r: &SGIRasterRules) -> bool {
    if !sgi_raster_rules_supported(r) {
        crate::qlog!("sgi-gl: raster rules are not implemented; nothing drawn");
        return false;
    }
    true
}

/// Whether the target can take the colour-index work a draw state asks.
pub fn sgi_soft_check_index(t: &SGIRasterTarget, index_bits: u32) -> bool {
    if index_bits != 0 && t.store_index.is_none() {
        crate::qlog!("sgi-gl: colour-index drawing is not implemented for these planes; nothing drawn");
        return false;
    }
    true
}

/// x rounded to an integer by the rules' rounding.
pub fn sgi_soft_round(r: &SGIRasterRules, x: f64) -> i64 {
    let rounding = r.rounding as u32;
    if rounding != SGI_ROUND_FLOOR && rounding != SGI_ROUND_HALF_UP && rounding != SGI_ROUND_HALF_EVEN {
        // llround(), without the call: x less its truncation is exact, so
        // a fraction of a half or more rounds away from zero.
        let t = x as i64;
        let frac = x - t as f64;
        return t + (frac >= 0.5) as i64 - (frac <= -0.5) as i64;
    }
    let f = x.floor();
    let frac = x - f;
    match rounding {
        SGI_ROUND_FLOOR => f as i64,
        SGI_ROUND_HALF_UP => f as i64 + (frac >= 0.5) as i64,
        _ => {
            if frac == 0.5 {
                return f as i64 + ((f as i64) & 1);
            }
            f as i64 + (frac > 0.5) as i64
        }
    }
}

/// The largest stored depth.
pub fn sgi_soft_depth_max(r: &SGIRasterRules) -> u32 {
    u32::MAX.wrapping_shr(32u32.wrapping_sub(r.depth_bits as u32))
}

/// A non-negative single's bits order as unsigned integers do, so the
/// shifted bits of a depth in [0, 1], at most 0x3f800000 >> (30 -
/// depth_bits), keep its order in depth_bits bits.
pub fn sgi_raster_depth_code(rules: &SGIRasterRules, z: f64) -> u32 {
    if !rules.depth_float {
        return sgi_soft_round(rules, z * sgi_soft_depth_max(rules) as f64) as u32;
    }
    let f = fmin(fmax(z, 0.0), 1.0) as f32;
    let bits = f.to_bits();
    bits.wrapping_shr(30u32.wrapping_sub(rules.depth_bits as u32))
}

/// A component clamped to [0, 1] and scaled to the rules' colour bits, a
/// fraction of a step rounded half up, whatever the rules' rounding: the
/// sum is truncated (OpenGL 1.2.1, 2.13.9).
pub fn sgi_soft_channel(r: &SGIRasterRules, c: f64) -> u32 {
    let max = (1u32.wrapping_shl(r.color_bits as u32)).wrapping_sub(1);

    if !(c > 0.0) {
        return 0;
    }
    if c >= 1.0 {
        return max;
    }
    (c * max as f64 + 0.5) as u32
}

/// Incoming alpha precision is independent of absent or narrower stored A.
pub fn sgi_soft_alpha_max(r: &SGIRasterRules) -> u32 {
    (1u32.wrapping_shl(std::cmp::max(2, std::cmp::max(r.color_bits, r.alpha_bits)) as u32)).wrapping_sub(1)
}

pub fn sgi_soft_alpha(r: &SGIRasterRules, value: f64) -> u32 {
    let max = sgi_soft_alpha_max(r);

    if !(value > 0.0) {
        0
    } else if value >= 1.0 {
        max
    } else {
        (value * max as f64 + 0.5) as u32
    }
}

pub fn sgi_soft_alpha_test(r: &SGIRasterRules, value: f64) -> u32 {
    let max = if r.alpha_test_bits != 0 {
        (1u32.wrapping_shl(r.alpha_test_bits as u32)).wrapping_sub(1)
    } else {
        sgi_soft_alpha_max(r)
    };

    if !(value > 0.0) {
        0
    } else if value >= 1.0 {
        max
    } else {
        (value * max as f64 + 0.5) as u32
    }
}

/// Internal nearest conversion precedes storage truncation, even on clears.
pub fn sgi_soft_stored_alpha(r: &SGIRasterRules, value: f64) -> u32 {
    let m = sgi_soft_alpha_max(r);
    if m == 0 {
        return 0;
    }
    sgi_soft_alpha(r, value)
        .wrapping_mul((1u32.wrapping_shl(r.alpha_bits as u32)).wrapping_sub(1))
        / m
}

pub fn sgi_soft_rgba_mask(r: &SGIRasterRules, mask: u32) -> u32 {
    mask & if r.alpha_bits != 0 { SGI_GL_COLOUR_MASK_ALL } else { SGI_GL_COLOUR_MASK_ALPHA - 1 }
}

/// Reject unsupported capabilities before the operation can write any plane.
pub fn sgi_soft_rgba_capable(r: &SGIRasterRules, t: &SGIRasterTarget, read: bool) -> bool {
    if (r.alpha_bits != 0 && t.store_rgba.is_none()) ||
        (t.store_rgba.is_some() && read && t.load_rgba.is_none()) ||
        (t.store_rgba.is_none() && t.load_rgba.is_some())
    {
        crate::qlog!("sgi-gl: inconsistent RGBA callbacks; operation dropped");
        return false;
    }
    true
}

/// Read every selected destination before any colour/depth/stencil write.
pub fn sgi_soft_rgba_load(
    r: &SGIRasterRules,
    t: &SGIRasterTarget,
    targets: &[SGIColourTarget],
    count: u32,
    x: i32,
    y: i32,
    dest: &mut [SGIRasterColour; SGI_GL_MAX_TARGETS as usize],
    reported: &mut bool,
) -> bool {
    for i in 0..count as usize {
        let mut valid = (t.load_rgba.unwrap())(t.opaque, targets[i], x, y, &mut dest[i]);

        let mut k = 0;
        while valid && k < 4 {
            let bits = if k == 3 { r.alpha_bits } else { r.color_bits };

            // An absent A is ignored, including its returned representation.
            valid = bits == 0 || dest[i].component[k] < (1u32 << bits);
            k += 1;
        }
        if !valid {
            // Bands of one draw share the flag (sgi_soft_bands_run()).
            let old = *reported;
            *reported = true;
            if !old {
                crate::qlog!("sgi-gl: RGBA destination read failed; pixel writes dropped");
            }
            return false;
        }
        if r.alpha_bits == 0 {
            dest[i].component[3] = 0;
        }
    }
    true
}

/// A colour, each component clamped and rounded to the rules' bits.
pub fn sgi_soft_rgb(r: &SGIRasterRules, c: &[f64]) -> u32 {
    (sgi_soft_channel(r, c[0]).wrapping_shl(2 * r.color_bits as u32)) |
        (sgi_soft_channel(r, c[1]).wrapping_shl(r.color_bits as u32)) |
        sgi_soft_channel(r, c[2])
}

/// SGI_DITHER_BAYER4_TOP_ROWS's thresholds, in sixteenths of a step.
pub static sgi_soft_bayer4: [[u8; 4]; 4] = [
    [0, 8, 2, 10],
    [12, 4, 14, 6],
    [3, 11, 1, 9],
    [15, 7, 13, 5],
];

/// A fragment's colour at plane pixel (x, y) as the planes store it:
/// dithered while the draw dithers, otherwise rounded as sgi_soft_rgb()
/// rounds. A clamped component of 0 or 1 is exact either way, and one
/// below 1 stays below max + 1 with the largest threshold added.
pub fn sgi_soft_store_rgb(r: &SGISoftDraw, x: i32, y: i32, c: &[f64]) -> u32 {
    let bits = r.rules.color_bits as u32;
    let max: u32 = (1u32 << bits) - 1;
    let mut out: u32 = 0;

    if !r.dither {
        return sgi_soft_rgb(&r.rules, c);
    }
    let row: i64 = if r.t.screen_rows_down {
        y as i64 - r.t.screen_top as i64
    } else {
        r.t.screen_top as i64 - y as i64
    };
    let b = sgi_soft_bayer4[(row & 3) as usize][(x & 3) as usize] as f64 / 16.0;
    for k in 0..3usize {
        let v: u32;

        if !(c[k] > 0.0) {
            v = 0;
        } else if c[k] >= 1.0 {
            v = max;
        } else {
            v = (c[k] * max as f64 + b).floor() as u32;
        }
        out |= v.wrapping_shl((2 - k as u32) * bits);
    }
    out
}

pub fn sgi_soft_edge(a: &SGISoftVertex, b: &SGISoftVertex, px: i64, py: i64) -> i64 {
    (b.x.wrapping_sub(a.x)).wrapping_mul(py.wrapping_sub(a.y))
        .wrapping_sub((b.y.wrapping_sub(a.y)).wrapping_mul(px.wrapping_sub(a.x)))
}

/// SGI_TIE_TOP_LEFT_BOTTOM_ORIGIN: a counter-clockwise edge owns the pixel
/// centres on it when it is a left edge (running down) or a top edge
/// (horizontal, running left), with rows counted up from the bottom of the
/// plane, so two triangles sharing an edge never both draw its centres, as
/// OpenGL 1.2.1, 3.5.1 requires.
pub fn sgi_soft_owns(a: &SGISoftVertex, b: &SGISoftVertex, e: i64) -> bool {
    if e != 0 {
        return e > 0;
    }
    b.y < a.y || (b.y == a.y && b.x < a.x)
}

/// Narrow [*lo, *hi], pixel steps along a row from a start whose edge value
/// is e, to the steps where sgi_soft_owns() holds for the edge from a to b,
/// whose value moves by step a pixel. The edge function is linear along the
/// row, so the owned steps are one run: e + k * step >= need, where need is
/// 0 for an edge that owns the centres on it and 1 for one that does not.
pub fn sgi_soft_owned_run(a: &SGISoftVertex, b: &SGISoftVertex, e: i64, step: i64, lo: &mut i64, hi: &mut i64) {
    let need: i64 = if sgi_soft_owns(a, b, 0) { 0 } else { 1 };
    let d = need.wrapping_sub(e);

    if step > 0 {
        // The first k with k * step >= d, rounding the quotient up.
        let v = if d > 0 { (d + step - 1) / step } else { -(-d / step) };
        *lo = std::cmp::max(*lo, v);
    } else if step < 0 {
        // The last k with k * -step <= -d, rounding the quotient down.
        let v = if -d >= 0 { -d / -step } else { -((d - step - 1) / -step) };
        *hi = std::cmp::min(*hi, v);
    } else if d > 0 {
        *hi = *lo - 1;
    }
}

/// Whether value passes a test comparing it with ref by func, the
/// comparison minus GL_NEVER: the depth test's incoming depth against the
/// stored one (4.1.5), the alpha test's alpha against its reference
/// (4.1.4), the stencil test's masked reference against the masked stored
/// value (4.1.5).
pub fn sgi_soft_compare(func: u32, value: u32, ref_: u32) -> bool {
    match func {
        SGI_GL_NEVER => false,
        SGI_GL_LESS => value < ref_,
        SGI_GL_EQUAL => value == ref_,
        SGI_GL_LEQUAL => value <= ref_,
        SGI_GL_GREATER => value > ref_,
        SGI_GL_NOTEQUAL => value != ref_,
        SGI_GL_GEQUAL => value >= ref_,
        _ => true,
    }
}

pub fn sgi_raster_alpha_test(r: &SGIRasterRules, func: u32, alpha: f64, ref_: f64) -> bool {
    sgi_soft_compare(func, sgi_soft_alpha_test(r, alpha), sgi_soft_alpha_test(r, ref_))
}

/// Whether the planes have a stencil buffer, and its largest value.
pub fn sgi_soft_stencil_max(rules: &SGIRasterRules, t: &SGIRasterTarget, max: &mut u32) -> bool {
    *max = (1u32.wrapping_shl(rules.stencil_bits as u32)).wrapping_sub(1);
    rules.stencil_bits != 0 && t.load_stencil.is_some() && t.store_stencil.is_some()
}

/// One of glStencilOp's operations on the stencil value at (x, y), the
/// result written under the stencil write mask (4.1.5, 4.2.2). GL_INCR
/// and GL_DECR clamp to the buffer's range.
pub fn sgi_soft_stencil_op(st: &SGIGLDrawState, t: &SGIRasterTarget, x: i32, y: i32, op: u32, max: u32, ref_: u32) {
    let old = (t.load_stencil.unwrap())(t.opaque, x, y) & max;
    let mask = st.stencil_write_mask & max;
    let value: u32;

    match op {
        SGI_GL_KEEP => return,
        SGI_GL_REPLACE => value = ref_,
        SGI_GL_INCR => value = if old < max { old + 1 } else { max },
        SGI_GL_DECR => value = if old != 0 { old - 1 } else { 0 },
        SGI_GL_INVERT => value = !old & max,
        _ => value = 0,
    }
    (t.store_stencil.unwrap())(t.opaque, x, y, (old & !mask) | (value & mask));
}

pub fn sgi_raster_depth_stencil(rules: &SGIRasterRules, st: &SGIGLDrawState, t: &SGIRasterTarget, x: i32, y: i32, z: f64) -> bool {
    let mut smax: u32 = 0;
    let mut sref: u32 = 0;

    if !(z >= 0.0 && z <= 1.0) ||
        (st.depth_test != 0 && (t.load_depth.is_none() || (st.depth_mask != 0 && t.store_depth.is_none())))
    {
        return false;
    }
    // The reference clamps to the buffer's range; with no stencil buffer
    // the test passes and nothing is written.
    let stencil = st.stencil_test != 0 && sgi_soft_stencil_max(rules, t, &mut smax);
    if stencil {
        let vmask = st.stencil_value_mask;

        sref = if st.stencil_ref < 0 { 0 } else { std::cmp::min(st.stencil_ref as u32, smax) };
        if !sgi_soft_compare(
            st.stencil_func,
            sref & vmask,
            (t.load_stencil.unwrap())(t.opaque, x, y) & smax & vmask,
        ) {
            sgi_soft_stencil_op(st, t, x, y, st.stencil_op[0], smax, sref);
            return false;
        }
    }
    let zq = sgi_raster_depth_code(rules, z);
    if st.depth_test != 0 {
        if !sgi_soft_compare(st.depth_func, zq, (t.load_depth.unwrap())(t.opaque, x, y)) {
            if stencil {
                sgi_soft_stencil_op(st, t, x, y, st.stencil_op[1], smax, sref);
            }
            return false;
        }
        if st.depth_mask != 0 {
            (t.store_depth.unwrap())(t.opaque, x, y, zq);
        }
    }
    // A disabled depth test passes (4.1.5).
    if stencil {
        sgi_soft_stencil_op(st, t, x, y, st.stencil_op[2], smax, sref);
    }
    true
}

/// One blend factor (OpenGL 1.2.1, tables 4.1 and 4.2) for component k,
/// red to alpha, from the source s, destination d and constant colour c.
pub fn sgi_soft_blend_factor(factor: u32, k: usize, s: &[f64; 4], d: &[f64; 4], c: &[f32; 4]) -> f64 {
    match factor {
        1 => 1.0,
        SGI_GL_SRC_COLOR => s[k],
        SGI_GL_ONE_MINUS_SRC_COLOR => 1.0 - s[k],
        SGI_GL_DST_COLOR => d[k],
        SGI_GL_ONE_MINUS_DST_COLOR => 1.0 - d[k],
        SGI_GL_SRC_ALPHA => s[3],
        SGI_GL_ONE_MINUS_SRC_ALPHA => 1.0 - s[3],
        SGI_GL_DST_ALPHA => d[3],
        SGI_GL_ONE_MINUS_DST_ALPHA => 1.0 - d[3],
        SGI_GL_SRC_ALPHA_SATURATE => if k == 3 { 1.0 } else { fmin(s[3], 1.0 - d[3]) },
        SGI_GL_CONSTANT_COLOR => c[k] as f64,
        SGI_GL_ONE_MINUS_CONSTANT_COLOR => 1.0 - c[k] as f64,
        SGI_GL_CONSTANT_ALPHA => c[3] as f64,
        SGI_GL_ONE_MINUS_CONSTANT_ALPHA => 1.0 - c[3] as f64,
        _ => 0.0,
    }
}

pub fn sgi_soft_blend_component(st: &SGIGLDrawState, k: usize, src: &[f64; 4], dst: &[f64; 4]) -> f64 {
    let sf = sgi_soft_blend_factor(st.blend_src, k, src, dst, &st.blend_color);
    let df = sgi_soft_blend_factor(st.blend_dst, k, src, dst, &st.blend_color);

    match st.blend_equation {
        SGI_GL_MIN => fmin(src[k], dst[k]),
        SGI_GL_MAX => fmax(src[k], dst[k]),
        SGI_GL_FUNC_SUBTRACT => src[k] * sf - dst[k] * df,
        SGI_GL_FUNC_REVERSE_SUBTRACT => dst[k] * df - src[k] * sf,
        _ => src[k] * sf + dst[k] * df,
    }
}

/// Blend the packed source colour src, with alpha alpha, and the packed
/// destination dst (4.1.6), each component the fixed point of the rules'
/// colour bits taken as a fraction of its largest value. The planes hold
/// no alpha, so the destination's alpha is 1. The result is clamped to
/// [0, 1] by the subsequent colour conversion.
pub fn sgi_soft_blend_rgb(rules: &SGIRasterRules, st: &SGIGLDrawState, src: u32, alpha: f64, dst: u32, out: &mut [f64; 3]) {
    let bits = rules.color_bits as u32;
    let max: f64 = ((1u32 << bits) - 1) as f64;
    let mut s = [0.0f64; 4];
    let mut d = [0.0f64; 4];

    for k in 0..3usize {
        let shift = (2 - k as u32) * bits;

        s[k] = ((src >> shift) & (max as u32)) as f64 / max;
        d[k] = ((dst >> shift) & (max as u32)) as f64 / max;
    }
    s[3] = alpha;
    d[3] = 1.0;
    for k in 0..3usize {
        out[k] = sgi_soft_blend_component(st, k, &s, &d);
    }
}

pub fn sgi_raster_blend_rgb(rules: &SGIRasterRules, st: &SGIGLDrawState, src: u32, alpha: f64, dst: u32) -> u32 {
    let mut out = [0.0f64; 3];

    let alpha = sgi_soft_alpha(rules, alpha) as f64 / sgi_soft_alpha_max(rules) as f64;
    sgi_soft_blend_rgb(rules, st, src, alpha, dst, &mut out);
    sgi_soft_rgb(rules, &out)
}

pub fn sgi_soft_blend(r: &SGISoftDraw, x: i32, y: i32, src: u32, alpha: f64, dst: u32) -> u32 {
    let mut out = [0.0f64; 3];

    sgi_soft_blend_rgb(&r.rules, &r.st, src, alpha, dst, &mut out);
    sgi_soft_store_rgb(r, x, y, &out)
}

/// glLogicOp's operation (4.1.8, table 4.3) on the source s and the
/// destination d, bit by bit.
pub fn sgi_soft_logic_op(op: u32, s: u32, d: u32) -> u32 {
    sgi_x_logic_op(op.wrapping_sub(SGI_GL_CLEAR), s, d)
}

/// RGB retains its existing arithmetic; spatial alpha dithering is omitted.
pub fn sgi_soft_rgba_value(rules: &SGIRasterRules, rgb: u32, alpha: f64) -> SGIRasterColour {
    let mut c = SGIRasterColour::default();
    let max = (1u32 << rules.color_bits) - 1;

    for k in 0..3usize {
        c.component[k] = (rgb >> ((2 - k as u32) * rules.color_bits as u32)) & max;
    }
    c.component[3] = sgi_soft_stored_alpha(rules, alpha);
    c
}

pub fn sgi_soft_rgba_result(r: &SGISoftDraw, x: i32, y: i32, frag: &SGISoftFrag, dest: &SGIRasterColour) -> SGIRasterColour {
    let mut out = SGIRasterColour::default();
    let rules = &*r.rules;
    let max = (1u32 << rules.color_bits) - 1;
    let alpha_max = sgi_soft_alpha_max(rules);
    let alpha_q = sgi_soft_alpha(rules, frag.rgba[3]);
    let alpha = alpha_q as f64 / alpha_max as f64;
    let rgb = if r.blend { sgi_soft_rgb(rules, &frag.rgba) } else { sgi_soft_store_rgb(r, x, y, &frag.rgba) };

    if !r.blend && !r.logic_op && r.rgba_mask == sgi_soft_rgba_mask(rules, SGI_GL_COLOUR_MASK_ALL) {
        // sgi_soft_rgba_value()'s stored alpha, sgi_soft_alpha() of
        // alpha_q / alpha_max scaled to the stored bits, without the round
        // trip: alpha_q / alpha_max * alpha_max + 0.5 truncates to alpha_q
        // for every alpha_q up to 1023.
        let stored = (1u32.wrapping_shl(rules.alpha_bits as u32)).wrapping_sub(1);

        for k in 0..3usize {
            out.component[k] = (rgb >> ((2 - k as u32) * rules.color_bits as u32)) & max;
        }
        out.component[3] = if stored == alpha_max { alpha_q } else { alpha_q.wrapping_mul(stored) / alpha_max };
        return out;
    }
    out = sgi_soft_rgba_value(rules, rgb, alpha);
    if r.blend {
        let mut src = [0.0f64; 4];
        let mut dst = [0.0f64; 4];
        let mut blended = [0.0f64; 4];

        for k in 0..3usize {
            src[k] = out.component[k] as f64 / max as f64;
            dst[k] = dest.component[k] as f64 / max as f64;
        }
        src[3] = alpha;
        dst[3] = if rules.alpha_bits != 0 {
            dest.component[3] as f64 / ((1u32 << rules.alpha_bits) - 1) as f64
        } else {
            1.0
        };
        for k in 0..4usize {
            blended[k] = sgi_soft_blend_component(&r.st, k, &src, &dst);
        }
        out = sgi_soft_rgba_value(rules, sgi_soft_store_rgb(r, x, y, &blended), blended[3]);
    }
    for k in 0..4usize {
        let bits = if k == 3 { rules.alpha_bits } else { rules.color_bits } as u32;

        if r.logic_op {
            out.component[k] = sgi_soft_logic_op(r.st.logic_op_mode, out.component[k], dest.component[k])
                & (1u32.wrapping_shl(bits)).wrapping_sub(1);
        }
        if r.rgba_mask & (1u32 << k) == 0 {
            out.component[k] = dest.component[k];
        }
    }
    out
}

/// The bits of a packed colour of the rules' colour bits that a
/// glColorMask channel mask lets through. The planes hold no alpha, so
/// its bit has no effect.
pub fn sgi_raster_colour_mask(r: &SGIRasterRules, mask: u32) -> u32 {
    let channel = (1u32 << r.color_bits) - 1;
    let mut out = 0u32;

    for k in 0..3u32 {
        if mask & (SGI_GL_COLOUR_MASK_RED << k) != 0 {
            out |= channel << ((2 - k) * r.color_bits as u32);
        }
    }
    out
}

/// The packed colour's bits, all of which a full mask writes.
pub fn sgi_soft_colour_bits(r: &SGIRasterRules) -> u32 {
    sgi_raster_colour_mask(r, SGI_GL_COLOUR_MASK_ALL)
}

/// Which of the operations that read the stored colour a draw state
/// enables. The logical operation applies to a colour index while
/// GL_INDEX_LOGIC_OP is enabled, and to an RGBA colour while
/// GL_COLOR_LOGIC_OP is, or while blending is enabled with
/// EXT_blend_logic_op's GL_LOGIC_OP equation; it replaces blending
/// (4.1.8). A colour mask with some channels clear keeps the stored
/// bits of those (4.2.2). Planes that cannot be read draw as if the
/// operations were disabled and the mask were all ones, logged once for
/// the draw. An RGBA draw is dithered while GL_DITHER is enabled under
/// rules that name a dither; a colour index never is.
pub fn sgi_soft_setup_ops(r: &mut SGISoftDraw) -> bool {
    let st = r.st.get();

    if st.index_bits != 0 {
        r.blend = false;
        r.logic_op = st.index_logic_op != 0;
    } else {
        r.logic_op = st.color_logic_op != 0 || (st.blend != 0 && st.blend_equation == SGI_GL_LOGIC_OP);
        r.blend = st.blend != 0 && !r.logic_op;
    }
    r.colour_mask = sgi_raster_colour_mask(&r.rules, st.colour_mask);
    r.dither = st.dither != 0 && r.rules.dither as u32 != SGI_DITHER_NONE;
    if st.index_bits == 0 {
        r.rgba_mask = sgi_soft_rgba_mask(&r.rules, st.colour_mask);
        r.rgba_read = r.ntargets != 0 && r.rgba_mask != 0 &&
            (r.blend || r.logic_op ||
             r.rgba_mask != sgi_soft_rgba_mask(&r.rules, SGI_GL_COLOUR_MASK_ALL));
        if !sgi_soft_rgba_capable(&r.rules, &r.t, r.rgba_read) {
            return false;
        }
        if r.t.store_rgba.is_some() {
            return true;
        }
    }
    if (r.blend || r.logic_op ||
        (st.index_bits == 0 && r.colour_mask != 0 && r.colour_mask != sgi_soft_colour_bits(&r.rules))) &&
        r.t.load_colour.is_none()
    {
        crate::qlog!("sgi-gl: blending, logical operations and partial colour masks need planes that can be read back; drawn without them");
        r.blend = false;
        r.logic_op = false;
        r.colour_mask = sgi_soft_colour_bits(&r.rules);
    }
    true
}

/// Runtime errors are observable without logging once for every pixel.
pub fn sgi_soft_texture_report(r: &SGISoftDraw, reason: u32, message: &str) {
    // Bands of one draw share the reports (sgi_soft_bands_run()).
    let rep = r.texture_reports.get();
    let old = *rep;
    *rep |= reason;
    if old & reason == 0 {
        crate::qlog!("sgi-gl: texture {}", message);
    }
}

pub fn sgi_soft_texture_format(format: u32) -> bool {
    format == SGI_GL_ALPHA || format == SGI_GL_LUMINANCE ||
        format == SGI_GL_LUMINANCE_ALPHA || format == SGI_GL_INTENSITY ||
        format == SGI_GL_RGB || format == SGI_GL_RGBA ||
        format == SGI_GL_DEPTH_COMPONENT
}

pub fn sgi_soft_texture_wrap(wrap: u32) -> bool {
    wrap == SGI_GL_REPEAT || wrap == SGI_GL_CLAMP ||
        wrap == SGI_GL_CLAMP_TO_EDGE || wrap == SGI_GL_CLAMP_TO_BORDER
}

/// OpenGL 1.2.1 table 3.16: exact identities remain distinct in a chain.
pub fn sgi_soft_texture_internal_format(format: u32) -> u32 {
    match format {
        SGI_GL_DEPTH_COMPONENT16_SGIX | SGI_GL_DEPTH_COMPONENT24_SGIX | SGI_GL_DEPTH_COMPONENT32_SGIX => SGI_GL_DEPTH_COMPONENT,
        SGI_GL_ALPHA4 | SGI_GL_ALPHA8 | SGI_GL_ALPHA12 | SGI_GL_ALPHA16 => SGI_GL_ALPHA,
        SGI_GL_LUMINANCE4 | SGI_GL_LUMINANCE8 | SGI_GL_LUMINANCE12 | SGI_GL_LUMINANCE16 => SGI_GL_LUMINANCE,
        SGI_GL_LUMINANCE4_ALPHA4 | SGI_GL_LUMINANCE6_ALPHA2 | SGI_GL_LUMINANCE8_ALPHA8 |
        SGI_GL_LUMINANCE12_ALPHA4 | SGI_GL_LUMINANCE12_ALPHA12 | SGI_GL_LUMINANCE16_ALPHA16 => SGI_GL_LUMINANCE_ALPHA,
        SGI_GL_INTENSITY4 | SGI_GL_INTENSITY8 | SGI_GL_INTENSITY12 | SGI_GL_INTENSITY16 => SGI_GL_INTENSITY,
        SGI_GL_R3_G3_B2 | SGI_GL_RGB4 | SGI_GL_RGB5 | SGI_GL_RGB8 | SGI_GL_RGB10 | SGI_GL_RGB12 | SGI_GL_RGB16 => SGI_GL_RGB,
        SGI_GL_RGBA2 | SGI_GL_RGBA4 | SGI_GL_RGB5_A1 | SGI_GL_RGBA8 | SGI_GL_RGB10_A2 | SGI_GL_RGBA12 | SGI_GL_RGBA16 => SGI_GL_RGBA,
        _ => if sgi_soft_texture_format(format) { format } else { 0 },
    }
}

pub fn sgi_soft_texture_mipmap(filter: u32) -> bool {
    filter >= SGI_GL_NEAREST_MIPMAP_NEAREST && filter <= SGI_GL_LINEAR_MIPMAP_LINEAR
}

pub fn sgi_soft_texture_sharpen(filter: u32) -> bool {
    filter == SGI_GL_LINEAR_SHARPEN_SGIS || filter == SGI_GL_LINEAR_SHARPEN_ALPHA_SGIS ||
        filter == SGI_GL_LINEAR_SHARPEN_COLOR_SGIS
}

pub fn sgi_soft_texture_detail(filter: u32) -> bool {
    filter == SGI_GL_LINEAR_DETAIL_SGIS || filter == SGI_GL_LINEAR_DETAIL_ALPHA_SGIS ||
        filter == SGI_GL_LINEAR_DETAIL_COLOR_SGIS
}

pub fn sgi_soft_texture_dimensions(w: u32, h: u32) -> bool {
    w != 0 && h != 0 && w <= i32::MAX as u32 && h <= i32::MAX as u32 &&
        is_power_of_2(w as u64) && is_power_of_2(h as u64)
}

pub fn sgi_soft_texture_component_mask(format: u32) -> u8 {
    match format {
        SGI_GL_ALPHA => 1u8 << 3,
        SGI_GL_LUMINANCE | SGI_GL_INTENSITY => 1,
        SGI_GL_LUMINANCE_ALPHA => (1 << 0) | (1 << 3),
        SGI_GL_RGB => 7,
        SGI_GL_RGBA => 15,
        _ => 0,
    }
}

pub fn sgi_soft_texture_component_bits_valid(format: u32, bits: &[u8; 4]) -> bool {
    let mask = sgi_soft_texture_component_mask(format);

    if mask == 0 {
        return false;
    }
    for k in 0..4usize {
        if (mask & (1u8 << k)) != 0 && (bits[k] == 0 || bits[k] > 16) {
            return false;
        }
    }
    true
}

/// p->levels[i].
pub fn tex_level_at<'a>(p: &SGIGLTexPyramid, i: usize) -> &'a SGIGLTexLevel {
    unsafe { &*p.levels.0.add(i) }
}

pub fn sgi_soft_texture_level<'a>(p: &SGIGLTexPyramid, level: u32) -> Option<&'a SGIGLTexLevel> {
    for i in 0..p.count as usize {
        let l = tex_level_at(p, i);
        if l.level == level {
            return Some(l);
        }
    }
    None
}

pub fn sgi_soft_texture_border_supported(im: &SGIGLTexImage, level: Option<&SGIGLTexLevel>) -> bool {
    let border = match level { Some(l) => l.border, None => im.border };
    let callback = if im.volume {
        match level {
            Some(l) => l.read_border_texel_3d.is_some() || l.read_border_texel_3d_components.is_some(),
            None => im.read_border_texel_3d.is_some() || im.read_border_texel_3d_components.is_some(),
        }
    } else {
        match level {
            Some(l) => l.read_border_texel.is_some() || l.read_border_texel_components.is_some(),
            None => im.read_border_texel.is_some() || im.read_border_texel_components.is_some(),
        }
    };

    border <= 1 && (border == 0 || callback)
}

pub fn sgi_soft_texture_shadow_supported(im: &SGIGLTexImage, format: u32) -> bool {
    if (im.shadow_compare != 0 &&
        im.shadow_compare != SGI_GL_TEXTURE_LEQUAL_R_SGIX &&
        im.shadow_compare != SGI_GL_TEXTURE_GEQUAL_R_SGIX) ||
        (im.shadow_compare != 0 &&
         (!im.shadow_ambient.is_finite() || im.shadow_ambient < 0.0 || im.shadow_ambient > 1.0))
    {
        return false;
    }
    // SGIX_depth_texture alone does not define a texture environment.
    format != SGI_GL_DEPTH_COMPONENT ||
        (im.shadow_compare != 0 && !sgi_soft_texture_sharpen(im.mag_filter) &&
         !sgi_soft_texture_detail(im.mag_filter))
}

pub fn sgi_soft_texture_pyramid(r: &mut SGISoftDraw, im: &SGIGLTexImage, report: bool) -> bool {
    let p = im.pyramid.get();
    let mipmap = sgi_soft_texture_mipmap(im.min_filter);
    let mut border_finite = true;

    #[derive(PartialEq)]
    enum Out { Unsupported, Incomplete, Done }
    let res = (|| -> Out {
        for k in 0..4usize {
            border_finite &= im.border_color[k].is_finite();
        }
        if p.count > SGI_GL_TEX_LEVELS_MAX || (p.count != 0 && p.levels.is_null()) ||
            !p.min_lod.is_finite() || !p.max_lod.is_finite() ||
            p.min_lod > p.max_lod || !border_finite ||
            !sgi_soft_texture_wrap(im.wrap_s) ||
            (!im.one_dimensional && !sgi_soft_texture_wrap(im.wrap_t)) ||
            (im.volume && !sgi_soft_texture_wrap(im.wrap_r)) ||
            (im.min_filter != SGI_GL_NEAREST && im.min_filter != SGI_GL_LINEAR && !mipmap) ||
            (sgi_soft_texture_detail(im.mag_filter) && !im.detail_image.is_null() &&
             (im.detail_image.read_texel_components.is_some() ||
              im.detail_image.read_border_texel_components.is_some()) &&
             !sgi_soft_texture_component_bits_valid(im.detail_image.base_format, &im.detail_image.component_bits)) ||
            (im.mag_filter != SGI_GL_NEAREST && im.mag_filter != SGI_GL_LINEAR &&
             !sgi_soft_texture_sharpen(im.mag_filter) && !sgi_soft_texture_detail(im.mag_filter))
        {
            return Out::Unsupported;
        }
        for i in 1..p.count as usize {
            if tex_level_at(p, i - 1).level >= tex_level_at(p, i).level {
                return Out::Unsupported;
            }
        }
        if mipmap && p.max_level < p.base_level {
            return Out::Incomplete;
        }
        let base = match sgi_soft_texture_level(p, p.base_level) {
            Some(b) => b,
            None => return Out::Incomplete,
        };
        if !sgi_soft_texture_shadow_supported(im, base.base_format) ||
            !sgi_soft_texture_border_supported(im, Some(base)) ||
            sgi_soft_texture_internal_format(base.internal_format) != base.base_format ||
            !sgi_soft_texture_format(base.base_format) ||
            ((base.read_texel_components.is_some() || base.read_texel_3d_components.is_some() ||
              base.read_border_texel_components.is_some() ||
              base.read_border_texel_3d_components.is_some()) &&
             !sgi_soft_texture_component_bits_valid(base.base_format, &base.component_bits)) ||
            (r.st.texture_env_mode == SGI_GL_DECAL &&
             base.base_format != SGI_GL_RGB && base.base_format != SGI_GL_RGBA)
        {
            return Out::Unsupported;
        }
        if im.one_dimensional && base.height != 1 {
            return Out::Unsupported;
        }
        if !sgi_soft_texture_dimensions(base.width, base.height) ||
            (im.volume && (base.depth == 0 || base.depth > i32::MAX as u32 || !is_power_of_2(base.depth as u64)))
        {
            return Out::Incomplete;
        }
        let mut last = p.base_level;
        if mipmap {
            let mut extent = std::cmp::max(base.width, base.height);

            if im.volume {
                extent = std::cmp::max(extent, base.depth);
            }
            let k = 31 - clz32(extent);
            last = std::cmp::min(p.base_level as u64 + k as u64, p.max_level as u64) as u32;
        }
        // Unsupported relevant forms cannot be concealed by missing images.
        for i in 0..p.count as usize {
            let l = tex_level_at(p, i);

            if l.level < p.base_level || l.level > last {
                continue;
            }
            if im.one_dimensional && l.height != 1 {
                return Out::Unsupported;
            }
            if !sgi_soft_texture_shadow_supported(im, l.base_format) {
                return Out::Unsupported;
            }
            if !sgi_soft_texture_border_supported(im, Some(&*l)) ||
                !sgi_soft_texture_format(l.base_format) ||
                sgi_soft_texture_internal_format(l.internal_format) != l.base_format ||
                ((l.read_texel_components.is_some() || l.read_texel_3d_components.is_some() ||
                  l.read_border_texel_components.is_some() ||
                  l.read_border_texel_3d_components.is_some()) &&
                 !sgi_soft_texture_component_bits_valid(l.base_format, &l.component_bits))
            {
                return Out::Unsupported;
            }
        }
        let mut id = p.base_level as u64;
        while id <= last as u64 {
            let l = sgi_soft_texture_level(p, id as u32);
            let shift = (id - p.base_level as u64) as u32;

            let l = match l { Some(l) => l, None => return Out::Incomplete };
            if (if im.volume {
                !(l.read_texel_3d.is_some() || l.read_texel_3d_components.is_some())
            } else {
                !(l.read_texel.is_some() || l.read_texel_components.is_some())
            }) ||
                (im.volume && l.depth != std::cmp::max(1, base.depth.wrapping_shr(shift))) ||
                l.width != std::cmp::max(1, base.width.wrapping_shr(shift)) ||
                l.height != std::cmp::max(1, base.height.wrapping_shr(shift)) ||
                l.internal_format != base.internal_format ||
                l.border != base.border
            {
                return Out::Incomplete;
            }
            id += 1;
        }
        r.texture = P::from_ref(im);
        r.texture_base = P::from_ref(base);
        r.texture_last = last;
        Out::Done
    })();
    match res {
        Out::Done => true,
        Out::Unsupported => {
            if report {
                crate::qlog!("sgi-gl: unsupported texture state in pyramid, draw dropped");
            }
            false
        }
        Out::Incomplete => {
            if report {
                crate::qlog!("sgi-gl: incomplete texture image pyramid, texturing disabled for draw");
            }
            true
        }
    }
}

/// Incomplete images disable texturing; unsupported states drop the draw.
pub fn sgi_soft_texture_setup(r: &mut SGISoftDraw, report: bool) -> bool {
    let im = r.t.texture.as_opt();
    let st = r.st.get();

    // With several targets enabled the highest dimension wins: 3D, then
    // 2D, then 1D (OpenGL 1.2.1, 3.8.10). The family supplies the winning
    // target's image, so the image is checked against that target only.
    let target: u32 = if st.texture_3d != 0 { 3 } else if st.texture_2d != 0 { 2 } else if st.texture_1d != 0 { 1 } else { 0 };

    if target == 0 || st.index_bits != 0 {
        return true;
    }
    if let Some(im) = im.as_deref() {
        if (im.one_dimensional && im.volume) ||
            (target == 1) != im.one_dimensional ||
            (im.one_dimensional && im.pyramid.is_null() && im.height != 1)
        {
            if report {
                crate::qlog!("sgi-gl: incompatible 1D texture image, draw dropped");
            }
            return false;
        }
    }
    // A volume image under the 2D enable alone stays accepted, as before
    // targets were ranked. With 1D enabled as well the 2D binding wins,
    // and a volume is not a 2D image.
    if target == 2 && im.as_deref().map_or(false, |i| i.volume) && st.texture_1d != 0 {
        if report {
            crate::qlog!("sgi-gl: volume texture image for a 2D target, draw dropped");
        }
        return false;
    }
    if target == 3 && im.as_deref().map_or(false, |i| !i.volume) {
        if report {
            crate::qlog!("sgi-gl: 3D texturing requires a volume image, draw dropped");
        }
        return false;
    }
    if st.texture_env_mode != SGI_GL_REPLACE && st.texture_env_mode != SGI_GL_MODULATE &&
        st.texture_env_mode != SGI_GL_DECAL && st.texture_env_mode != SGI_GL_BLEND
    {
        if report {
            crate::qlog!("sgi-gl: unsupported texture state environment {:#x}, draw dropped", st.texture_env_mode);
        }
        return false;
    }
    if let Some(im) = im.as_deref() {
        if im.max_clamp &&
            !(im.max_clamp_s >= 0.0 && im.max_clamp_s <= 1.0 &&
              im.max_clamp_t >= 0.0 && im.max_clamp_t <= 1.0 &&
              im.max_clamp_r >= 0.0 && im.max_clamp_r <= 1.0)
        {
            if report {
                crate::qlog!("sgi-gl: invalid coordinate clamp, draw dropped");
            }
            return false;
        }
        if !im.lod_scale_s.is_finite() || im.lod_scale_s < 0.0 ||
            (!im.one_dimensional && (!im.lod_scale_t.is_finite() || im.lod_scale_t < 0.0)) ||
            (im.volume && (!im.lod_scale_r.is_finite() || im.lod_scale_r < 0.0))
        {
            if report {
                crate::qlog!("sgi-gl: invalid derivative scales, draw dropped");
            }
            return false;
        }
        if !im.pyramid.is_null() {
            return sgi_soft_texture_pyramid(r, im, report);
        }
        let mut border_finite = true;

        for k in 0..4usize {
            border_finite &= im.border_color[k].is_finite();
        }
        if !sgi_soft_texture_shadow_supported(im, im.base_format) ||
            !sgi_soft_texture_border_supported(im, None) ||
            !border_finite ||
            !sgi_soft_texture_format(im.base_format) ||
            ((im.read_texel_components.is_some() || im.read_texel_3d_components.is_some() ||
              im.read_border_texel_components.is_some() ||
              im.read_border_texel_3d_components.is_some()) &&
             !sgi_soft_texture_component_bits_valid(im.base_format, &im.component_bits)) ||
            (sgi_soft_texture_detail(im.mag_filter) && !im.detail_image.is_null() &&
             (im.detail_image.read_texel_components.is_some() ||
              im.detail_image.read_border_texel_components.is_some()) &&
             !sgi_soft_texture_component_bits_valid(im.detail_image.base_format, &im.detail_image.component_bits)) ||
            !sgi_soft_texture_wrap(im.wrap_s) ||
            (!im.one_dimensional && !sgi_soft_texture_wrap(im.wrap_t)) ||
            (im.volume && !sgi_soft_texture_wrap(im.wrap_r)) ||
            im.min_filter != im.mag_filter ||
            (im.min_filter != SGI_GL_NEAREST && im.min_filter != SGI_GL_LINEAR) ||
            (st.texture_env_mode == SGI_GL_DECAL && im.base_format != SGI_GL_RGB && im.base_format != SGI_GL_RGBA)
        {
            if report {
                crate::qlog!(
                    "sgi-gl: unsupported texture state format {:#x} border {} filters {:#x}/{:#x} wraps {:#x}/{:#x}/{:#x} environment {:#x}{}, draw dropped",
                    im.base_format, im.border, im.min_filter, im.mag_filter, im.wrap_s, im.wrap_t,
                    im.wrap_r, st.texture_env_mode,
                    if border_finite { "" } else { " nonfinite border colour" }
                );
            }
            return false;
        }
    }
    let incomplete = match im.as_deref() {
        None => true,
        Some(im) => {
            (if im.volume {
                !(im.read_texel_3d.is_some() || im.read_texel_3d_components.is_some())
            } else {
                !(im.read_texel.is_some() || im.read_texel_components.is_some())
            }) ||
                im.width == 0 || im.height == 0 ||
                im.width > i32::MAX as u32 || im.height > i32::MAX as u32 ||
                !is_power_of_2(im.width as u64) || !is_power_of_2(im.height as u64) ||
                (im.volume && (im.depth == 0 || im.depth > i32::MAX as u32 || !is_power_of_2(im.depth as u64)))
        }
    };
    if incomplete {
        if report {
            crate::qlog!("sgi-gl: incomplete texture image, texturing disabled for draw");
        }
        return true;
    }
    r.texture = P::from_ref(im.as_deref().unwrap());
    true
}

/// Whether a clipped vertex has a clip w and the texture coordinates im
/// uses: s and q always, t unless the image is one-dimensional, and r for
/// a shadow comparison against a depth image.
pub fn sgi_soft_texcoord_valid(im: &SGIGLTexImage, base: Option<&SGIGLTexLevel>, v: &SGIVolumeVertex) -> bool {
    let mut valid = v.clip[3].is_finite() && v.clip[3] > 0.0 && v.attr[SGI_GL_V_TEXCOORD + 3] > 0.0;

    for k in 0..4usize {
        if !im.one_dimensional || k == 0 || k == 3 ||
            (k == 2 && im.shadow_compare != 0 &&
             base.map_or(im.base_format, |b| b.base_format) == SGI_GL_DEPTH_COMPONENT)
        {
            valid &= v.attr[SGI_GL_V_TEXCOORD + k].is_finite();
        }
    }
    valid
}

pub fn sgi_soft_texture_vertex_valid(r: &SGISoftDraw, v: &SGIVolumeVertex) -> bool {
    if r.texture.is_null() {
        return true;
    }
    let valid = sgi_soft_texcoord_valid(&r.texture, r.texture_base.as_opt().map(|b| &*b), v);
    if !valid {
        sgi_soft_texture_report(r, 4, "invalid clipped coordinates, primitive dropped");
    }
    valid
}

/// Meaningful base components, including the border, before filtering.
pub fn sgi_soft_texture_components(format: u32, v: &mut [f64; 4]) {
    match format {
        SGI_GL_ALPHA => {
            v[0] = 1.0;
            v[1] = 1.0;
            v[2] = 1.0;
        }
        SGI_GL_LUMINANCE => {
            v[3] = 1.0;
            v[1] = v[0];
            v[2] = v[0];
        }
        SGI_GL_LUMINANCE_ALPHA => {
            v[1] = v[0];
            v[2] = v[0];
        }
        SGI_GL_INTENSITY => {
            v[1] = v[0];
            v[2] = v[0];
            v[3] = v[0];
        }
        SGI_GL_RGB => {
            v[3] = 1.0;
        }
        _ => {}
    }
}

pub fn sgi_soft_texture_shadow(im: &SGIGLTexImage, depth: f64, reference: f64, out: &mut [f64; 4]) {
    let pass = if im.shadow_compare == SGI_GL_TEXTURE_LEQUAL_R_SGIX { depth <= reference } else { depth >= reference };

    let v = if pass { 1.0 } else { im.shadow_ambient };
    out[0] = v;
    out[1] = v;
    out[2] = v;
    out[3] = 1.0;
}

/// GL_REPEAT's wrap of a texel index into [0, n). Image and level sizes are
/// powers of two wherever setup admits them, and there the mask gives the
/// remainder without two 64-bit divisions a tap.
pub fn sgi_soft_texture_repeat(i: i64, n: u32) -> i64 {
    if is_power_of_2(n as u64) {
        i & (n as i64 - 1)
    } else {
        (i % n as i64 + n as i64) % n as i64
    }
}

/// An 8-bit channel normalized, k / 255.0 for each byte k.
pub fn sgi_soft_unorm8(k: u32) -> f64 {
    k as f64 / 255.0
}

pub const SGI_SOFT_TEXEL_CACHE_SIZE: usize = 4096;
pub const SGI_SOFT_FIXED_LEVELS: usize = 14;
pub const SGI_SOFT_FIXED_FAILED: u32 = 0x80000000;

/// The fixed-point sampler's texels of one level for one draw, by interior
/// index: each the read tag in the high word and the packed RGBA8 texel,
/// components expanded, in the low (sgi_soft_fixed_texel()).
#[derive(Clone, Default)]
pub struct SGISoftFixedStore {
    pub cell: Vec<u64>,
    pub size: usize,
    pub plain: Vec<u64>,
    pub plain_size: usize,
}

#[derive(Clone, Copy, Default)]
pub struct SGISoftTexelEntry {
    pub serial: u64,
    /// the level's address, 0 for the image itself
    pub level: usize,
    pub x: u32,
    pub y: u32,
    pub z: u32,
    pub out: [f64; 4],
}

/// The texels one draw has read, by level and interior index, after
/// conversion to components in [0, 1]. A family's image stays unchanged
/// through a synchronous draw (SGIGLTexImage), so a texel read twice in one
/// draw reads the same both times. A draw starts by taking a new serial,
/// which empties the table without clearing it. Border texels, depth
/// texels and failed reads always go to the family.
pub struct SGISoftTexelCache {
    pub serial: u64,
    pub image: P<SGIGLTexImage>,
    /// Set for a fragment that minifies by more than two texels a pixel
    /// without mipmaps.
    pub bypass: bool,
    pub tag: u32,
    pub fixed: Vec<SGISoftFixedStore>,
    pub entry: Vec<SGISoftTexelEntry>,
}

thread_local! {
    pub static sgi_soft_texel_cache: std::cell::RefCell<Option<Box<SGISoftTexelCache>>> =
        const { std::cell::RefCell::new(None) };
}

pub fn sgi_soft_texel_cache_begin(image: &SGIGLTexImage) -> P<SGISoftTexelCache> {
    sgi_soft_texel_cache.with(|cell| {
        let mut slot = cell.borrow_mut();
        if slot.is_none() {
            *slot = Some(Box::new(SGISoftTexelCache {
                serial: 0,
                image: P::null(),
                bypass: false,
                tag: 0,
                fixed: vec![SGISoftFixedStore::default(); SGI_SOFT_FIXED_LEVELS],
                entry: vec![SGISoftTexelEntry::default(); SGI_SOFT_TEXEL_CACHE_SIZE],
            }));
        }
        let c = slot.as_mut().unwrap();
        c.serial += 1;
        c.image = P::from_ref(image);
        c.bypass = false;
        c.tag = (c.serial & !(SGI_SOFT_FIXED_FAILED as u64)) as u32;
        if c.tag == 0 {
            // Tags have come round: cells from 2^31 draws ago would match.
            for i in 0..SGI_SOFT_FIXED_LEVELS {
                for v in c.fixed[i].cell.iter_mut() {
                    *v = 0;
                }
            }
            c.serial += 1;
            c.tag = (c.serial & !(SGI_SOFT_FIXED_FAILED as u64)) as u32;
        }
        P::new(&mut **c)
    })
}

pub fn sgi_soft_texel_slot(c: &SGISoftTexelCache, level: usize, x: u32, y: u32, z: u32) -> usize {
    let hash: u32 = x.wrapping_mul(0x9e3779b1)
        ^ y.wrapping_mul(0x85ebca77)
        ^ z.wrapping_mul(0xc2b2ae3d)
        ^ ((level >> 4) as u32).wrapping_mul(0x27d4eb2f);

    ((hash ^ (hash >> 15)) as usize) & (SGI_SOFT_TEXEL_CACHE_SIZE - 1)
}

/// None is the C's read_failure exit.
pub fn sgi_soft_texture_tap_inner(
    r: &SGISoftDraw,
    level: Option<&SGIGLTexLevel>,
    mut x: i64,
    mut y: i64,
    mut z: i64,
    reference: f64,
    out: &mut [f64; 4],
) -> Option<bool> {
    let im: &SGIGLTexImage = &r.texture;
    let width = level.map_or(im.width, |l| l.width);
    let height = level.map_or(im.height, |l| l.height);
    let depth = if im.volume { level.map_or(im.depth, |l| l.depth) } else { 1 };
    let format = level.map_or(im.base_format, |l| l.base_format);
    let mut rgba: u32 = 0;
    let mut component = [0u16; 4];
    let mut raw_components = false;
    let mut slot: Option<usize> = None;
    let level_addr = level.map_or(0usize, |l| l as *const SGIGLTexLevel as usize);

    if im.wrap_s == SGI_GL_REPEAT {
        x = sgi_soft_texture_repeat(x, width);
    }
    if !im.one_dimensional && im.wrap_t == SGI_GL_REPEAT {
        y = sgi_soft_texture_repeat(y, height);
    }
    if im.volume && im.wrap_r == SGI_GL_REPEAT {
        z = sgi_soft_texture_repeat(z, depth);
    }
    if x < 0 || y < 0 || z < 0 || x >= width as i64 || y >= height as i64 || z >= depth as i64 {
        let border = level.map_or(im.border, |l| l.border);

        if border == 0 {
            for k in 0..4usize {
                out[k] = fmin(fmax(im.border_color[k] as f64, 0.0), 1.0);
            }
            if format == SGI_GL_DEPTH_COMPONENT {
                let o0 = out[0];
                sgi_soft_texture_shadow(im, o0, reference, out);
            } else {
                sgi_soft_texture_components(format, out);
            }
            return Some(true);
        }
        // Signed indices cover the stored faces, edges and corners.
        if x < -1 || y < -1 || x > width as i64 || y > height as i64 ||
            (im.volume && (z < -1 || z > depth as i64))
        {
            return None;
        }
        let read: bool;
        if im.volume {
            raw_components = match level {
                Some(l) => l.read_border_texel_3d_components.is_some(),
                None => im.read_border_texel_3d_components.is_some(),
            };
            read = match level {
                Some(l) => {
                    if raw_components {
                        (l.read_border_texel_3d_components.unwrap())(l.opaque, l.level, x as i32, y as i32, z as i32, &mut component)
                    } else {
                        match l.read_border_texel_3d {
                            Some(f) => f(l.opaque, l.level, x as i32, y as i32, z as i32, &mut rgba),
                            None => false,
                        }
                    }
                }
                None => {
                    if raw_components {
                        (im.read_border_texel_3d_components.unwrap())(im.opaque, x as i32, y as i32, z as i32, &mut component)
                    } else {
                        match im.read_border_texel_3d {
                            Some(f) => f(im.opaque, x as i32, y as i32, z as i32, &mut rgba),
                            None => false,
                        }
                    }
                }
            };
        } else {
            raw_components = match level {
                Some(l) => l.read_border_texel_components.is_some(),
                None => im.read_border_texel_components.is_some(),
            };
            read = match level {
                Some(l) => {
                    if raw_components {
                        (l.read_border_texel_components.unwrap())(l.opaque, l.level, x as i32, y as i32, &mut component)
                    } else {
                        match l.read_border_texel {
                            Some(f) => f(l.opaque, l.level, x as i32, y as i32, &mut rgba),
                            None => false,
                        }
                    }
                }
                None => {
                    if raw_components {
                        (im.read_border_texel_components.unwrap())(im.opaque, x as i32, y as i32, &mut component)
                    } else {
                        match im.read_border_texel {
                            Some(f) => f(im.opaque, x as i32, y as i32, &mut rgba),
                            None => false,
                        }
                    }
                }
            };
        }
        if !read {
            return None;
        }
    } else {
        if !r.texels.is_null() && r.texels.image == P::from_ref(im) && !r.texels.bypass &&
            format != SGI_GL_DEPTH_COMPONENT
        {
            let c = r.texels.get();
            let idx = sgi_soft_texel_slot(c, level_addr, x as u32, y as u32, z as u32);
            let e = &c.entry[idx];
            if e.serial == c.serial && e.level == level_addr &&
                e.x as i64 == x && e.y as i64 == y && e.z as i64 == z
            {
                *out = e.out;
                return Some(true);
            }
            slot = Some(idx);
        }
        let read: bool;
        if im.volume {
            raw_components = match level {
                Some(l) => l.read_texel_3d_components.is_some(),
                None => im.read_texel_3d_components.is_some(),
            };
            read = match level {
                Some(l) => {
                    if raw_components {
                        (l.read_texel_3d_components.unwrap())(l.opaque, l.level, x as u32, y as u32, z as u32, &mut component)
                    } else {
                        (l.read_texel_3d.unwrap())(l.opaque, l.level, x as u32, y as u32, z as u32, &mut rgba)
                    }
                }
                None => {
                    if raw_components {
                        (im.read_texel_3d_components.unwrap())(im.opaque, x as u32, y as u32, z as u32, &mut component)
                    } else {
                        (im.read_texel_3d.unwrap())(im.opaque, x as u32, y as u32, z as u32, &mut rgba)
                    }
                }
            };
        } else {
            raw_components = match level {
                Some(l) => l.read_texel_components.is_some(),
                None => im.read_texel_components.is_some(),
            };
            read = match level {
                Some(l) => {
                    if raw_components {
                        (l.read_texel_components.unwrap())(l.opaque, l.level, x as u32, y as u32, &mut component)
                    } else {
                        (l.read_texel.unwrap())(l.opaque, l.level, x as u32, y as u32, &mut rgba)
                    }
                }
                None => {
                    if raw_components {
                        (im.read_texel_components.unwrap())(im.opaque, x as u32, y as u32, &mut component)
                    } else {
                        (im.read_texel.unwrap())(im.opaque, x as u32, y as u32, &mut rgba)
                    }
                }
            };
        }

        if !read {
            return None;
        }
    }
    if format == SGI_GL_DEPTH_COMPONENT {
        // Depth arrives only through the packed callback.
        if raw_components {
            return None;
        }
        sgi_soft_texture_shadow(im, rgba as f64 / u32::MAX as f64, reference, out);
        return Some(true);
    }
    if raw_components {
        let bits = level.map_or(&im.component_bits, |l| &l.component_bits);
        let mask = sgi_soft_texture_component_mask(format);

        if !sgi_soft_texture_component_bits_valid(format, bits) {
            return None;
        }
        for k in 0..4usize {
            if mask & (1u8 << k) != 0 {
                let maximum: u32 = (1u32 << bits[k]) - 1;

                if component[k] as u32 > maximum {
                    sgi_soft_texture_report(r, 64, "component outside declared precision, fragment dropped");
                    return Some(false);
                }
                out[k] = component[k] as f64 / maximum as f64;
            }
        }
    } else {
        for k in 0..4u32 {
            out[k as usize] = sgi_soft_unorm8((rgba >> (24 - k * 8)) & 255);
        }
    }
    sgi_soft_texture_components(format, out);
    if let Some(idx) = slot {
        let c = r.texels.get();
        c.entry[idx] = SGISoftTexelEntry {
            serial: c.serial,
            level: level_addr,
            x: x as u32,
            y: y as u32,
            z: z as u32,
            out: *out,
        };
    }
    Some(true)
}

pub fn sgi_soft_texture_tap(
    r: &SGISoftDraw,
    level: Option<&SGIGLTexLevel>,
    x: i64,
    y: i64,
    z: i64,
    reference: f64,
    out: &mut [f64; 4],
) -> bool {
    match sgi_soft_texture_tap_inner(r, level, x, y, z, reference, out) {
        Some(b) => b,
        None => {
            sgi_soft_texture_report(r, 1, "image-read failure, fragment dropped");
            false
        }
    }
}

/// Reduce finite coordinates before any float-to-integer conversion.
pub fn sgi_soft_texture_axis(x: f64, n: u32, wrap: u32, upper: f64) -> f64 {
    let half = 0.5 / n as f64;

    match wrap {
        SGI_GL_REPEAT => x - x.floor(),
        SGI_GL_CLAMP_TO_EDGE => fmin(fmax(x, half), 1.0 - half),
        SGI_GL_CLAMP_TO_BORDER => fmin(fmax(x, -half), 1.0 + half),
        _ => fmin(fmax(x, 0.0), upper),
    }
}

/// Sampling always reduces coordinates before any integer conversion.
pub fn sgi_soft_texture_sample(
    r: &SGISoftDraw,
    level: Option<&SGIGLTexLevel>,
    filter: u32,
    s: f64,
    t: f64,
    rr: f64,
    texel: &mut [f64; 4],
) -> bool {
    let im: &SGIGLTexImage = &r.texture;
    let width = level.map_or(im.width, |l| l.width);
    let height = level.map_or(im.height, |l| l.height);
    let depth = if im.volume { level.map_or(im.depth, |l| l.depth) } else { 1 };

    let reference = rr;
    let mut rr = rr;

    *texel = [0.0; 4];
    let s = sgi_soft_texture_axis(s, width, im.wrap_s, if im.max_clamp { im.max_clamp_s } else { 1.0 });
    let t = if im.one_dimensional {
        0.0
    } else {
        sgi_soft_texture_axis(t, height, im.wrap_t, if im.max_clamp { im.max_clamp_t } else { 1.0 })
    };
    if im.volume {
        rr = sgi_soft_texture_axis(rr, depth, im.wrap_r, if im.max_clamp { im.max_clamp_r } else { 1.0 });
    }
    if filter == SGI_GL_NEAREST {
        let mut x = (width as f64 * s).floor() as i64;
        let mut y = (height as f64 * t).floor() as i64;
        let mut z = if im.volume { (depth as f64 * rr).floor() as i64 } else { 0 };

        if s == 1.0 && im.wrap_s != SGI_GL_CLAMP_TO_BORDER {
            x = width as i64 - 1;
        }
        if t == 1.0 && im.wrap_t != SGI_GL_CLAMP_TO_BORDER {
            y = height as i64 - 1;
        }
        if rr == 1.0 && im.volume && im.wrap_r != SGI_GL_CLAMP_TO_BORDER {
            z = depth as i64 - 1;
        }
        return sgi_soft_texture_tap(r, level, x, y, z, reference, texel);
    }
    let u = width as f64 * s - 0.5;
    let v = if im.one_dimensional { 0.0 } else { height as f64 * t - 0.5 };
    let x = u.floor() as i64;
    let y = v.floor() as i64;
    let w = depth as f64 * rr - 0.5;
    let z = if im.volume { w.floor() as i64 } else { 0 };
    let a = u - x as f64;
    let b = v - y as f64;
    let c = if im.volume { w - z as f64 } else { 0.0 };

    for k in 0..(if im.volume { 2 } else { 1 }) {
        for j in 0..(if im.one_dimensional { 1 } else { 2 }) {
            for i in 0..2 {
                let weight = (if i != 0 { a } else { 1.0 - a }) * (if j != 0 { b } else { 1.0 - b })
                    * (if im.volume { if k != 0 { c } else { 1.0 - c } } else { 1.0 });
                let mut tap = [0.0f64; 4];

                if weight == 0.0 {
                    continue;
                }
                if !sgi_soft_texture_tap(r, level, x + i as i64, y + j as i64, z + k as i64, reference, &mut tap) {
                    return false;
                }
                for channel in 0..4usize {
                    texel[channel] += weight * tap[channel];
                }
            }
        }
    }
    true
}

#[derive(Clone, Copy, Default)]
pub struct SGISoftTexSelection {
    pub first: u32,
    pub second: u32,
    pub filter: u32,
    pub alpha: f64,
    pub lambda: f64,
}

/// Relative levels avoid rounding a large absolute base in floating point.
pub fn sgi_soft_texture_select(min: u32, mag: u32, p: &SGIGLTexPyramid, last: u32, rho: f64, out: &mut SGISoftTexSelection) -> bool {
    let enhanced = sgi_soft_texture_sharpen(mag) || sgi_soft_texture_detail(mag);
    let c: f64 = if enhanced {
        if min == SGI_GL_NEAREST_MIPMAP_NEAREST || min == SGI_GL_LINEAR_MIPMAP_NEAREST { 0.5 } else { 0.0 }
    } else if mag == SGI_GL_LINEAR && (min == SGI_GL_NEAREST_MIPMAP_NEAREST || min == SGI_GL_NEAREST_MIPMAP_LINEAR) {
        0.5
    } else {
        0.0
    };
    let range: u32 = last.wrapping_sub(p.base_level);

    if !rho.is_finite() || rho < 0.0 {
        return false;
    }
    let mut lambda = if rho != 0.0 { rho.log2() } else { p.min_lod };
    if !lambda.is_finite() {
        return false;
    }
    lambda = fmin(fmax(lambda, p.min_lod), p.max_lod);
    *out = SGISoftTexSelection {
        first: p.base_level,
        second: p.base_level,
        filter: if lambda <= c { mag } else { min },
        alpha: 0.0,
        lambda,
    };
    if lambda <= c || !sgi_soft_texture_mipmap(min) {
        return true;
    }
    out.filter = if min == SGI_GL_NEAREST_MIPMAP_NEAREST || min == SGI_GL_NEAREST_MIPMAP_LINEAR {
        SGI_GL_NEAREST
    } else {
        SGI_GL_LINEAR
    };
    if min == SGI_GL_NEAREST_MIPMAP_NEAREST || min == SGI_GL_LINEAR_MIPMAP_NEAREST {
        let mut relative: f64;

        if lambda <= 0.5 {
            relative = 0.0;
        } else if lambda > range as f64 + 0.5 {
            relative = range as f64;
        } else {
            relative = lambda.floor();
            // Adding 0.5 can round the next value above a half tie onto it.
            relative += (lambda - relative > 0.5) as i32 as f64;
        }
        out.first = out.first.wrapping_add(fmin(fmax(relative, 0.0), range as f64) as u32);
        out.second = out.first;
    } else {
        let relative = fmin(fmax(lambda, 0.0), range as f64);
        let n = relative.floor() as u32;

        out.first = out.first.wrapping_add(n);
        out.second = if out.first == last { last } else { out.first.wrapping_add(1) };
        out.alpha = relative - n as f64;
    }
    true
}

/// Projective gradients precede wrapping; D is Q/W, not geometry W.
pub fn sgi_soft_texture_lod_scale(scale: f64) -> f64 {
    if scale != 0.0 { scale } else { 1.0 }
}

pub fn sgi_soft_texture_derivatives(
    value: &[f64; 4],
    dx: &[f64; 4],
    dy: &[f64; 4],
    ns: f64,
    nt: f64,
    nr: f64,
    out: &mut [f64; 5],
) -> bool {
    let d = value[2];
    let d2 = d * d;

    if !(d > 0.0) || !d2.is_finite() || d2 == 0.0 {
        return false;
    }
    for k in 0..2usize {
        out[2 * k] = (dx[k] * d - value[k] * dx[2]) / d2;
        out[2 * k + 1] = (dy[k] * d - value[k] * dy[2]) / d2;
        if !(value[k] / d).is_finite() || !out[2 * k].is_finite() || !out[2 * k + 1].is_finite() {
            return false;
        }
    }
    out[4] = fmax((ns * out[0]).hypot(nt * out[2]), (ns * out[1]).hypot(nt * out[3]));
    if nr != 0.0 {
        let drdx = (dx[3] * d - value[3] * dx[2]) / d2;
        let drdy = (dy[3] * d - value[3] * dy[2]) / d2;

        if !(value[3] / d).is_finite() || !drdx.is_finite() || !drdy.is_finite() {
            return false;
        }
        out[4] = fmax(
            (ns * out[0]).hypot(nt * out[2]).hypot(nr * drdx),
            (ns * out[1]).hypot(nt * out[3]).hypot(nr * drdy),
        );
    }
    out[4].is_finite()
}

/// The last level selection a thread's draw made, by the footprint it was
/// made for.
#[derive(Clone, Copy, Default)]
pub struct SGISoftTexSelectionMemo {
    pub serial: u64,
    pub rho: u64,
    pub selected: SGISoftTexSelection,
}

thread_local! {
    pub static sgi_soft_texture_selected: std::cell::Cell<SGISoftTexSelectionMemo> =
        std::cell::Cell::new(SGISoftTexSelectionMemo {
            serial: 0,
            rho: 0,
            selected: SGISoftTexSelection { first: 0, second: 0, filter: 0, alpha: 0.0, lambda: 0.0 },
        });
}

pub fn sgi_soft_texture_select_memo(r: &SGISoftDraw, rho: f64, out: &mut SGISoftTexSelection) -> bool {
    let im: &SGIGLTexImage = &r.texture;
    let memo = !r.texels.is_null() && r.texels.image == P::from_ref(im);
    let bits = rho.to_bits();

    if memo {
        let m = sgi_soft_texture_selected.with(|c| c.get());
        if m.serial == r.texels.serial && m.rho == bits {
            *out = m.selected;
            return true;
        }
    }
    if !sgi_soft_texture_select(im.min_filter, im.mag_filter, &im.pyramid, r.texture_last, rho, out) {
        return false;
    }
    if memo {
        sgi_soft_texture_selected.with(|c| {
            c.set(SGISoftTexSelectionMemo { serial: r.texels.serial, rho: bits, selected: *out })
        });
    }
    true
}

pub fn sgi_soft_texture_mix(value: &mut [f64; 4], other: &[f64; 4], alpha: f64) {
    for k in 0..4usize {
        value[k] = (1.0 - alpha) * value[k] + alpha * other[k];
    }
}

pub fn sgi_soft_texture_weight(r: &SGISoftDraw, lambda: f64, weight: &mut f64) -> bool {
    let im: &SGIGLTexImage = &r.texture;

    *weight = fmin(fmax(-lambda / 4.0, 0.0), 1.0);
    if let Some(f) = im.read_filter_weight {
        if !f(im.opaque, lambda, weight) || !weight.is_finite() {
            sgi_soft_texture_report(r, 16, "invalid filter weight, fragment dropped");
            return false;
        }
    }
    true
}

pub fn sgi_soft_texture_sharpen_sample(
    r: &SGISoftDraw,
    filter: u32,
    lambda: f64,
    s: f64,
    t: f64,
    rr: f64,
    texel: &mut [f64; 4],
) -> bool {
    let im: &SGIGLTexImage = &r.texture;
    let p: &SGIGLTexPyramid = &im.pyramid;
    let base: &SGIGLTexLevel = &r.texture_base;
    let mut extent = std::cmp::max(base.width, base.height);
    let mut other = [0.0f64; 4];
    let mut weight = 0.0f64;

    if !sgi_soft_texture_sample(r, Some(base), SGI_GL_LINEAR, s, t, rr, texel) {
        return false;
    }
    if im.volume {
        extent = std::cmp::max(extent, base.depth);
    }
    let coarse = if p.base_level < u32::MAX { sgi_soft_texture_level(p, p.base_level + 1) } else { None };
    let coarse = match coarse {
        Some(c) if !(extent <= 1 || p.max_level <= p.base_level ||
            c.width != std::cmp::max(1, base.width / 2) ||
            c.height != std::cmp::max(1, base.height / 2) ||
            (im.volume && c.depth != std::cmp::max(1, base.depth / 2)) ||
            c.internal_format != base.internal_format ||
            c.base_format != base.base_format ||
            c.border != base.border ||
            !sgi_soft_texture_border_supported(im, Some(c)) ||
            (if im.volume { c.read_texel_3d.is_none() } else { c.read_texel.is_none() })) => c,
        _ => return true,
    };
    if !sgi_soft_texture_weight(r, lambda, &mut weight) ||
        !sgi_soft_texture_sample(r, Some(coarse), SGI_GL_LINEAR, s, t, rr, &mut other)
    {
        return false;
    }
    for k in 0..4usize {
        if (k == 3 && filter == SGI_GL_LINEAR_SHARPEN_COLOR_SGIS) ||
            (k != 3 && filter == SGI_GL_LINEAR_SHARPEN_ALPHA_SGIS)
        {
            continue;
        }
        texel[k] = fmin(fmax(texel[k] + weight * (texel[k] - other[k]), 0.0), 1.0);
    }
    true
}

/// C scalbn(): x * 2^n, exact.
pub fn scalbn(x: f64, n: i32) -> f64 {
    if x == 0.0 || !x.is_finite() {
        return x;
    }
    let mut x = x;
    let mut n = n.clamp(-3000, 3000);
    while n > 1000 {
        x *= 2f64.powi(1000);
        n -= 1000;
    }
    while n < -1000 {
        x *= 2f64.powi(-1000);
        n += 1000;
    }
    x * 2f64.powi(n)
}

pub fn sgi_soft_texture_detail_sample(
    r: &SGISoftDraw,
    filter: u32,
    lambda: f64,
    s: f64,
    t: f64,
    rr: f64,
    texel: &mut [f64; 4],
) -> bool {
    let im: &SGIGLTexImage = &r.texture;
    let base: &SGIGLTexLevel = &r.texture_base;
    let mut repeat: SGIGLTexImage = im.clone();
    let mut draw: SGISoftDraw = *r;
    let mut other = [0.0f64; 4];
    let mut weight = 0.0f64;
    let scale: i32 = std::cmp::min(-(im.detail_level as i64), i32::MAX as i64) as i32;

    if !sgi_soft_texture_sample(r, Some(base), SGI_GL_LINEAR, s, t, rr, texel) {
        return false;
    }
    let detail = match im.detail_image.as_opt() {
        Some(d) if !(im.one_dimensional || im.volume || im.pyramid.base_level != 0 ||
            d.level != 0 || d.border != 0 ||
            !(d.read_texel.is_some() || d.read_texel_components.is_some()) ||
            !sgi_soft_texture_dimensions(d.width, d.height) ||
            d.internal_format != base.internal_format ||
            d.base_format != base.base_format || im.detail_level > 0) => d,
        _ => return true,
    };
    if im.detail_mode != SGI_GL_ADD && im.detail_mode != SGI_GL_MODULATE {
        sgi_soft_texture_report(r, 32, "unsupported detail mode, fragment dropped");
        return false;
    }
    // Scaling is in primary texels, independent of the detail dimensions.
    let ds = scalbn(s * base.width as f64 / detail.width as f64, scale);
    let dt = scalbn(t * base.height as f64 / detail.height as f64, scale);
    if !ds.is_finite() || !dt.is_finite() {
        sgi_soft_texture_report(r, 32, "invalid detail coordinates, fragment dropped");
        return false;
    }
    repeat.wrap_s = SGI_GL_REPEAT;
    repeat.wrap_t = SGI_GL_REPEAT;
    draw.texture = P::new(&mut repeat);
    if !sgi_soft_texture_weight(r, lambda, &mut weight) ||
        !sgi_soft_texture_sample(&draw, Some(&*detail), SGI_GL_LINEAR, ds, dt, 0.0, &mut other)
    {
        return false;
    }
    for k in 0..4usize {
        let signed_detail = weight * (2.0 * other[k] - 1.0);

        if (k == 3 && filter == SGI_GL_LINEAR_DETAIL_COLOR_SGIS) ||
            (k != 3 && filter == SGI_GL_LINEAR_DETAIL_ALPHA_SGIS)
        {
            continue;
        }
        texel[k] = if im.detail_mode == SGI_GL_ADD {
            texel[k] + signed_detail
        } else {
            texel[k] * (1.0 + signed_detail)
        };
        texel[k] = fmin(fmax(texel[k], 0.0), 1.0);
    }
    true
}

/// A filtered texel through the image's post-filter, if it has one, and
/// then by the texture environment env into a fragment's colour rgba,
/// for an image of base format format (OpenGL 1.2.1, 3.8.9).
#[inline(always)]
pub fn sgi_soft_texture_apply(r: &SGISoftDraw, format: u32, env: u32, texel: &mut [f64; 4], rgba: &mut [f64; 4]) {
    let im: &SGIGLTexImage = &r.texture;
    let mut format = format;

    if let Some(f) = im.post_filter {
        f(im.opaque, texel);
    }
    if format == SGI_GL_DEPTH_COMPONENT {
        format = SGI_GL_LUMINANCE;
    }
    for k in 0..4usize {
        let alpha = k == 3;

        if (!alpha && format == SGI_GL_ALPHA) ||
            (alpha && (format == SGI_GL_RGB || format == SGI_GL_LUMINANCE))
        {
            continue;
        }
        match env {
            SGI_GL_REPLACE => rgba[k] = texel[k],
            SGI_GL_MODULATE => rgba[k] *= texel[k],
            SGI_GL_DECAL => {
                if !alpha {
                    rgba[k] = if format == SGI_GL_RGB {
                        texel[k]
                    } else {
                        rgba[k] * (1.0 - texel[3]) + texel[k] * texel[3]
                    };
                }
            }
            SGI_GL_BLEND => {
                rgba[k] = if alpha && format != SGI_GL_INTENSITY {
                    rgba[k] * texel[k]
                } else {
                    rgba[k] * (1.0 - texel[k])
                        + fmin(fmax(r.st.texture_env_color[k] as f64, 0.0), 1.0) * texel[k]
                };
            }
            _ => {}
        }
    }
}

pub fn sgi_soft_texture(r: &SGISoftDraw, frag: &mut SGISoftFrag) -> bool {
    let im: &SGIGLTexImage = &r.texture;
    let st: &SGIGLDrawState = &r.st;
    let base = r.texture_base.as_opt().map(|b| &*b);
    let format = base.map_or(im.base_format, |b| b.base_format);
    let mut texel = [0.0f64; 4];
    let mut rr = 0.0f64;
    let q = frag.texcoord[3];

    if !(q > 0.0) || !q.is_finite() {
        sgi_soft_texture_report(r, 2, "invalid projected coordinates, fragment dropped");
        return false;
    }
    let s = frag.texcoord[0] / q;
    let t = if im.one_dimensional { 0.0 } else { frag.texcoord[1] / q };
    if im.volume || (format == SGI_GL_DEPTH_COMPONENT && im.shadow_compare != 0) {
        rr = frag.texcoord[2] / q;
    }
    if !s.is_finite() || !t.is_finite() || !rr.is_finite() ||
        (format == SGI_GL_DEPTH_COMPONENT && (rr < 0.0 || rr > 1.0))
    {
        sgi_soft_texture_report(r, 2, "invalid projected coordinates, fragment dropped");
        return false;
    }
    if let Some(base) = base {
        let mut selected = SGISoftTexSelection::default();
        let mut other = [0.0f64; 4];

        if !sgi_soft_texture_select_memo(r, frag.texture_rho, &mut selected) {
            sgi_soft_texture_report(r, 8, "invalid footprint, fragment dropped");
            return false;
        }
        let mut level = sgi_soft_texture_level(&im.pyramid, selected.first);
        if !r.texels.is_null() {
            r.texels.get().bypass = selected.lambda > 1.0 && !sgi_soft_texture_mipmap(im.min_filter);
        }
        if sgi_soft_texture_sharpen(selected.filter) {
            if !sgi_soft_texture_sharpen_sample(r, selected.filter, selected.lambda, s, t, rr, &mut texel) {
                return false;
            }
        } else if sgi_soft_texture_detail(selected.filter) {
            if !sgi_soft_texture_detail_sample(r, selected.filter, selected.lambda, s, t, rr, &mut texel) {
                return false;
            }
        } else if !sgi_soft_texture_sample(r, level, selected.filter, s, t, rr, &mut texel) {
            return false;
        }
        if selected.alpha != 0.0 && selected.first != selected.second {
            level = sgi_soft_texture_level(&im.pyramid, selected.second);
            if !sgi_soft_texture_sample(r, level, selected.filter, s, t, rr, &mut other) {
                return false;
            }
            sgi_soft_texture_mix(&mut texel, &other, selected.alpha);
        }
        let _ = base;
    } else if !sgi_soft_texture_sample(r, None, im.min_filter, s, t, rr, &mut texel) {
        return false;
    }
    sgi_soft_texture_apply(r, format, st.texture_env_mode, &mut texel, &mut frag.rgba);
    true
}

pub fn sgi_raster_texture_check_draw(draw: &mut SGISoftDraw, report: bool) -> SGIRasterTextureResult {
    let state = draw.st.get();

    if draw.st.is_null() || !sgi_soft_texture_setup(draw, report) {
        return SGI_RASTER_TEXTURE_UNSUPPORTED;
    }
    if !draw.texture.is_null() {
        return SGI_RASTER_TEXTURE_READY;
    }
    if state.index_bits != 0 || (state.texture_1d == 0 && state.texture_2d == 0 && state.texture_3d == 0) {
        return SGI_RASTER_TEXTURE_DISABLED;
    }
    SGI_RASTER_TEXTURE_INCOMPLETE
}

pub fn sgi_raster_texture_check(state: &SGIGLDrawState, texture: &SGIGLTexImage) -> SGIRasterTextureResult {
    let mut target = SGIRasterTarget::default();
    let mut reports: u32 = 0;
    target.texture = P::from_ref(texture);
    let mut draw = SGISoftDraw::default();
    draw.t = P::new(&mut target);
    draw.st = P::from_ref(state);
    draw.texture_reports = P::new(&mut reports);

    sgi_raster_texture_check_draw(&mut draw, false)
}

pub fn sgi_raster_texture_prepare(state: &SGIGLDrawState, texture: &SGIGLTexImage, out: &mut Option<Box<SGIRasterTexture>>) -> SGIRasterTextureResult {
    let mut target = SGIRasterTarget::default();
    let mut reports: u32 = 0;
    target.texture = P::from_ref(texture);
    let mut draw = SGISoftDraw::default();
    draw.t = P::new(&mut target);
    draw.st = P::from_ref(state);
    draw.texture_reports = P::new(&mut reports);

    *out = None;
    let result = sgi_raster_texture_check_draw(&mut draw, true);
    if result != SGI_RASTER_TEXTURE_READY {
        return result;
    }
    let mut sampler = Box::new(SGIRasterTexture {
        target: SGIRasterTarget::default(),
        draw,
        reports: 0,
    });
    sampler.target.texture = P::from_ref(texture);
    sampler.draw.t = P::new(&mut sampler.target);
    sampler.draw.texture_reports = P::new(&mut sampler.reports);
    *out = Some(sampler);
    SGI_RASTER_TEXTURE_READY
}

/// Stop the sampler's reports.
pub fn sgi_raster_texture_quiet(texture: &mut SGIRasterTexture) {
    texture.reports = u32::MAX;
}

pub fn sgi_raster_texture_fragment(texture: &mut SGIRasterTexture, coord: &[f64; 4], color: &mut [f64; 4]) -> bool {
    let mut frag = SGISoftFrag::default();

    frag.texcoord = *coord;
    frag.rgba = *color;
    if !sgi_soft_texture(&texture.draw, &mut frag) {
        return false;
    }
    *color = frag.rgba;
    true
}

pub fn sgi_gl_texture_apply_pixel(state: &SGIGLDrawState, texture: &SGIGLTexImage, coord: &[f32; 4], color: &[f32; 4], out: &mut [f32; 4]) -> bool {
    let mut reports: u32 = 0;
    let mut target = SGIRasterTarget::default();
    target.texture = P::from_ref(texture);
    let mut r = SGISoftDraw::default();
    r.t = P::new(&mut target);
    r.st = P::from_ref(state);
    r.texture_reports = P::new(&mut reports);
    let mut frag = SGISoftFrag::default();

    if !sgi_soft_texture_setup(&mut r, true) || r.texture.is_null() {
        return false;
    }
    for k in 0..4usize {
        frag.texcoord[k] = coord[k] as f64;
        frag.rgba[k] = color[k] as f64;
    }
    if !sgi_soft_texture(&r, &mut frag) {
        return false;
    }
    for k in 0..4usize {
        out[k] = frag.rgba[k] as f32;
    }
    true
}

/// Fog (3.10): the fog factor f from the fragment's fog coordinate c by
/// the mode, clamped to [0, 1], blends an RGBA fragment's red, green and
/// blue toward the fog colour, f C + (1 - f) Cf, and adds (1 - f) times
/// the fog index to a colour index. A family's fog_factor, where set, gives
/// f from the window depth and fog coordinate instead. GL_LINEAR with start
/// equal to end divides by zero; the model takes f as 1 before the end and
/// 0 from it, the limit of the equation from either side.
pub fn sgi_soft_fog(r: &SGISoftDraw, frag: &mut SGISoftFrag, z: f64) {
    let st: &SGIGLDrawState = &r.st;
    let c = frag.fog;
    let mut f: f64;
    let dens = st.fog_density as f64;

    if let Some(ff) = r.t.fog_factor {
        // A family's own curve replaces the mode; the blend is the same.
        f = ff(r.t.opaque, z as f32, c as f32) as f64;
    } else if st.fog_mode == SGI_GL_LINEAR {
        f = if st.fog_end == st.fog_start {
            if c < st.fog_end as f64 { 1.0 } else { 0.0 }
        } else {
            (st.fog_end as f64 - c) / (st.fog_end as f64 - st.fog_start as f64)
        };
    } else if st.fog_mode == SGI_GL_EXP2 {
        f = (-(dens * c) * (dens * c)).exp();
    } else {
        f = (-dens * c).exp();
    }
    f = if f >= 1.0 { 1.0 } else if f > 0.0 { f } else { 0.0 };
    if st.index_bits != 0 {
        frag.index += (1.0 - f) * st.fog_index as f64;
        return;
    }
    for k in 0..3usize {
        frag.rgba[k] = f * frag.rgba[k] + (1.0 - f) * st.fog_color[k] as f64;
    }
}

/// Whether a point whose client clip plane values are d lies outside an
/// enabled plane (2.11).
pub fn sgi_soft_clipped(st: &SGIGLDrawState, d: &[f64]) -> bool {
    for i in 0..SGI_GL_CLIP_PLANES as usize {
        if (st.clip_planes & (1u32 << i)) != 0 && !(d[i] >= 0.0) {
            return true;
        }
    }
    false
}

/// A fragment's colour value from an interpolated or flat index.
pub fn sgi_soft_index(r: &SGISoftDraw, index: f64) -> u32 {
    sgi_gl_index_value(r.st.index_bits, index)
}

pub fn sgi_soft_owned_at(r: &SGISoftDraw, px: i32, py: i32) -> bool {
    let x = r.win.ox.wrapping_add(px);
    let y = r.win.oy.wrapping_add(py);
    let st: &SGIGLDrawState = &r.st;

    (r.win.clip.n <= 1 || sgi_clip_contains(&r.win.clip, x, y)) &&
        (st.scissor_test == 0 ||
         (px >= st.scissor[0] &&
          (px as i64) < st.scissor[0] as i64 + st.scissor[2] as i64 &&
          py >= st.scissor[1] &&
          (py as i64) < st.scissor[1] as i64 + st.scissor[3] as i64))
}

/// The colour of a fragment that has passed every test written to the
/// draw's colour targets at plane pixel (x, y): blended with or combined
/// by the logical operation with what is there, dithered and masked, from
/// dest in RGBA mode, which the caller has read where the draw reads it.
#[inline(always)]
pub fn sgi_soft_fragment_store(r: &SGISoftDraw, x: i32, y: i32, f: &SGISoftFrag, dest: &[SGIRasterColour; SGI_GL_MAX_TARGETS as usize]) {
    let t: &SGIRasterTarget = &r.t;
    let st: &SGIGLDrawState = &r.st;

    if st.index_bits == 0 && t.store_rgba.is_some() {
        let mut i = 0;
        while r.rgba_mask != 0 && i < r.ntargets as usize {
            let out = sgi_soft_rgba_result(r, x, y, f, &dest[i]);

            (t.store_rgba.unwrap())(t.opaque, r.targets[i], x, y, &out);
            i += 1;
        }
        return;
    }
    // Dithering follows blending (4.1.7), so a colour to be blended is
    // rounded and the blend's result dithered.
    let colour = if st.index_bits != 0 {
        sgi_soft_index(r, f.index)
    } else if r.blend {
        sgi_soft_rgb(&r.rules, &f.rgba)
    } else {
        sgi_soft_store_rgb(r, x, y, &f.rgba)
    };
    for i in 0..r.ntargets as usize {
        let mut out = colour;

        if r.logic_op {
            let bits: u32 = if st.index_bits != 0 { st.index_bits } else { 3 * r.rules.color_bits as u32 };

            out = sgi_soft_logic_op(
                st.logic_op_mode,
                colour,
                (t.load_colour.unwrap())(t.opaque, r.targets[i], x, y),
            ) & (if bits >= 32 { u32::MAX } else { (1u32 << bits) - 1 });
        }
        if st.index_bits != 0 {
            (t.store_index.unwrap())(t.opaque, r.targets[i], x, y, out, st.index_mask);
            continue;
        }
        if r.colour_mask == 0 {
            continue;
        }
        if r.blend {
            out = sgi_soft_blend(
                r,
                x,
                y,
                colour,
                sgi_soft_alpha(&r.rules, f.rgba[3]) as f64 / sgi_soft_alpha_max(&r.rules) as f64,
                (t.load_colour.unwrap())(t.opaque, r.targets[i], x, y),
            );
        }
        if r.colour_mask != sgi_soft_colour_bits(&r.rules) {
            out = (out & r.colour_mask) | ((t.load_colour.unwrap())(t.opaque, r.targets[i], x, y) & !r.colour_mask);
        }
        (t.store_colour.unwrap())(t.opaque, r.targets[i], x, y, out);
    }
}

pub fn sgi_soft_fragment_at(r: &SGISoftDraw, px: i32, py: i32, z: f64, frag: &SGISoftFrag, coverage: f64, antialias: bool) {
    let t: &SGIRasterTarget = &r.t;
    let st: &SGIGLDrawState = &r.st;
    let mut fogged = *frag;
    let x = r.win.ox.wrapping_add(px);
    let y = r.win.oy.wrapping_add(py);
    let mut dest = [SGIRasterColour::default(); SGI_GL_MAX_TARGETS as usize];

    if !(z >= 0.0 && z <= 1.0) {
        return;
    }
    if !r.texture.is_null() {
        if frag.fixed {
            let format = match r.texture_base.as_opt() {
                Some(b) => b.base_format,
                None => r.texture.base_format,
            };
            sgi_soft_texture_apply(r, format, st.texture_env_mode, &mut fogged.texel, &mut fogged.rgba);
        } else if !sgi_soft_texture(r, &mut fogged) {
            return;
        }
    }
    if st.fog != 0 {
        sgi_soft_fog(r, &mut fogged, z);
    }
    fogged.rgba[3] *= coverage;
    if antialias && st.index_bits != 0 {
        let bits = std::cmp::min(4, st.index_bits);
        let mask = (1u32 << bits) - 1;
        let index = sgi_soft_index(r, fogged.index);
        let value = std::cmp::min((coverage * (1u32 << bits) as f64).floor() as u32, mask);

        fogged.index = ((index & !mask) | value) as f64;
    }
    if !sgi_soft_owned_at(r, px, py) {
        return;
    }
    // Incoming alpha and reference use at least two bits and at least the
    // stored alpha width, independently of the colour mask (4.1.4, 2.13.9).
    let f = &fogged;
    if st.alpha_test != 0 && st.index_bits == 0 &&
        !sgi_raster_alpha_test(&r.rules, st.alpha_func, f.rgba[3], st.alpha_ref as f64)
    {
        return;
    }
    if st.index_bits == 0 && t.store_rgba.is_some() && r.rgba_read &&
        !sgi_soft_rgba_load(&r.rules, t, &r.targets, r.ntargets, x, y, &mut dest, r.rgba_reported.get())
    {
        return;
    }
    if !sgi_raster_depth_stencil(&r.rules, st, t, x, y, z) {
        return;
    }
    sgi_soft_fragment_store(r, x, y, f, &dest);
}

/// The depth range [n, f] normalized depth is mapped through (2.10.1): the
/// draw state's, or [0, 1] under a window matrix, whose depth is already
/// window depth.
pub fn sgi_soft_depth_range(st: &SGIGLDrawState, n: &mut f64, f: &mut f64) {
    *n = if st.window_matrix_given != 0 { 0.0 } else { st.depth_range[0] as f64 };
    *f = if st.window_matrix_given != 0 { 1.0 } else { st.depth_range[1] as f64 };
}

pub fn sgi_raster_window_depth(st: &SGIGLDrawState, z: f64, offset: f64, window: &mut f64) -> bool {
    let mut n = 0.0;
    let mut f = 0.0;

    if !(z >= 0.0 && z <= 1.0) {
        return false;
    }
    sgi_soft_depth_range(st, &mut n, &mut f);
    let zw = n + (f - n) * z + offset;
    *window = if zw >= 1.0 { 1.0 } else if zw > 0.0 { zw } else { 0.0 };
    true
}

/// The window pixels a primitive's fragments may fall on, inclusive: the
/// bounds of those the window owns, within the viewport unless a window
/// matrix replaces it. False when there are none.
pub fn sgi_soft_bounds(r: &SGISoftDraw, x0: &mut i32, y0: &mut i32, x1: &mut i32, y1: &mut i32) -> bool {
    let st: &SGIGLDrawState = &r.st;
    let win = &r.win;
    let vx = st.viewport[0];
    let vy = st.viewport[1];
    let vw = st.viewport[2];
    let vh = st.viewport[3];

    *x0 = win.x0.wrapping_sub(win.ox);
    *y0 = win.y0.wrapping_sub(win.oy);
    *x1 = win.x1.wrapping_sub(win.ox);
    *y1 = win.y1.wrapping_sub(win.oy);
    if st.window_matrix_given != 0 {
        return true;
    }
    if vw <= 0 || vh <= 0 {
        return false;
    }
    *x0 = std::cmp::max(*x0, vx);
    *y0 = std::cmp::max(*y0, vy);
    *x1 = std::cmp::min(*x1, vx.wrapping_add(vw).wrapping_sub(1));
    *y1 = std::cmp::min(*y1, vy.wrapping_add(vh).wrapping_sub(1));
    true
}

/// Whether a polygon fragment at (px, py) is drawn under the polygon
/// stipple, and in *background whether it takes the target's opaque
/// stipple background (SGIRasterTarget's stipple_opaque).
pub fn sgi_soft_stipple_pass(r: &SGISoftDraw, px: i32, py: i32, background: &mut bool) -> bool {
    *background = false;
    if r.polygon_stipple.is_null() ||
        (unsafe { *r.polygon_stipple.0.add(((py as u32) & 31) as usize) } & (1u32 << ((px as u32) & 31))) != 0
    {
        return true;
    }
    *background = r.t.stipple_opaque;
    *background
}

pub fn sgi_soft_stipple_background(r: &SGISoftDraw, frag: &mut SGISoftFrag) {
    for k in 0..4usize {
        frag.rgba[k] = r.t.stipple_background[k] as f64;
    }
    frag.index = r.t.stipple_background_index as f64;
}

/// A primitive's fragment at normalized depth z, (zd + 1) / 2 for the
/// normalized device z zd: dropped outside [0, 1], the view volume's near
/// and far planes, which clipping has already cut the primitive to, and
/// otherwise mapped to window depth n + (f - n) z through the depth range
/// [n, f] (OpenGL 1.2.1, 2.10.1). A polygon's offset is then added to the
/// window depth, and the sum clamped to [0, 1] (3.5.5).
pub fn sgi_soft_fragment_coverage(
    r: &SGISoftDraw,
    px: i32,
    py: i32,
    z: f64,
    offset: f64,
    depth_w: f64,
    frag: &SGISoftFrag,
    coverage: f64,
    antialias: bool,
) {
    let mut z = z;
    let mut offset = offset;
    let mut zw = 0.0;

    if r.rules.depth_row_given {
        let slack = r.rules.depth_slack;

        // Depth and its divisor, each over w, interpolate linearly in the
        // window; their ratio is the row's depth at the fragment.
        z = (z + offset) / depth_w;
        offset = 0.0;
        if z < 0.0 && z >= -slack {
            z = 0.0;
        } else if z > 1.0 && z <= 1.0 + slack {
            z = 1.0;
        }
    }
    if !(coverage > 0.0) || !sgi_raster_window_depth(&r.st, z, offset, &mut zw) {
        return;
    }
    sgi_soft_fragment_at(r, px, py, zw, frag, coverage, antialias);
}

pub fn sgi_soft_fragment(r: &SGISoftDraw, px: i32, py: i32, z: f64, offset: f64, depth_w: f64, frag: &SGISoftFrag) {
    sgi_soft_fragment_coverage(r, px, py, z, offset, depth_w, frag, 1.0, false);
}

/// A triangle's polygon offset (3.5.5), o = m factor + r units, where m is
/// the larger magnitude of its window depth's slopes in x and y, which the
/// specification allows in place of their length, and r the rules' one
/// depth step; EXT_polygon_offset's bias adds in window depth. Boundary modes
/// share this slope calculation and select their own offset enable.
pub fn sgi_raster_depth_slope(st: &SGIGLDrawState, window: &[[f64; 3]; 3]) -> f64 {
    let mut n = 0.0;
    let mut f = 0.0;

    sgi_soft_depth_range(st, &mut n, &mut f);
    let x1 = window[1][0] - window[0][0];
    let y1 = window[1][1] - window[0][1];
    let x2 = window[2][0] - window[0][0];
    let y2 = window[2][1] - window[0][1];
    let z1 = window[1][2] - window[0][2];
    let z2 = window[2][2] - window[0][2];
    let det = x1 * y2 - x2 * y1;
    // Reviewed model choice for an entirely degenerate boundary: m = 0.
    if det == 0.0 {
        return 0.0;
    }
    fmax((z1 * y2 - z2 * y1).abs(), (x1 * z2 - x2 * z1).abs()) / det.abs() * (f - n).abs()
}

pub fn sgi_soft_window_vertices(r: &SGISoftDraw, v: &[SGISoftVertex; 3], window: &mut [[f64; 3]; 3]) {
    let sub = (1i32 << r.rules.subpixel_bits) as f64;

    for i in 0..3usize {
        window[i][0] = v[i].x as f64 / sub;
        window[i][1] = v[i].y as f64 / sub;
        window[i][2] = v[i].z;
    }
}

pub fn sgi_soft_slope(r: &SGISoftDraw, v: &[SGISoftVertex; 3]) -> f64 {
    let mut window = [[0.0f64; 3]; 3];

    sgi_soft_window_vertices(r, v, &mut window);
    sgi_raster_depth_slope(&r.st, &window)
}

pub fn sgi_raster_polygon_offset(rules: &SGIRasterRules, st: &SGIGLDrawState, window: &[[f64; 3]; 3]) -> f64 {
    if st.polygon_offset & SGI_GL_OFFSET_FILL == 0 {
        return 0.0;
    }
    sgi_raster_depth_slope(st, window) * st.offset_factor as f64
        + st.offset_units as f64 / sgi_soft_depth_max(rules) as f64
        + st.offset_bias as f64
}

pub fn sgi_soft_offset(r: &SGISoftDraw, v: &[SGISoftVertex; 3]) -> f64 {
    let mut window = [[0.0f64; 3]; 3];

    sgi_soft_window_vertices(r, v, &mut window);
    sgi_raster_polygon_offset(&r.rules, &r.st, &window)
}

/// The six planes of the view volume (OpenGL 1.2.1, 2.11), -w <= x, y,
/// z <= w, as each one's value at a point, which is >= 0 inside it.
pub const SGI_VOLUME_PLANES: i32 = 6;

pub fn sgi_volume_plane(v: &SGIVolumeVertex, plane: i32) -> f64 {
    let c = v.clip[(plane / 2) as usize];

    if plane & 1 != 0 { v.clip[3] - c } else { v.clip[3] + c }
}

pub fn sgi_volume_contains(v: &SGIVolumeVertex) -> bool {
    for i in 0..SGI_VOLUME_PLANES {
        if !(sgi_volume_plane(v, i) >= 0.0) {
            return false;
        }
    }
    true
}

/// The point at t along from in to out, in clip and eye coordinates and
/// every float attribute (2.13.8); the material index and edge flag are
/// bit patterns and stay in's.
pub fn sgi_volume_lerp(in_: &SGIVolumeVertex, out: &SGIVolumeVertex, t: f64, r: &mut SGIVolumeVertex) {
    for k in 0..4usize {
        r.clip[k] = in_.clip[k] + t * (out.clip[k] - in_.clip[k]);
        r.eye[k] = in_.eye[k] + t * (out.eye[k] - in_.eye[k]);
    }
    for k in 0..SGI_GL_VERTEX_FLOATS {
        r.attr[k] = if k == SGI_GL_V_MATERIAL || k == SGI_GL_V_EDGE {
            in_.attr[k]
        } else {
            (in_.attr[k] as f64 + t * (out.attr[k] - in_.attr[k]) as f64) as f32
        };
    }
}

/// Where the edge from in, inside a plane with value din, to out, outside
/// it with value dout, meets the plane. It is always found from the inside
/// end, so that two primitives sharing the edge make the same vertex.
pub fn sgi_volume_cut(in_: &SGIVolumeVertex, din: f64, out: &SGIVolumeVertex, dout: f64, r: &mut SGIVolumeVertex) {
    sgi_volume_lerp(in_, out, din / (din - dout), r);
}

/// Sutherland and Hodgman's clipping, one plane at a time: each edge
/// keeps its inside end and adds the point where it crosses the plane.
/// A value that is not a number is outside.
pub fn sgi_volume_clip_triangle(in_: &[SGIVolumeVertex; 3], out: &mut [SGIVolumeVertex; SGI_VOLUME_TRIANGLE_MAX]) -> u32 {
    let mut src: [SGIVolumeVertex; SGI_VOLUME_TRIANGLE_MAX] = std::array::from_fn(|_| SGIVolumeVertex::default());
    let mut dst: [SGIVolumeVertex; SGI_VOLUME_TRIANGLE_MAX] = std::array::from_fn(|_| SGIVolumeVertex::default());
    let mut n: usize = 3;

    for i in 0..3 {
        out[i] = in_[i].clone();
        src[i] = in_[i].clone();
    }
    if sgi_volume_contains(&in_[0]) && sgi_volume_contains(&in_[1]) && sgi_volume_contains(&in_[2]) {
        return 3;
    }
    for p in 0..SGI_VOLUME_PLANES {
        let mut m: usize = 0;

        for i in 0..n {
            let a = &src[i];
            let b = &src[(i + 1) % n];
            let da = sgi_volume_plane(a, p);
            let db = sgi_volume_plane(b, p);

            if da >= 0.0 {
                dst[m] = a.clone();
                m += 1;
                if !(db >= 0.0) {
                    let mut c = SGIVolumeVertex::default();
                    sgi_volume_cut(a, da, b, db, &mut c);
                    dst[m] = c;
                    m += 1;
                }
            } else if db >= 0.0 {
                let mut c = SGIVolumeVertex::default();
                sgi_volume_cut(b, db, a, da, &mut c);
                dst[m] = c;
                m += 1;
            }
        }
        n = m;
        std::mem::swap(&mut src, &mut dst);
        if n < 3 {
            return 0;
        }
    }
    for i in 0..n {
        out[i] = src[i].clone();
    }
    n as u32
}

/// Liang and Barsky's clipping: the segment keeps the part every plane
/// allows. Each end that a plane bounds moves to where the segment meets
/// that plane, found from the other end, inside it, as a triangle's cut
/// is: a fraction sa of the way from b to a, and tb from a to b.
pub fn sgi_volume_clip_segment(a: &mut SGIVolumeVertex, b: &mut SGIVolumeVertex) -> bool {
    let mut na = a.clone();
    let mut nb = b.clone();
    let mut sa = 1.0f64;
    let mut tb = 1.0f64;

    for p in 0..SGI_VOLUME_PLANES {
        let da = sgi_volume_plane(a, p);
        let db = sgi_volume_plane(b, p);

        if da >= 0.0 && db >= 0.0 {
            continue;
        }
        if !(da >= 0.0) && !(db >= 0.0) {
            return false;
        }
        if da >= 0.0 {
            tb = fmin(tb, da / (da - db));
        } else {
            sa = fmin(sa, db / (db - da));
        }
    }
    if !(sa + tb >= 1.0) {
        return false;
    }
    if sa < 1.0 {
        sgi_volume_lerp(b, a, sa, &mut na);
    }
    if tb < 1.0 {
        sgi_volume_lerp(a, b, tb, &mut nb);
    }
    *a = na;
    *b = nb;
    true
}

/// One vertex to clip coordinates (OpenGL 1.2.1, 2.10): object to eye by
/// the modelview and eye to clip by the projection, or by the window
/// matrix as sgi_gl_eye_to_clip() takes it.
pub fn sgi_volume_from_draw_vertex(st: &SGIGLDrawState, v: &[f32], out: &mut SGIVolumeVertex) {
    for row in 0..4usize {
        out.eye[row] = 0.0;
        for k in 0..4usize {
            out.eye[row] += st.modelview[k * 4 + row] as f64 * v[SGI_GL_V_POSITION + k] as f64;
        }
    }
    sgi_gl_eye_to_clip(st, &out.eye, &mut out.clip);
    out.attr.copy_from_slice(&v[..SGI_GL_VERTEX_FLOATS]);
}

/// Map one clipped vertex (2.10): the divide by w and the viewport map,
/// with depth left normalized to [0, 1]; the depth range maps it per
/// fragment. Under a window matrix the position is the matrix's and the
/// depth its window depth. Clipping has put the vertex in the view volume,
/// so its normalized depth is clamped to [0, 1] against rounding. Under the
/// rules' depth row the depth is the row's over w instead, unclamped, as
/// clipping does not bound it. The colour is the front colour; a back-facing
/// triangle takes its back colour later.
/// A vertex with w <= 0 is in the volume only at the clip-space origin,
/// where the divide has no answer, and returns false, as does one mapped
/// beyond the rasterizer's range.
pub fn sgi_soft_project(r: &SGISoftDraw, cv: &SGIVolumeVertex, out: &mut SGISoftVertex) -> bool {
    let st: &SGIGLDrawState = &r.st;
    let clip = &cv.clip;
    let eye = &cv.eye;
    let mut win = [0.0f64; 3];
    let quiet = sgi_soft_band_quiet.with(|q| q.get());

    if !sgi_soft_texture_vertex_valid(r, cv) {
        return false;
    }
    if !(clip[3] > 0.0) {
        if !quiet {
            crate::qlog!("sgi-gl: clipped vertex at w {} has no window position, primitive dropped", clip[3]);
        }
        return false;
    }
    sgi_gl_clip_to_window(st, clip, &mut win);
    let xw = win[0];
    let yw = win[1];
    if !(xw.abs() < 65536.0 && yw.abs() < 65536.0) {
        if !quiet {
            crate::qlog!("sgi-gl: vertex outside the rasterizer's range, primitive dropped");
        }
        return false;
    }
    for k in 0..4usize {
        out.texcoord[k] = cv.attr[SGI_GL_V_TEXCOORD + k] as f64;
    }
    let sub = (1i32 << r.rules.subpixel_bits) as f64;
    out.x = sgi_soft_round(&r.rules, xw * sub);
    out.y = sgi_soft_round(&r.rules, yw * sub);
    if !r.rules.depth_row_given {
        let z = win[2];
        out.z = if z >= 1.0 { 1.0 } else if z > 0.0 { z } else { 0.0 };
    } else {
        let d = &r.rules.depth_row;

        // The head's depth over w, which a window line interpolates.
        out.z = (d[0] * eye[0] + d[1] * eye[1] + d[2] * eye[2] + d[3] * eye[3]) / clip[3];
    }
    out.inv_w = 1.0 / clip[3];
    out.depth_w = out.inv_w;
    if r.rules.depth_divisor_given {
        let d = &r.rules.depth_divisor;

        out.depth_w = (d[0] * eye[0] + d[1] * eye[1] + d[2] * eye[2] + d[3] * eye[3]) / clip[3];
    }
    // The fog coordinate approximates the eye distance by |z|, which
    // 3.10 allows; it is affine in eye space, so interpolating it as a
    // vertex attribute gives it exactly at each fragment.
    out.fog = (if eye[3] != 0.0 { eye[2] / eye[3] } else { eye[2] }).abs();
    for i in 0..SGI_GL_CLIP_PLANES as usize {
        let p = &st.clip_plane[i];

        out.clip[i] = p[0] as f64 * eye[0] + p[1] as f64 * eye[1] + p[2] as f64 * eye[2] + p[3] as f64 * eye[3];
    }
    for k in 0..4usize {
        let a = cv.attr[SGI_GL_V_COLOR + k];
        let m = if a > 0.0 { a } else { 0.0 };
        let n = if m < 1.0 { m } else { 1.0 };
        out.rgba[k] = n as f64;
    }
    out.index = cv.attr[SGI_GL_V_INDEX] as f64;
    true
}

#[derive(Clone, Copy, Default)]
pub struct SGISoftTexPlane {
    /// S/W, T/W, Q/W and, for volumes, R/W.
    pub value: [f64; 4],
    pub dx: [f64; 4],
    pub dy: [f64; 4],
    pub x: f64,
    pub y: f64,
}

pub fn sgi_raster_set_parallel(bands: u32, min_pixels: u64) {
    sgi_soft_bands_wanted.store(bands, std::sync::atomic::Ordering::Relaxed);
    sgi_soft_bands_min_pixels.store(min_pixels, std::sync::atomic::Ordering::Relaxed);
}
