//! Ported from hw/display/sgi/gl-context.h (include/hw/display/sgi/) in
//! jamesbraid/qemu-sgi-origami.
//!
//! One OpenGL context's state, in the shape the SGI graphics heads share.
//! A head's family code frames and decodes its own command words and keeps
//! its own table of contexts. What one table entry holds is here: the GL
//! state a draw is made under (SGIGLDrawState), the matrix stacks, the
//! current vertex attributes, the clear values and the primitive being
//! assembled. Colours, matrices and vertex components are IEEE single
//! floats holding exactly the bits the client sent; arithmetic is double.
//!
//! Function bodies are ported from gl-context.c; the migration description
//! and its helpers (post-load, field-by-field state) are dropped.

use super::*;

/// A raster operation: pixel work on a head's planes that is not a GL
/// primitive, queued beside draws in the order the head received it.
pub type SGIRasterKind = u32;

/// Where colour is stored: a value the family's drawable gives and only
/// the family's store hook reads. Shared code copies it, never interprets it.
pub type SGIColourTarget = u32;

pub type SGIGLDrawResult = u32;
pub type SGIGLOp = u32;

// Constants
pub const SGI_GL_MAX_TARGETS: u32 = 4;

// The channels glColorMask lets a draw or clear write (4.2.2).
pub const SGI_GL_COLOUR_MASK_RED: u32 = 0x1;
pub const SGI_GL_COLOUR_MASK_GREEN: u32 = 0x2;
pub const SGI_GL_COLOUR_MASK_BLUE: u32 = 0x4;
pub const SGI_GL_COLOUR_MASK_ALPHA: u32 = 0x8;
pub const SGI_GL_COLOUR_MASK_ALL: u32 = 0xf;

// The buffers a clear names, glClear's mask bits.
pub const SGI_GL_DEPTH_BUFFER_BIT: u32 = 0x0100;
pub const SGI_GL_STENCIL_BUFFER_BIT: u32 = 0x0400;
pub const SGI_GL_ACCUM_BUFFER_BIT: u32 = 0x0200;
pub const SGI_GL_COLOR_BUFFER_BIT: u32 = 0x4000;


pub const SGI_GL_VERTEX_FLOATS: usize = 24;
pub const SGI_GL_V_POSITION: usize = 0;
pub const SGI_GL_V_COLOR: usize = 4;
pub const SGI_GL_V_NORMAL: usize = 8;
pub const SGI_GL_V_MATERIAL: usize = 11;
pub const SGI_GL_V_TEXCOORD: usize = 12;
pub const SGI_GL_V_FOG: usize = 16;
pub const SGI_GL_V_EDGE: usize = 17;
pub const SGI_GL_V_INDEX: usize = 18;
pub const SGI_GL_V_BACK_INDEX: usize = 19;
pub const SGI_GL_V_BACK_COLOR: usize = 20;

// OpenGL 1.2 enumerants the state holds.
pub const SGI_GL_POINTS: u32 = 0;
pub const SGI_GL_LINES: u32 = 1;
pub const SGI_GL_LINE_LOOP: u32 = 2;
pub const SGI_GL_LINE_STRIP: u32 = 3;
pub const SGI_GL_TRIANGLES: u32 = 4;
pub const SGI_GL_TRIANGLE_STRIP: u32 = 5;
pub const SGI_GL_TRIANGLE_FAN: u32 = 6;
pub const SGI_GL_QUADS: u32 = 7;
pub const SGI_GL_QUAD_STRIP: u32 = 8;
pub const SGI_GL_POLYGON: u32 = 9;
pub const SGI_GL_MODELVIEW: u32 = 0x1700;
pub const SGI_GL_PROJECTION: u32 = 0x1701;
pub const SGI_GL_TEXTURE: u32 = 0x1702;
pub const SGI_GL_TEXTURE_1D: u32 = 0x0de0;
pub const SGI_GL_TEXTURE_2D: u32 = 0x0de1;
pub const SGI_GL_TEXTURE_3D: u32 = 0x806f;
pub const SGI_GL_TEXTURE_ENV_MODE: u32 = 0x2200;
// Texture coordinate generation (OpenGL 1.2.1, 2.10.4).
pub const SGI_GL_S: u32 = 0x2000;
pub const SGI_GL_T: u32 = 0x2001;
pub const SGI_GL_R: u32 = 0x2002;
pub const SGI_GL_Q: u32 = 0x2003;
pub const SGI_GL_EYE_LINEAR: u32 = 0x2400;
pub const SGI_GL_OBJECT_LINEAR: u32 = 0x2401;
pub const SGI_GL_SPHERE_MAP: u32 = 0x2402;
pub const SGI_GL_TEXTURE_GEN_MODE: u32 = 0x2500;
pub const SGI_GL_OBJECT_PLANE: u32 = 0x2501;
pub const SGI_GL_EYE_PLANE: u32 = 0x2502;
pub const SGI_GL_TEXTURE_GEN_S: u32 = 0x0c60;
pub const SGI_GL_TEXTURE_GEN_T: u32 = 0x0c61;
pub const SGI_GL_TEXTURE_GEN_R: u32 = 0x0c62;
pub const SGI_GL_TEXTURE_GEN_Q: u32 = 0x0c63;
pub const SGI_GL_TEXTURE_ENV_COLOR: u32 = 0x2201;
pub const SGI_GL_MODULATE: u32 = 0x2100;
pub const SGI_GL_DECAL: u32 = 0x2101;
pub const SGI_GL_NEAREST: u32 = 0x2600;
pub const SGI_GL_NEAREST_MIPMAP_NEAREST: u32 = 0x2700;
pub const SGI_GL_LINEAR_MIPMAP_NEAREST: u32 = 0x2701;
pub const SGI_GL_NEAREST_MIPMAP_LINEAR: u32 = 0x2702;
pub const SGI_GL_LINEAR_MIPMAP_LINEAR: u32 = 0x2703;
pub const SGI_GL_LINEAR_SHARPEN_SGIS: u32 = 0x80ad;
pub const SGI_GL_LINEAR_SHARPEN_ALPHA_SGIS: u32 = 0x80ae;
pub const SGI_GL_LINEAR_SHARPEN_COLOR_SGIS: u32 = 0x80af;
pub const SGI_GL_LINEAR_DETAIL_SGIS: u32 = 0x8097;
pub const SGI_GL_LINEAR_DETAIL_ALPHA_SGIS: u32 = 0x8098;
pub const SGI_GL_LINEAR_DETAIL_COLOR_SGIS: u32 = 0x8099;
// glAccum's operations (OpenGL 1.2.1, 4.2.4); ADD is also a texture mode.
pub const SGI_GL_ACCUM: u32 = 0x0100;
pub const SGI_GL_LOAD: u32 = 0x0101;
pub const SGI_GL_RETURN: u32 = 0x0102;
pub const SGI_GL_MULT: u32 = 0x0103;
pub const SGI_GL_ADD: u32 = 0x0104;
pub const SGI_GL_REPEAT: u32 = 0x2901;
pub const SGI_GL_CLAMP: u32 = 0x2900;
pub const SGI_GL_CLAMP_TO_EDGE: u32 = 0x812f;
pub const SGI_GL_CLAMP_TO_BORDER: u32 = 0x812d;
pub const SGI_GL_ALPHA4: u32 = 0x803b;
pub const SGI_GL_ALPHA8: u32 = 0x803c;
pub const SGI_GL_ALPHA12: u32 = 0x803d;
pub const SGI_GL_ALPHA16: u32 = 0x803e;
pub const SGI_GL_LUMINANCE4: u32 = 0x803f;
pub const SGI_GL_LUMINANCE8: u32 = 0x8040;
pub const SGI_GL_LUMINANCE12: u32 = 0x8041;
pub const SGI_GL_LUMINANCE16: u32 = 0x8042;
pub const SGI_GL_LUMINANCE4_ALPHA4: u32 = 0x8043;
pub const SGI_GL_LUMINANCE6_ALPHA2: u32 = 0x8044;
pub const SGI_GL_LUMINANCE8_ALPHA8: u32 = 0x8045;
pub const SGI_GL_LUMINANCE12_ALPHA4: u32 = 0x8046;
pub const SGI_GL_LUMINANCE12_ALPHA12: u32 = 0x8047;
pub const SGI_GL_LUMINANCE16_ALPHA16: u32 = 0x8048;
pub const SGI_GL_INTENSITY4: u32 = 0x804a;
pub const SGI_GL_INTENSITY8: u32 = 0x804b;
pub const SGI_GL_INTENSITY12: u32 = 0x804c;
pub const SGI_GL_INTENSITY16: u32 = 0x804d;
pub const SGI_GL_R3_G3_B2: u32 = 0x2a10;
pub const SGI_GL_RGB4: u32 = 0x804f;
pub const SGI_GL_RGB5: u32 = 0x8050;
pub const SGI_GL_RGB8: u32 = 0x8051;
pub const SGI_GL_RGB10: u32 = 0x8052;
pub const SGI_GL_RGB12: u32 = 0x8053;
pub const SGI_GL_RGB16: u32 = 0x8054;
pub const SGI_GL_RGBA2: u32 = 0x8055;
pub const SGI_GL_RGBA4: u32 = 0x8056;
pub const SGI_GL_RGB5_A1: u32 = 0x8057;
pub const SGI_GL_RGBA8: u32 = 0x8058;
pub const SGI_GL_RGB10_A2: u32 = 0x8059;
pub const SGI_GL_RGBA12: u32 = 0x805a;
pub const SGI_GL_RGBA16: u32 = 0x805b;
pub const SGI_GL_ALPHA: u32 = 0x1906;
pub const SGI_GL_RGB: u32 = 0x1907;
pub const SGI_GL_RGBA: u32 = 0x1908;
pub const SGI_GL_DEPTH_COMPONENT: u32 = 0x1902;
pub const SGI_GL_DEPTH_COMPONENT16_SGIX: u32 = 0x81a5;
pub const SGI_GL_DEPTH_COMPONENT24_SGIX: u32 = 0x81a6;
pub const SGI_GL_DEPTH_COMPONENT32_SGIX: u32 = 0x81a7;
pub const SGI_GL_TEXTURE_LEQUAL_R_SGIX: u32 = 0x819c;
pub const SGI_GL_TEXTURE_GEQUAL_R_SGIX: u32 = 0x819d;
pub const SGI_GL_LUMINANCE: u32 = 0x1909;
pub const SGI_GL_LUMINANCE_ALPHA: u32 = 0x190a;
pub const SGI_GL_INTENSITY: u32 = 0x8049;
pub const SGI_GL_FLAT: u32 = 0x1d00;
pub const SGI_GL_SMOOTH: u32 = 0x1d01;
// The comparisons in GL_NEVER's order, as depth_func holds them.
pub const SGI_GL_NEVER: u32 = 0;
pub const SGI_GL_LESS: u32 = 1;
pub const SGI_GL_EQUAL: u32 = 2;
pub const SGI_GL_LEQUAL: u32 = 3;
pub const SGI_GL_GREATER: u32 = 4;
pub const SGI_GL_NOTEQUAL: u32 = 5;
pub const SGI_GL_GEQUAL: u32 = 6;
pub const SGI_GL_ALWAYS: u32 = 7;
pub const SGI_GL_DRAW_FRONT: u32 = 0x1;
pub const SGI_GL_DRAW_BACK: u32 = 0x2;
pub const SGI_GL_DRAW_EXTRA0: u32 = 0x4;
pub const SGI_GL_DRAW_EXTRA1: u32 = 0x8;
//
// The family's overlay planes, a layer of their own: set, the draw goes
// to the drawable's overlay target alone and the other bits name nothing.
//
pub const SGI_GL_DRAW_OVERLAY: u32 = 0x40;
pub const SGI_GL_NEVER_ENUM: u32 = 0x0200;
pub const SGI_GL_CURRENT_COLOR: u32 = 0x0b00;
pub const SGI_GL_CURRENT_INDEX: u32 = 0x0b01;
pub const SGI_GL_CURRENT_NORMAL: u32 = 0x0b02;
pub const SGI_GL_CURRENT_TEXTURE_COORDS: u32 = 0x0b03;
pub const SGI_GL_CURRENT_RASTER_COLOR: u32 = 0x0b04;
pub const SGI_GL_CURRENT_RASTER_INDEX: u32 = 0x0b05;
pub const SGI_GL_CURRENT_RASTER_TEXTURE_COORDS: u32 = 0x0b06;
pub const SGI_GL_CURRENT_RASTER_POSITION: u32 = 0x0b07;
pub const SGI_GL_CURRENT_RASTER_POSITION_VALID: u32 = 0x0b08;
pub const SGI_GL_CURRENT_RASTER_DISTANCE: u32 = 0x0b09;
pub const SGI_GL_POINT_SMOOTH: u32 = 0x0b10;
pub const SGI_GL_POINT_SIZE: u32 = 0x0b11;
pub const SGI_GL_LINE_SMOOTH: u32 = 0x0b20;
pub const SGI_GL_LINE_WIDTH: u32 = 0x0b21;
pub const SGI_GL_LINE_STIPPLE: u32 = 0x0b24;
pub const SGI_GL_LINE_STIPPLE_PATTERN: u32 = 0x0b25;
pub const SGI_GL_LINE_STIPPLE_REPEAT: u32 = 0x0b26;
pub const SGI_GL_POLYGON_MODE: u32 = 0x0b40;
pub const SGI_GL_POLYGON_SMOOTH: u32 = 0x0b41;
pub const SGI_GL_POLYGON_STIPPLE: u32 = 0x0b42;
pub const SGI_GL_EDGE_FLAG: u32 = 0x0b43;
pub const SGI_GL_CULL_FACE: u32 = 0x0b44;
pub const SGI_GL_CULL_FACE_MODE: u32 = 0x0b45;
pub const SGI_GL_FRONT_FACE: u32 = 0x0b46;
pub const SGI_GL_LIGHTING: u32 = 0x0b50;
pub const SGI_GL_LIGHT_MODEL_LOCAL_VIEWER: u32 = 0x0b51;
pub const SGI_GL_LIGHT_MODEL_TWO_SIDE: u32 = 0x0b52;
pub const SGI_GL_LIGHT_MODEL_AMBIENT: u32 = 0x0b53;
pub const SGI_GL_SHADE_MODEL: u32 = 0x0b54;
pub const SGI_GL_COLOR_MATERIAL_FACE: u32 = 0x0b55;
pub const SGI_GL_COLOR_MATERIAL_PARAMETER: u32 = 0x0b56;
pub const SGI_GL_COLOR_MATERIAL: u32 = 0x0b57;
pub const SGI_GL_FOG: u32 = 0x0b60;
pub const SGI_GL_FOG_INDEX: u32 = 0x0b61;
pub const SGI_GL_FOG_DENSITY: u32 = 0x0b62;
pub const SGI_GL_FOG_START: u32 = 0x0b63;
pub const SGI_GL_FOG_END: u32 = 0x0b64;
pub const SGI_GL_FOG_MODE: u32 = 0x0b65;
pub const SGI_GL_FOG_COLOR: u32 = 0x0b66;
pub const SGI_GL_DEPTH_RANGE: u32 = 0x0b70;
pub const SGI_GL_DEPTH_TEST: u32 = 0x0b71;
pub const SGI_GL_DEPTH_WRITEMASK: u32 = 0x0b72;
pub const SGI_GL_DEPTH_FUNC: u32 = 0x0b74;
pub const SGI_GL_STENCIL_TEST: u32 = 0x0b90;
pub const SGI_GL_STENCIL_CLEAR_VALUE: u32 = 0x0b91;
pub const SGI_GL_STENCIL_FUNC: u32 = 0x0b92;
pub const SGI_GL_STENCIL_VALUE_MASK: u32 = 0x0b93;
pub const SGI_GL_STENCIL_FAIL: u32 = 0x0b94;
pub const SGI_GL_STENCIL_PASS_DEPTH_FAIL: u32 = 0x0b95;
pub const SGI_GL_STENCIL_PASS_DEPTH_PASS: u32 = 0x0b96;
pub const SGI_GL_STENCIL_REF: u32 = 0x0b97;
pub const SGI_GL_STENCIL_WRITEMASK: u32 = 0x0b98;
pub const SGI_GL_PERSPECTIVE_CORRECTION_HINT: u32 = 0x0c50;
pub const SGI_GL_POINT_SMOOTH_HINT: u32 = 0x0c51;
pub const SGI_GL_LINE_SMOOTH_HINT: u32 = 0x0c52;
pub const SGI_GL_POLYGON_SMOOTH_HINT: u32 = 0x0c53;
pub const SGI_GL_FOG_HINT: u32 = 0x0c54;
pub const SGI_GL_DONT_CARE: u32 = 0x1100;
pub const SGI_GL_FASTEST: u32 = 0x1101;
pub const SGI_GL_NICEST: u32 = 0x1102;
pub const SGI_GL_HINTS: u32 = 5;
pub const SGI_GL_MATRIX_MODE: u32 = 0x0ba0;
pub const SGI_GL_NORMALIZE: u32 = 0x0ba1;
// SGIX_sprite: the enable, its parameters and the modes.
pub const SGI_GL_SPRITE_SGIX: u32 = 0x8148;
pub const SGI_GL_SPRITE_MODE_SGIX: u32 = 0x8149;
pub const SGI_GL_SPRITE_AXIS_SGIX: u32 = 0x814a;
pub const SGI_GL_SPRITE_TRANSLATION_SGIX: u32 = 0x814b;
pub const SGI_GL_SPRITE_AXIAL_SGIX: u32 = 0x814c;
pub const SGI_GL_SPRITE_OBJECT_ALIGNED_SGIX: u32 = 0x814d;
pub const SGI_GL_SPRITE_EYE_ALIGNED_SGIX: u32 = 0x814e;
pub const SGI_GL_VIEWPORT: u32 = 0x0ba2;
pub const SGI_GL_MODELVIEW_STACK_DEPTH: u32 = 0x0ba3;
pub const SGI_GL_PROJECTION_STACK_DEPTH: u32 = 0x0ba4;
pub const SGI_GL_TEXTURE_STACK_DEPTH: u32 = 0x0ba5;
pub const SGI_GL_MODELVIEW_MATRIX: u32 = 0x0ba6;
pub const SGI_GL_PROJECTION_MATRIX: u32 = 0x0ba7;
pub const SGI_GL_TEXTURE_MATRIX: u32 = 0x0ba8;
pub const SGI_GL_ALPHA_TEST: u32 = 0x0bc0;
pub const SGI_GL_ALPHA_TEST_FUNC: u32 = 0x0bc1;
pub const SGI_GL_ALPHA_TEST_REF: u32 = 0x0bc2;
pub const SGI_GL_DITHER: u32 = 0x0bd0;
pub const SGI_GL_BLEND_DST: u32 = 0x0be0;
pub const SGI_GL_BLEND_SRC: u32 = 0x0be1;
pub const SGI_GL_BLEND: u32 = 0x0be2;
pub const SGI_GL_LOGIC_OP_MODE: u32 = 0x0bf0;
pub const SGI_GL_INDEX_LOGIC_OP: u32 = 0x0bf1;
pub const SGI_GL_COLOR_LOGIC_OP: u32 = 0x0bf2;
pub const SGI_GL_SCISSOR_BOX: u32 = 0x0c10;
pub const SGI_GL_SCISSOR_TEST: u32 = 0x0c11;
pub const SGI_GL_INDEX_CLEAR_VALUE: u32 = 0x0c20;
pub const SGI_GL_INDEX_WRITEMASK: u32 = 0x0c21;
pub const SGI_GL_COLOR_CLEAR_VALUE: u32 = 0x0c22;
pub const SGI_GL_COLOR_WRITEMASK: u32 = 0x0c23;
pub const SGI_GL_CLIP_PLANE0: u32 = 0x3000;
pub const SGI_GL_LIGHT0: u32 = 0x4000;
pub const SGI_GL_BLEND_COLOR: u32 = 0x8005;
pub const SGI_GL_BLEND_EQUATION: u32 = 0x8009;
pub const SGI_GL_RESCALE_NORMAL: u32 = 0x803a;
// Polygon offset (3.5.5, and EXT_polygon_offset's bias).
pub const SGI_GL_POLYGON_OFFSET_UNITS: u32 = 0x2a00;
pub const SGI_GL_POLYGON_OFFSET_POINT: u32 = 0x2a01;
pub const SGI_GL_POLYGON_OFFSET_LINE: u32 = 0x2a02;
pub const SGI_GL_POLYGON_OFFSET_FILL: u32 = 0x8037;
pub const SGI_GL_POLYGON_OFFSET_FACTOR: u32 = 0x8038;
pub const SGI_GL_POLYGON_OFFSET_BIAS: u32 = 0x8039;
// The polygon_offset bits: the enable of each polygon mode's offset.
pub const SGI_GL_OFFSET_FILL: u32 = 0x1;
pub const SGI_GL_OFFSET_LINE: u32 = 0x2;
pub const SGI_GL_OFFSET_POINT: u32 = 0x4;

//
// glBlendFunc's factors (4.1.6, and the imaging subset's constant
// factors) and glBlendEquation's equations. GL_ZERO is 0 and GL_ONE 1;
// GL_LOGIC_OP is EXT_blend_logic_op's equation.
//
pub const SGI_GL_SRC_COLOR: u32 = 0x0300;
pub const SGI_GL_ONE_MINUS_SRC_COLOR: u32 = 0x0301;
pub const SGI_GL_SRC_ALPHA: u32 = 0x0302;
pub const SGI_GL_ONE_MINUS_SRC_ALPHA: u32 = 0x0303;
pub const SGI_GL_DST_ALPHA: u32 = 0x0304;
pub const SGI_GL_ONE_MINUS_DST_ALPHA: u32 = 0x0305;
pub const SGI_GL_DST_COLOR: u32 = 0x0306;
pub const SGI_GL_ONE_MINUS_DST_COLOR: u32 = 0x0307;
pub const SGI_GL_SRC_ALPHA_SATURATE: u32 = 0x0308;
pub const SGI_GL_CONSTANT_COLOR: u32 = 0x8001;
pub const SGI_GL_ONE_MINUS_CONSTANT_COLOR: u32 = 0x8002;
pub const SGI_GL_CONSTANT_ALPHA: u32 = 0x8003;
pub const SGI_GL_ONE_MINUS_CONSTANT_ALPHA: u32 = 0x8004;
pub const SGI_GL_FUNC_ADD: u32 = 0x8006;
pub const SGI_GL_MIN: u32 = 0x8007;
pub const SGI_GL_MAX: u32 = 0x8008;
pub const SGI_GL_FUNC_SUBTRACT: u32 = 0x800a;
pub const SGI_GL_FUNC_REVERSE_SUBTRACT: u32 = 0x800b;
pub const SGI_GL_LOGIC_OP: u32 = 0x0bf1;
// glLogicOp's operations run from GL_CLEAR to GL_SET (4.1.8).
pub const SGI_GL_CLEAR: u32 = 0x1500;
pub const SGI_GL_COPY: u32 = 0x1503;
pub const SGI_GL_SET: u32 = 0x150f;

// Faces, and the light and material parameters (OpenGL 1.2.1, 2.13.2).
pub const SGI_GL_FRONT: u32 = 0x0404;
pub const SGI_GL_BACK: u32 = 0x0405;
pub const SGI_GL_FRONT_AND_BACK: u32 = 0x0408;
// glPolygonMode's modes (3.5.4).
pub const SGI_GL_POINT: u32 = 0x1b00;
pub const SGI_GL_LINE: u32 = 0x1b01;
pub const SGI_GL_FILL: u32 = 0x1b02;
//
// The rasterization enables a context keeps but does not draw with, as
// bits of raster_enables.
//
pub const SGI_GL_RASTER_POINT_SMOOTH: u32 = 0x01;
pub const SGI_GL_RASTER_LINE_SMOOTH: u32 = 0x02;
pub const SGI_GL_RASTER_LINE_STIPPLE: u32 = 0x04;
pub const SGI_GL_RASTER_POLYGON_SMOOTH: u32 = 0x08;
pub const SGI_GL_RASTER_POLYGON_STIPPLE: u32 = 0x10;
// glFog's modes (3.10).
pub const SGI_GL_EXP: u32 = 0x0800;
pub const SGI_GL_EXP2: u32 = 0x0801;
pub const SGI_GL_LINEAR: u32 = 0x2601;
// glStencilOp's operations (4.1.5); GL_ZERO is 0.
pub const SGI_GL_INVERT: u32 = 0x150a;
pub const SGI_GL_KEEP: u32 = 0x1e00;
pub const SGI_GL_REPLACE: u32 = 0x1e01;
pub const SGI_GL_INCR: u32 = 0x1e02;
pub const SGI_GL_DECR: u32 = 0x1e03;
// glFrontFace's directions (2.13.1).
pub const SGI_GL_CW: u32 = 0x0900;
pub const SGI_GL_CCW: u32 = 0x0901;
pub const SGI_GL_AMBIENT: u32 = 0x1200;
pub const SGI_GL_DIFFUSE: u32 = 0x1201;
pub const SGI_GL_SPECULAR: u32 = 0x1202;
pub const SGI_GL_POSITION: u32 = 0x1203;
pub const SGI_GL_SPOT_DIRECTION: u32 = 0x1204;
pub const SGI_GL_SPOT_EXPONENT: u32 = 0x1205;
pub const SGI_GL_SPOT_CUTOFF: u32 = 0x1206;
pub const SGI_GL_CONSTANT_ATTENUATION: u32 = 0x1207;
pub const SGI_GL_LINEAR_ATTENUATION: u32 = 0x1208;
pub const SGI_GL_QUADRATIC_ATTENUATION: u32 = 0x1209;
pub const SGI_GL_EMISSION: u32 = 0x1600;
pub const SGI_GL_SHININESS: u32 = 0x1601;
pub const SGI_GL_AMBIENT_AND_DIFFUSE: u32 = 0x1602;
pub const SGI_GL_COLOR_INDEXES: u32 = 0x1603;

// The client clip planes a context keeps, GL_CLIP_PLANE0 to 5 (2.11).
pub const SGI_GL_CLIP_PLANES: u32 = 6;

// The lights a context keeps, GL_LIGHT0 to GL_LIGHT7.
pub const SGI_GL_LIGHTS: u32 = 8;

pub const SGI_GL_STACK_DEPTH: u32 = 32;

//
// The most vertices a context holds between pieces of a primitive: a quad
// is drawn once its fourth arrives, so three wait.
//
pub const SGI_GL_HELD_VERTICES: u32 = 3;

pub const SGI_GL_WINDOW_GUARD: i32 = 16384;

pub const SGI_GL_INVALID_ENUM: u32 = 0x0500;
pub const SGI_GL_INVALID_OPERATION: u32 = 0x0502;
pub const SGI_GL_OUT_OF_MEMORY: u32 = 0x0505;

// Enumerations as type alias plus constants
pub const SGI_RASTER_FILL: SGIRasterKind = 0;
pub const SGI_RASTER_CLEAR: SGIRasterKind = 1;
pub const SGI_RASTER_COPY: SGIRasterKind = 2;
pub const SGI_RASTER_GLYPH: SGIRasterKind = 3;
pub const SGI_RASTER_LINE: SGIRasterKind = 4;
pub const SGI_RASTER_POINT: SGIRasterKind = 5;
pub const SGI_RASTER_BLIT: SGIRasterKind = 6;
pub const SGI_RASTER_BITMAP: SGIRasterKind = 7;
pub const SGI_RASTER_PIXELS: SGIRasterKind = 8;

pub const SGI_GL_DRAW_OK: SGIGLDrawResult = 0;
pub const SGI_GL_DRAW_OUT_OF_MEMORY: SGIGLDrawResult = 1;
pub const SGI_GL_DRAW_UNSUPPORTED: SGIGLDrawResult = 2;
pub const SGI_GL_DRAW_RESOURCE_ERROR: SGIGLDrawResult = 3;
pub const SGI_GL_DRAW_HOST_ERROR: SGIGLDrawResult = 4;

pub const SGI_GL_OP_VIEWPORT: SGIGLOp = 0;
pub const SGI_GL_OP_CLEAR_COLOR: SGIGLOp = 1;
pub const SGI_GL_OP_CLEAR_DEPTH: SGIGLOp = 2;
pub const SGI_GL_OP_SHADE_MODEL: SGIGLOp = 3;
pub const SGI_GL_OP_MATRIX_MODE: SGIGLOp = 4;
pub const SGI_GL_OP_LOAD_MATRIX: SGIGLOp = 5;
pub const SGI_GL_OP_LOAD_IDENTITY: SGIGLOp = 6;
pub const SGI_GL_OP_MULT_MATRIX: SGIGLOp = 7;
pub const SGI_GL_OP_FRUSTUM: SGIGLOp = 8;
pub const SGI_GL_OP_ORTHO: SGIGLOp = 9;
pub const SGI_GL_OP_PUSH_MATRIX: SGIGLOp = 10;
pub const SGI_GL_OP_POP_MATRIX: SGIGLOp = 11;
pub const SGI_GL_OP_ROTATE: SGIGLOp = 12;
pub const SGI_GL_OP_SCALE: SGIGLOp = 13;
pub const SGI_GL_OP_TRANSLATE: SGIGLOp = 14;
pub const SGI_GL_OP_ENABLE: SGIGLOp = 15;
pub const SGI_GL_OP_DISABLE: SGIGLOp = 16;
pub const SGI_GL_OP_DEPTH_FUNC: SGIGLOp = 17;
pub const SGI_GL_OP_DEPTH_MASK: SGIGLOp = 18;
pub const SGI_GL_OP_DRAW_BUFFERS: SGIGLOp = 19;
pub const SGI_GL_OP_COLOR3F: SGIGLOp = 20;
pub const SGI_GL_OP_COLOR4F: SGIGLOp = 21;
pub const SGI_GL_OP_COLOR3UB: SGIGLOp = 22;
pub const SGI_GL_OP_COLOR4UB: SGIGLOp = 23;
pub const SGI_GL_OP_NORMAL3F: SGIGLOp = 24;
pub const SGI_GL_OP_EDGE_FLAG: SGIGLOp = 25;
pub const SGI_GL_OP_BEGIN: SGIGLOp = 26;
pub const SGI_GL_OP_END: SGIGLOp = 27;
pub const SGI_GL_OP_VERTEX2F: SGIGLOp = 28;
pub const SGI_GL_OP_VERTEX3F: SGIGLOp = 29;
pub const SGI_GL_OP_VERTEX4F: SGIGLOp = 30;
pub const SGI_GL_OP_VERTEX2I: SGIGLOp = 31;
pub const SGI_GL_OP_FLUSH: SGIGLOp = 32;
pub const SGI_GL_OP_CLEAR: SGIGLOp = 33;
pub const SGI_GL_OP_FINISH: SGIGLOp = 34;
pub const SGI_GL_OP_RASTER_POS2F: SGIGLOp = 35;
pub const SGI_GL_OP_RASTER_POS3F: SGIGLOp = 36;
pub const SGI_GL_OP_RASTER_POS4F: SGIGLOp = 37;
pub const SGI_GL_OP_INDEXF: SGIGLOp = 38;
pub const SGI_GL_OP_INDEX_MASK: SGIGLOp = 39;
pub const SGI_GL_OP_CLEAR_INDEX: SGIGLOp = 40;
pub const SGI_GL_OP_DEPTH_RANGE: SGIGLOp = 41;
//
// glLightfv: the light's number, 0 for GL_LIGHT0, the parameter and
// four floats, of which the parameter uses as many as it takes.
//
pub const SGI_GL_OP_LIGHT: SGIGLOp = 42;
// glMaterialfv: the face, the parameter and four floats, likewise.
pub const SGI_GL_OP_MATERIAL: SGIGLOp = 43;
//
// glLightModelfv: the parameter and four floats; a boolean parameter
// is true for a nonzero first float.
//
pub const SGI_GL_OP_LIGHT_MODEL: SGIGLOp = 44;
pub const SGI_GL_OP_COLOR_MATERIAL: SGIGLOp = 45;
pub const SGI_GL_OP_CULL_FACE: SGIGLOp = 46;
pub const SGI_GL_OP_FRONT_FACE: SGIGLOp = 47;
pub const SGI_GL_OP_SCISSOR: SGIGLOp = 48;
pub const SGI_GL_OP_ALPHA_FUNC: SGIGLOp = 49;
// GL_NEVER to GL_ALWAYS, the reference as int32, the value mask
pub const SGI_GL_OP_STENCIL_FUNC: SGIGLOp = 50;
pub const SGI_GL_OP_STENCIL_OP: SGIGLOp = 51;
pub const SGI_GL_OP_STENCIL_MASK: SGIGLOp = 52;
pub const SGI_GL_OP_CLEAR_STENCIL: SGIGLOp = 53;
pub const SGI_GL_OP_BLEND_FUNC: SGIGLOp = 54;
pub const SGI_GL_OP_BLEND_EQUATION: SGIGLOp = 55;
pub const SGI_GL_OP_BLEND_COLOR: SGIGLOp = 56;
pub const SGI_GL_OP_LOGIC_OP: SGIGLOp = 57;
pub const SGI_GL_OP_COLOR_MASK: SGIGLOp = 58;
//
// glFogfv: the parameter and four floats, of which it uses as many as
// it takes; GL_FOG_MODE's is the mode's enumerant as a float.
//
pub const SGI_GL_OP_FOG: SGIGLOp = 59;
pub const SGI_GL_OP_POLYGON_OFFSET: SGIGLOp = 60;
pub const SGI_GL_OP_POLYGON_OFFSET_EXT: SGIGLOp = 61;
pub const SGI_GL_OP_CLIP_PLANE: SGIGLOp = 62;
pub const SGI_GL_OP_POINT_SIZE: SGIGLOp = 63;
pub const SGI_GL_OP_LINE_WIDTH: SGIGLOp = 64;
pub const SGI_GL_OP_LINE_STIPPLE: SGIGLOp = 65;
pub const SGI_GL_OP_POLYGON_MODE: SGIGLOp = 66;
pub const SGI_GL_OP_TEXCOORD2F: SGIGLOp = 67;
pub const SGI_GL_OP_TEXCOORD3F: SGIGLOp = 68;
pub const SGI_GL_OP_TEXCOORD4F: SGIGLOp = 69;
// Parameter and four floats; mode's enumerant as a float.
pub const SGI_GL_OP_TEX_ENV: SGIGLOp = 70;
//
// IRIS GL swaptmesh as the OpenGL client passes it through an open
// triangle strip: the next vertex makes its triangle with the strip's
// oldest retained vertex instead of its middle one.
//
pub const SGI_GL_OP_SWAP_TMESH: SGIGLOp = 71;
pub const SGI_GL_OP_HINT: SGIGLOp = 72;
//
// glTexGenfv: the coordinate (GL_S to GL_Q), the parameter and four
// floats; GL_TEXTURE_GEN_MODE's mode is its enumerant as a float.
//
pub const SGI_GL_OP_TEX_GEN: SGIGLOp = 73;


/// BITMAP: a run of glBitmap's image (OpenGL 1.2.1, 3.7), drawn as
/// fragments under a context's draw state. The image is height rows of
/// (width + 7) / 8 bytes, the bottom row first, most significant bit
/// leftmost. bytes[0] is byte `first` of the image. `bytes` points at
/// `count` bytes.
#[derive(Clone, Copy, Default)]
pub struct SGIRasterBitmap {
    pub state: P<SGIGLDrawState>,
    /// The window pixel of the image's lower-left corner.
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub first: u32,
    pub count: u32,
    pub bytes: P<u8>,
    /// The raster position's window depth, colour, colour index and eye
    /// distance (the fragments' fog coordinate, 3.10).
    pub z: f32,
    pub rgba: [f32; 4],
    pub index: f32,
    pub distance: f32,
    pub texcoord: [f32; 4],
}

/// Initial RGBA pixel-transfer scale and bias (OpenGL 1.2.1, 3.6.5).
#[derive(Clone, Copy, Default)]
pub struct SGIGLPixelTransfer {
    pub scale: [f32; 4],
    pub bias: [f32; 4],
}

/// Colour, depth and stencil pixel rectangles (OpenGL 1.2.1, 3.6.4). The
/// source pointers each address width * height elements, bottom row first;
/// null where the source is absent.
#[derive(Clone, Copy, Default)]
pub struct SGIRasterPixels {
    pub state: P<SGIGLDrawState>,
    /// Raster position in window coordinates.
    pub x: f64,
    pub y: f64,
    pub zoom: [f32; 2],
    pub transfer: SGIGLPixelTransfer,
    pub width: u32,
    pub height: u32,
    /// 0xRRGGBBAA.
    pub rgba: P<u32>,
    /// Unquantized.
    pub rgba_float: P<[f32; 4]>,
    pub rgba_double: P<[f64; 4]>,
    /// Normalized source.
    pub depth: P<f64>,
    /// Transferred indices.
    pub stencil: P<u32>,
    /// Transferred colour indices.
    pub index: P<u32>,
    pub depth_scale: f32,
    pub depth_bias: f32,
    /// Raster position colour for depth sources.
    pub colour: [f32; 4],
    /// Raster position index for depth sources.
    pub colour_index: f32,
    /// Raster position's window depth.
    pub z: f32,
    /// Raster position's fog distance.
    pub distance: f32,
    pub texcoord: [f32; 4],
}

/// Only the fields a kind the rasterizer executes needs are here.
/// Coordinates are plane pixels, the rectangle inclusive.
#[derive(Clone, Copy, Default)]
pub struct SGIRasterOp {
    /// SGIRasterKind.
    pub kind: u32,
    /// The colour targets written, as the drawable names them.
    pub targets: [SGIColourTarget; SGI_GL_MAX_TARGETS as usize],
    pub ntargets: u32,
    pub x0: i32,
    pub y0: i32,
    pub x1: i32,
    pub y1: i32,
    /// Only pixels inside these are written; at most the rectangle's. An
    /// empty list is how a family says its window has no visible pixel.
    pub clip: SGIClipList,
    /// CLEAR: SGI_GL_COLOR_BUFFER_BIT and SGI_GL_DEPTH_BUFFER_BIT.
    pub clear_buffers: u32,
    /// The GL clear colour; the rasterizer quantizes it to the plane.
    pub clear_rgba: [f32; 4],
    /// glColorMask's channels written: SGI_GL_COLOUR_MASK_RED and so on.
    pub clear_colour_mask: u32,
    /// In the depth plane's units; the plane keeps its low bits.
    pub clear_depth24: u32,
    /// SGI_GL_STENCIL_BUFFER_BIT: the clear value, written under the
    /// stencil write mask; the plane keeps its low bits.
    pub clear_stencil: u32,
    pub stencil_mask: u32,
    /// CLEAR in colour-index mode: nonzero index_bits, and the clear index
    /// already masked to them, written under index_mask.
    pub index_bits: u32,
    pub clear_index: u32,
    pub index_mask: u32,
    pub bitmap: SGIRasterBitmap,
    pub pixels: SGIRasterPixels,
}

/// One light's parameters (2.13.1, table 2.10): position and spot
/// direction in eye coordinates; attenuation constant, linear, quadratic.
/// spot_cutoff is in degrees, as glGetLightfv answers it; the lighting
/// equation compares against spot_cos_cutoff, its cosine.
#[derive(Clone, Copy, Default)]
pub struct SGIGLLight {
    pub ambient: [f32; 4],
    pub diffuse: [f32; 4],
    pub specular: [f32; 4],
    pub position: [f32; 4],
    pub spot_direction: [f32; 3],
    pub spot_exponent: f32,
    pub spot_cutoff: f32,
    pub attenuation: [f32; 3],
    pub spot_cos_cutoff: f32,
}

/// A light as a family's board delivers it, already decoded to eye
/// coordinates: for sgi_gl_set_light(). position has w 0 for a light at
/// infinity. spot_cos_cutoff of -1 or less is no spot.
#[derive(Clone, Copy, Default)]
pub struct SGIGLEyeLight {
    pub position: [f32; 4],
    pub spot_direction: [f32; 3],
    pub spot_cos_cutoff: f32,
    pub spot_exponent: f32,
    pub attenuation: [f32; 3],
    pub ambient: [f32; 4],
    pub diffuse: [f32; 4],
    pub specular: [f32; 4],
}

/// One face's material (2.13.1, table 2.10).
#[derive(Clone, Copy, Default)]
pub struct SGIGLMaterial {
    pub ambient: [f32; 4],
    pub diffuse: [f32; 4],
    pub specular: [f32; 4],
    pub emission: [f32; 4],
    pub shininess: f32,
    /// Ambient, diffuse, specular.
    pub color_indexes: [f32; 3],
}

/// Where a context draws, as the family's context switch, window clip and
/// buffer selection give it. Coordinates are plane pixels.
#[derive(Clone, Copy, Default)]
pub struct SGIGLDrawable {
    /// 0 while the family has no window for the context: nothing draws, and
    /// each draw, clear or bitmap is logged as dropped. 0 or 1.
    pub bound: u32,
    /// The window's lower-left pixel, where window coordinates start; its
    /// top-left under a window matrix whose rows run down the display.
    pub origin: [i32; 2],
    /// 0/0 until the family supplies a validated drawable extent.
    pub width: u32,
    pub height: u32,
    /// The pixels the window owns. A bound window with no rectangle is one
    /// other windows wholly cover: nothing draws and nothing is logged. The
    /// list's empty flag plays no part here.
    pub clip: SGIClipList,
    /// The colour targets glDrawBuffer's front and back buffers name.
    pub front: SGIColourTarget,
    pub back: SGIColourTarget,
    /// The targets draw-buffer bits 2 and 3 name; zero where the board has
    /// none, and the bit is then ignored.
    pub extra: [SGIColourTarget; 2],
    /// The target SGI_GL_DRAW_OVERLAY names; zero where none.
    pub overlay: SGIColourTarget,
}

/// The GL state a draw is made under.
#[derive(Clone, Copy, Default)]
pub struct SGIGLDrawState {
    /// Column-major, as glLoadMatrixf lays a matrix out.
    pub modelview: [f32; 16],
    pub projection: [f32; 16],
    pub viewport: [i32; 4],
    /// GL_FLAT or GL_SMOOTH.
    pub shade_model: u32,
    /// 0 or 1.
    pub depth_test: u32,
    /// The comparison minus GL_NEVER, 0 to 7.
    pub depth_func: u32,
    /// 0 or 1.
    pub depth_mask: u32,
    /// glDepthRange's near and far, each clamped to [0, 1] (2.10.1).
    pub depth_range: [f32; 2],
    /// SGI_GL_DRAW_* bits; those the drawable has no target for are ignored.
    pub draw_buffers: u32,
    /// 0 in RGBA mode; in colour-index mode the bits a colour index holds
    /// (sgi_gl_set_index_mode()).
    pub index_bits: u32,
    /// glIndexMask's write mask (4.2.2).
    pub index_mask: u32,
    /// glColorMask's channels, SGI_GL_COLOUR_MASK_RED and so on (4.2.2).
    pub colour_mask: u32,
    /// Polygon culling (3.5.1): GL_CULL_FACE's enable (0 or 1),
    /// glCullFace's mode, and glFrontFace's GL_CW or GL_CCW. Any
    /// front_face but GL_CW is GL_CCW.
    pub cull_face: u32,
    pub cull_face_mode: u32,
    pub front_face: u32,
    /// The scissor test (4.1.2): its enable (0 or 1) and glScissor's box,
    /// x, y, width and height in window coordinates.
    pub scissor_test: u32,
    pub scissor: [i32; 4],
    /// The alpha test (4.1.4): its enable (0 or 1), glAlphaFunc's
    /// comparison minus GL_NEVER, and its reference clamped to [0, 1].
    pub alpha_test: u32,
    pub alpha_func: u32,
    pub alpha_ref: f32,
    /// The stencil test (4.1.5): its enable (0 or 1), glStencilFunc's
    /// comparison minus GL_NEVER, reference (kept as sent) and value mask,
    /// glStencilMask's write mask, and glStencilOp's three operations as
    /// enumerants.
    pub stencil_test: u32,
    pub stencil_func: u32,
    pub stencil_ref: i32,
    pub stencil_value_mask: u32,
    pub stencil_write_mask: u32,
    pub stencil_op: [u32; 3],
    /// Blending (4.1.6): its enable (0 or 1), glBlendFunc's factors,
    /// glBlendEquation's equation, and glBlendColor clamped to [0, 1].
    pub blend: u32,
    pub blend_src: u32,
    pub blend_dst: u32,
    pub blend_equation: u32,
    pub blend_color: [f32; 4],
    /// The logical operation (4.1.8): the GL_INDEX_LOGIC_OP and
    /// GL_COLOR_LOGIC_OP enables (0 or 1) and glLogicOp's operation.
    pub index_logic_op: u32,
    pub color_logic_op: u32,
    pub logic_op_mode: u32,
    /// Fog (3.10): its enable (0 or 1), mode, density, start and end, the
    /// fog colour clamped to [0, 1] and the fog index.
    pub fog: u32,
    pub fog_mode: u32,
    pub fog_density: f32,
    pub fog_start: f32,
    pub fog_end: f32,
    pub fog_color: [f32; 4],
    pub fog_index: f32,
    /// Polygon offset (3.5.5): the enables as SGI_GL_OFFSET_FILL and so
    /// on, the factor, and the constant term, either glPolygonOffset's
    /// units or EXT_polygon_offset's bias; the command last given sets its
    /// own and zeroes the other.
    pub polygon_offset: u32,
    pub offset_factor: f32,
    pub offset_units: f32,
    pub offset_bias: f32,
    /// Client clip planes (2.11): bit i enables GL_CLIP_PLANEi; each plane
    /// is kept in eye coordinates.
    pub clip_planes: u32,
    pub clip_plane: [[f32; 4]; SGI_GL_CLIP_PLANES as usize],
    pub drawable: SGIGLDrawable,
    /// GL_DITHER's enable (0 or 1), initially on (4.1.7).
    pub dither: u32,
    /// A window matrix, for a family whose board takes one eye-to-window
    /// transform in place of a projection and a viewport; none of it is GL
    /// state (sgi_gl_set_window_matrix()). While window_matrix_given is 1,
    /// an eye position e goes to h = W e by window_matrix W, column-major,
    /// and h replaces the projection, the divide and the viewport map:
    /// window x and y are h0 / h3 and h1 / h3, unrounded, in pixels from
    /// the drawable's origin, and window depth is
    /// window_depth_scale * h2 / h3. With window_rows_down 1 window y
    /// counts rows down the display from the drawable's origin (then the
    /// window's top-left pixel). Clipping keeps near and far planes, window
    /// depth 0 and 1, and bounds x and y by SGI_GL_WINDOW_GUARD. See
    /// gl-context.h for the departures from OpenGL this implies (raster
    /// position validity, bitmap rows, tie rule, scissor measured from the
    /// top).
    pub window_matrix_given: u32,
    pub window_matrix: [f32; 16],
    pub window_depth_scale: f32,
    pub window_rows_down: u32,
    /// The texture environment belongs to the context, not its objects.
    pub texture_1d: u32,
    pub texture_2d: u32,
    pub texture_3d: u32,
    pub texture_env_mode: u32,
    pub texture_env_color: [f32; 4],
}

/// Host-owned deferred boundary. `vertices` was a malloc'd float array of
/// `capacity` floats with `count` vertices used; here a Vec sized by
/// `capacity`. Never shallow-copy a live context.
#[derive(Clone, Default)]
pub struct SGIGLPolygonAssembly {
    pub vertices: Vec<f32>,
    pub count: u32,
    pub capacity: u32,
    pub failed: u32,
    pub error: u32,
    /// Host activation and transient migration marker, never serialized.
    pub enabled: u32,
    pub loaded: u32,
}

#[derive(Clone, Default)]
pub struct SGIGLContext {
    /// The hardware identity the family keys this context by.
    pub hw_id: u32,
    pub cur: SGIGLDrawState,
    /// Matrices glPushMatrix saved under the current ones, oldest first.
    pub mv_stack: [[f32; 16]; SGI_GL_STACK_DEPTH as usize - 1],
    pub pr_stack: [[f32; 16]; SGI_GL_STACK_DEPTH as usize - 1],
    pub mv_depth: u32,
    pub pr_depth: u32,
    /// The texture matrix and the ones glPushMatrix saved under it (2.10.2),
    /// applied to each vertex before clipping.
    pub texture: [f32; 16],
    pub tx_stack: [[f32; 16]; SGI_GL_STACK_DEPTH as usize - 1],
    pub tx_depth: u32,
    /// GL_MODELVIEW, GL_PROJECTION, GL_TEXTURE or other.
    pub matrix_mode: u32,
    /// The lighting state glLight, glLightModel, glMaterial and
    /// glColorMaterial set. material[0] is the front face's, [1] the back's.
    pub light: [SGIGLLight; SGI_GL_LIGHTS as usize],
    pub material: [SGIGLMaterial; 2],
    pub light_model_ambient: [f32; 4],
    /// 0 or 1.
    pub light_model_local_viewer: u32,
    /// 0 or 1.
    pub light_model_two_side: u32,
    /// GL_COLOR_MATERIAL's enable.
    pub color_material: u32,
    /// GL_FRONT, GL_BACK or both.
    pub color_material_face: u32,
    /// A material parameter.
    pub color_material_mode: u32,
    /// GL_LIGHTING's enable.
    pub lighting: u32,
    /// Bit i: GL_LIGHTi's enable.
    pub lights_enabled: u32,
    /// GL_NORMALIZE's enable.
    pub normalize: u32,
    /// GL_RESCALE_NORMAL's enable.
    pub rescale_normal: u32,
    /// A family whose board sends its own normal matrix: while
    /// normal_matrix_given is 1, normals go to eye coordinates by
    /// normal_matrix, column-major 3x3, as given. normalize_always makes
    /// every eye normal unit length. None of the three is GL state;
    /// sgi_gl_init() clears them.
    pub normal_matrix_given: u32,
    pub normal_matrix: [f32; 9],
    pub normalize_always: u32,
    pub cur_color: [f32; 4],
    pub cur_index: f32,
    pub cur_normal: [f32; 3],
    /// 0 or 1.
    pub cur_edge: u32,
    /// The current raster position (2.12): window x, y and z and clip w,
    /// whether it is valid (0 or 1), its eye distance and its colour.
    pub raster_pos: [f32; 4],
    pub raster_valid: u32,
    pub raster_distance: f32,
    pub raster_color: [f32; 4],
    pub raster_index: f32,
    pub clear_color: [f32; 4],
    pub clear_index: f32,
    /// In the depth plane's units, as the family delivered it; a clear
    /// keeps the low bits the plane holds, so all ones is 1.0 at any depth.
    pub clear_depth: u32,
    /// glClearStencil's value.
    pub clear_stencil: u32,
    /// Aliased sizes and retained polygon modes (3.3 to 3.5).
    pub point_size: f32,
    pub line_width: f32,
    pub line_stipple_repeat: u32,
    pub line_stipple_pattern: u32,
    pub polygon_mode: [u32; 2],
    pub raster_enables: u32,
    /// Canonical window rows: bit x of row y (OpenGL 1.2.1, 3.5.2).
    pub polygon_stipple: [u32; 32],
    pub polygon: SGIGLPolygonAssembly,
    /// The primitive open since glBegin, SGI_GL_POINTS to SGI_GL_POLYGON.
    pub begin_mode: u32,
    /// 0 or 1.
    pub in_begin: u32,
    /// Vertices waiting for the rest of their piece, nheld of them.
    pub held: [[f32; SGI_GL_VERTEX_FLOATS]; SGI_GL_HELD_VERTICES as usize],
    pub nheld: u32,
    /// 1 while the next triangle of an open strip is an odd one, whose
    /// first two vertices are sent swapped (2.6.1).
    pub strip_odd: u32,
    /// 1 while held[2] keeps the open triangle strip's oldest retained
    /// vertex, which a swap makes the next triangle's first vertex.
    pub strip_prev: u32,
    /// 1 from a swap until the next vertex of the strip.
    pub strip_swap: u32,
    pub cur_texcoord: [f32; 4],
    pub raster_texcoord: [f32; 4],
    /// Host policy and migration-presence markers are never serialized.
    pub standalone_stipple: bool,
    pub line_phase_loaded: bool,
    pub draw_error_loaded: bool,
    pub line_phase: u32,
    /// Advisory modes are retained even though rendering ignores them.
    pub hints: [u32; SGI_GL_HINTS as usize],
    /// SGIX_sprite: whether it is enabled (0 or 1), the mode, and the axis
    /// and translation, as the family last set them.
    pub sprite: u32,
    pub sprite_mode: u32,
    pub sprite_axis: [f32; 3],
    pub sprite_translation: [f32; 3],
    /// Texture coordinate generation for S, T, R and Q (2.10.4): bit i of
    /// tex_gen_enabled is GL_TEXTURE_GEN_S + i's enable, each coordinate's
    /// mode, its object plane and its eye plane (kept as p M^-1 for the
    /// modelview M current when it was given).
    pub tex_gen_enabled: u32,
    pub tex_gen_mode: [u32; 4],
    pub tex_gen_object_plane: [[f32; 4]; 4],
    pub tex_gen_eye_plane: [[f32; 4]; 4],
}

/// A decoded command: its entry point and its scalar arguments as 32-bit
/// words in the specification's order, a float argument as the IEEE single
/// bits the client sent. `args` points at `nargs` words.
#[derive(Clone, Copy, Default)]
pub struct SGIGLCmd {
    pub op: u16,
    pub nargs: u16,
    pub args: P<u32>,
}

impl SGIGLCmd {
    /// `args` as a slice (added for the port; the C indexes the pointer).
    pub fn args(&self) -> &[u32] {
        if self.args.is_null() || self.nargs == 0 {
            return &[];
        }
        unsafe { std::slice::from_raw_parts(self.args.0, self.nargs as usize) }
    }

    /// A command over a caller-owned word slice, which must outlive it.
    pub fn new(op: u16, args: &[u32]) -> SGIGLCmd {
        SGIGLCmd {
            op,
            nargs: args.len() as u16,
            args: P(args.as_ptr() as *mut u32),
        }
    }
}

/// A batch of vertices drawn as one primitive mode under one draw state.
/// The context lights each vertex before it is drawn, so no draw carries
/// materials: a vertex's material index is 0, the draw state's. Pointer
/// fields are borrowed for the duration of the call that carries the draw;
/// null means absent.
#[derive(Clone, Copy, Default)]
pub struct SGIGLDraw {
    /// The context's hw_id.
    pub context: u32,
    /// SGI_GL_POINTS to SGI_GL_POLYGON.
    pub mode: u32,
    pub count: u32,
    /// count * SGI_GL_VERTEX_FLOATS floats, texture-matrix-transformed strq
    /// included, ready for clipping.
    pub vertices: P<f32>,
    pub materials: P<f32>,
    pub nmaterials: u32,
    /// True when the vertices carry back-face colour or index values for a
    /// back-facing polygon: two-sided lighting (2.13.1).
    pub back_colors: bool,
    pub state: P<SGIGLDrawState>,
    /// Borrowed dimensions; zero in direct submissions means one.
    pub point_size: f32,
    pub line_width: f32,
    pub point_smooth: bool,
    pub line_smooth: bool,
    pub polygon_smooth: bool,
    /// 32 canonical rows; null means stipple is disabled.
    pub polygon_stipple: P<u32>,
    pub polygon_stipple_enabled: bool,
    /// Two modes; null preserves legacy FILL.
    pub polygon_modes: P<u32>,
    pub fill_by_triangle: bool,
    pub line_stipple_enabled: bool,
    pub line_stipple_repeat: u32,
    pub line_stipple_pattern: u32,
    /// Borrowed through one synchronous rasterization; null means local.
    pub line_phase: P<u32>,
    /// With a borrowed phase, reset a local candidate before counting.
    pub line_phase_reset: bool,
}

/// Where a context's work goes. Every item is executed before the call
/// returns, so what it points at need only live that long.
#[derive(Clone, Copy, Default)]
pub struct SGIGLSink {
    pub draw: Option<fn(Opaque, &SGIGLDraw)>,
    pub raster: Option<fn(Opaque, &SGIRasterOp)>,
    /// glFinish's fence, once everything sent before it is done; None when
    /// the family has nothing to report for it.
    pub fence: Option<fn(Opaque, u32)>,
    pub opaque: Opaque,
    /// Preferred for every draw; activated contexts require it.
    pub draw_checked: Option<fn(Opaque, &SGIGLDraw) -> SGIGLDrawResult>,
    /// Borrowed by one synchronous checked command; null when unused.
    pub result: P<SGIGLDrawResult>,
}

/// Where glBitmap's image falls, from sgi_gl_bitmap_place().
#[derive(Clone, Copy, Default)]
pub struct SGIGLBitmap {
    /// Window pixel of the lower-left corner.
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

/// The most words one sgi_gl_get() answer takes: a matrix's.
pub const SGI_GL_GET_WORDS: usize = 16;

// Functions. Ported from vpro-src/shared/gl-context.c (the migration
// description at the end of the file is not ported).

use std::sync::atomic::{AtomicU32, Ordering};

// The last client clip plane and light enumerants (case ranges in the C).
pub const SGI_GL_CLIP_PLANE_LAST: u32 = SGI_GL_CLIP_PLANE0 + SGI_GL_CLIP_PLANES - 1;
pub const SGI_GL_LIGHT_LAST: u32 = SGI_GL_LIGHT0 + SGI_GL_LIGHTS - 1;

/// MIN/MAX of the C: `(a) < (b) ? (a) : (b)` and `(a) > (b) ? (a) : (b)`.
pub fn sgi_gl_c_min(a: f32, b: f32) -> f32 {
    if a < b { a } else { b }
}

pub fn sgi_gl_c_max(a: f32, b: f32) -> f32 {
    if a > b { a } else { b }
}

/// error_report_once()/warn_report_once(): one message per site.
pub fn sgi_gl_report_once(site: u32, msg: &str) {
    static REPORTED: AtomicU32 = AtomicU32::new(0);
    let bit = 1u32 << site;

    if REPORTED.fetch_or(bit, Ordering::Relaxed) & bit == 0 {
        eprintln!("{}", msg);
    }
}

/// The word `i` of a command's arguments (the C reads the pointer; a short
/// list reads as 0 here rather than past the end).
pub fn sgi_gl_arg(p: &[u32], i: usize) -> u32 {
    p.get(i).copied().unwrap_or(0)
}

pub fn sgi_gl_float(bits: u32) -> f32 {
    f32::from_bits(bits)
}

pub fn sgi_gl_bits(value: f32) -> u32 {
    value.to_bits()
}

pub fn sgi_gl_identity(m: &mut [f32; 16]) {
    for i in 0..16 {
        m[i] = if i % 5 == 0 { 1.0 } else { 0.0 };
    }
}

pub fn sgi_gl_rgba(v: &mut [f32; 4], r: f32, g: f32, b: f32, a: f32) {
    v[0] = r;
    v[1] = g;
    v[2] = b;
    v[3] = a;
}

/// The lighting state's initial values (OpenGL 1.2.1, table 2.10).
pub fn sgi_gl_lighting_init(c: &mut SGIGLContext) {
    for i in 0..SGI_GL_LIGHTS as usize {
        let l = &mut c.light[i];
        let on: f32 = if i == 0 { 1.0 } else { 0.0 };

        sgi_gl_rgba(&mut l.ambient, 0.0, 0.0, 0.0, 1.0);
        sgi_gl_rgba(&mut l.diffuse, on, on, on, 1.0);
        sgi_gl_rgba(&mut l.specular, on, on, on, 1.0);
        sgi_gl_rgba(&mut l.position, 0.0, 0.0, 1.0, 0.0);
        l.spot_direction[2] = -1.0;
        l.spot_cutoff = 180.0;
        l.spot_cos_cutoff = -1.0;
        l.attenuation[0] = 1.0;
    }
    for i in 0..2 {
        let m = &mut c.material[i];

        sgi_gl_rgba(&mut m.ambient, 0.2, 0.2, 0.2, 1.0);
        sgi_gl_rgba(&mut m.diffuse, 0.8, 0.8, 0.8, 1.0);
        sgi_gl_rgba(&mut m.specular, 0.0, 0.0, 0.0, 1.0);
        sgi_gl_rgba(&mut m.emission, 0.0, 0.0, 0.0, 1.0);
        m.color_indexes[1] = 1.0;
        m.color_indexes[2] = 1.0;
    }
    sgi_gl_rgba(&mut c.light_model_ambient, 0.2, 0.2, 0.2, 1.0);
    c.color_material_face = SGI_GL_FRONT_AND_BACK;
    c.color_material_mode = SGI_GL_AMBIENT_AND_DIFFUSE;
}

/// OpenGL 1.2.1 initial state (2.7, 2.10, 2.13.7, 4.1.5, 4.2.1-4.2.3).
pub fn sgi_gl_init(c: &mut SGIGLContext, hw_id: u32) {
    *c = SGIGLContext::default();
    c.hw_id = hw_id;
    for i in 0..SGI_GL_HINTS as usize {
        c.hints[i] = SGI_GL_DONT_CARE;
    }
    for i in 0..4 {
        c.cur_color[i] = 1.0;
    }
    c.cur.shade_model = SGI_GL_SMOOTH;
    c.sprite_mode = SGI_GL_SPRITE_AXIAL_SGIX;
    c.sprite_axis[2] = 1.0;
    c.matrix_mode = SGI_GL_MODELVIEW;
    c.cur.dither = 1;
    sgi_gl_identity(&mut c.cur.modelview);
    sgi_gl_identity(&mut c.cur.projection);
    sgi_gl_identity(&mut c.texture);
    c.cur.depth_func = SGI_GL_LESS;
    c.cur.alpha_func = SGI_GL_ALWAYS;
    c.cur.stencil_func = SGI_GL_ALWAYS;
    c.cur.stencil_value_mask = u32::MAX;
    c.cur.stencil_write_mask = u32::MAX;
    for i in 0..3 {
        c.cur.stencil_op[i] = SGI_GL_KEEP;
    }
    c.cur.blend_src = 1;
    c.cur.blend_equation = SGI_GL_FUNC_ADD;
    c.cur.logic_op_mode = SGI_GL_COPY;
    c.cur.texture_env_mode = SGI_GL_MODULATE;
    c.cur_texcoord[3] = 1.0;
    c.raster_texcoord[3] = 1.0;
    for i in 0..4 {
        c.tex_gen_mode[i] = SGI_GL_EYE_LINEAR;
    }
    c.tex_gen_object_plane[0][0] = 1.0;
    c.tex_gen_eye_plane[0][0] = 1.0;
    c.tex_gen_object_plane[1][1] = 1.0;
    c.tex_gen_eye_plane[1][1] = 1.0;
    c.cur.fog_mode = SGI_GL_EXP;
    c.cur.fog_density = 1.0;
    c.cur.fog_end = 1.0;
    c.point_size = 1.0;
    c.line_width = 1.0;
    c.line_stipple_repeat = 1;
    c.line_stipple_pattern = 0xffff;
    c.polygon_mode[0] = SGI_GL_FILL;
    c.polygon_mode[1] = SGI_GL_FILL;
    c.polygon_stipple = [0xffff_ffff; 32];
    c.cur.depth_mask = 1;
    c.cur.depth_range[1] = 1.0;
    c.clear_depth = u32::MAX;
    c.cur.draw_buffers = SGI_GL_DRAW_FRONT;
    c.cur.index_mask = u32::MAX;
    c.cur.colour_mask = SGI_GL_COLOUR_MASK_ALL;
    c.cur.cull_face_mode = SGI_GL_BACK;
    c.cur.front_face = SGI_GL_CCW;
    c.cur_index = 1.0;
    c.cur_normal[2] = 1.0;
    c.cur_edge = 1;
    c.raster_pos[3] = 1.0;
    c.raster_valid = 1;
    for i in 0..4 {
        c.raster_color[i] = 1.0;
    }
    c.raster_index = 1.0;
    sgi_gl_lighting_init(c);
}

pub fn sgi_gl_polygon_owned(c: &SGIGLContext) -> bool {
    c.polygon.error != 0
        || (c.polygon.enabled != 0 && c.in_begin != 0 && c.begin_mode == SGI_GL_POLYGON)
}

pub fn sgi_gl_polygon_release(p: &mut SGIGLPolygonAssembly) {
    p.vertices = Vec::new();
    p.count = 0;
    p.capacity = 0;
    p.failed = 0;
    p.loaded = 0;
}

/// Audited host consumers opt in; reset preserves that host policy.
pub fn sgi_gl_enable_polygon_modes(c: &mut SGIGLContext, sink: Option<&SGIGLSink>) -> bool {
    match sink {
        Some(s) if c.in_begin == 0 && s.draw_checked.is_some() => {}
        _ => return false,
    }
    c.polygon.enabled = 1;
    true
}

pub fn sgi_gl_open_line(c: &SGIGLContext) -> bool {
    c.in_begin != 0 && (c.begin_mode == SGI_GL_LINE_STRIP || c.begin_mode == SGI_GL_LINE_LOOP)
}

pub fn sgi_gl_enable_line_stipple(c: &mut SGIGLContext, sink: Option<&SGIGLSink>) -> bool {
    match sink {
        Some(s) if c.in_begin == 0 && s.draw_checked.is_some() => {}
        _ => return false,
    }
    c.standalone_stipple = true;
    true
}

pub fn sgi_gl_cleanup(c: &mut SGIGLContext) {
    sgi_gl_polygon_release(&mut c.polygon);
    c.polygon.enabled = 0;
    c.polygon.error = 0;
    c.standalone_stipple = false;
    c.line_phase_loaded = false;
    c.draw_error_loaded = false;
    c.line_phase = 0;
}

pub fn sgi_gl_reset(c: &mut SGIGLContext, hw_id: u32) {
    let enabled = c.polygon.enabled;
    let standalone = c.standalone_stipple;

    sgi_gl_cleanup(c);
    sgi_gl_init(c, hw_id);
    c.polygon.enabled = enabled;
    c.standalone_stipple = standalone;
}

/// Count and bytes are representation bounds, not hardware size limits.
/// `vertices` is kept `capacity * SGI_GL_VERTEX_FLOATS` floats long.
pub fn sgi_gl_polygon_reserve(p: &mut SGIGLPolygonAssembly, count: usize) -> bool {
    let mut capacity: usize = std::cmp::max(p.capacity as usize, 8);

    if count > u32::MAX as usize || count > usize::MAX / SGI_GL_VERTEX_FLOATS / 4 {
        return false;
    }
    if count <= p.capacity as usize {
        return true;
    }
    while capacity < count {
        if capacity > (u32::MAX / 2) as usize
            || capacity > usize::MAX / SGI_GL_VERTEX_FLOATS / 4 / 2
        {
            capacity = count;
            break;
        }
        capacity *= 2;
    }
    let floats = capacity * SGI_GL_VERTEX_FLOATS;
    if p.vertices.try_reserve_exact(floats.saturating_sub(p.vertices.len())).is_err() {
        return false;
    }
    p.vertices.resize(floats, 0.0);
    p.capacity = capacity as u32;
    true
}

/// Both contexts initialized and distinct; failure leaves both unchanged.
pub fn sgi_gl_clone(dst: &mut SGIGLContext, src: &SGIGLContext) -> bool {
    let open = src.in_begin != 0 && src.begin_mode == SGI_GL_POLYGON;

    if std::ptr::eq(dst as *const SGIGLContext, src as *const SGIGLContext)
        || (sgi_gl_open_line(src) && dst.standalone_stipple != src.standalone_stipple)
        || (open && dst.polygon.enabled != src.polygon.enabled)
        || (dst.polygon.enabled == 0
            && (src.polygon.count != 0
                || src.polygon.failed != 0
                || (src.polygon.enabled != 0 && sgi_gl_polygon_owned(src))))
    {
        return false;
    }
    // Ownership is replaced before the copy becomes visible.
    let mut copy = src.clone();
    copy.polygon.vertices = Vec::new();
    copy.polygon.capacity = 0;
    copy.polygon.enabled = dst.polygon.enabled;
    copy.polygon.loaded = 0;
    copy.standalone_stipple = dst.standalone_stipple;
    copy.line_phase_loaded = false;
    copy.draw_error_loaded = false;
    if !sgi_gl_polygon_reserve(&mut copy.polygon, src.polygon.count as usize) {
        return false;
    }
    if src.polygon.count != 0 {
        let n = src.polygon.count as usize * SGI_GL_VERTEX_FLOATS;
        copy.polygon.vertices[..n].copy_from_slice(&src.polygon.vertices[..n]);
    }
    sgi_gl_cleanup(dst);
    *dst = copy;
    true
}

pub fn sgi_gl_consume_error(c: &mut SGIGLContext, error: &mut u32) -> bool {
    if c.in_begin != 0 {
        return false;
    }
    *error = c.polygon.error;
    c.polygon.error = 0;
    true
}

pub fn sgi_gl_draw_result(c: &mut SGIGLContext, sink: &SGIGLSink, d: &SGIGLDraw) {
    let result: SGIGLDrawResult;

    if !sink.result.is_null() && *sink.result.get() != SGI_GL_DRAW_OK {
        return;
    }
    if sink.result.is_null()
        && c.polygon.enabled == 0
        && !c.standalone_stipple
        && sink.draw.is_some()
    {
        (sink.draw.unwrap())(sink.opaque, d);
        return;
    } else if let Some(f) = sink.draw_checked {
        result = f(sink.opaque, d);
    } else {
        sgi_gl_report_once(0, "sgi-gl: required checked draw callback is missing");
        if !sink.result.is_null() {
            *sink.result.get() = SGI_GL_DRAW_RESOURCE_ERROR;
        }
        return;
    }
    if result == SGI_GL_DRAW_OUT_OF_MEMORY && c.polygon.error == 0 {
        c.polygon.error = SGI_GL_OUT_OF_MEMORY;
    }
    if result == SGI_GL_DRAW_OK {
        return;
    }
    if !sink.result.is_null() {
        *sink.result.get() = result;
        return;
    }
    match result {
        SGI_GL_DRAW_OUT_OF_MEMORY => {}
        SGI_GL_DRAW_UNSUPPORTED => {
            sgi_gl_report_once(1, "sgi-gl: unsupported checked draw has no result consumer");
        }
        SGI_GL_DRAW_RESOURCE_ERROR => {
            sgi_gl_report_once(2, "sgi-gl: checked draw resource failure has no result consumer");
        }
        SGI_GL_DRAW_HOST_ERROR => {
            sgi_gl_report_once(3, "sgi-gl: checked draw host failure has no result consumer");
        }
        _ => {
            sgi_gl_report_once(
                4,
                &format!(
                    "sgi-gl: invalid checked draw result {} has no result consumer",
                    result
                ),
            );
        }
    }
}

/// Canonical 32x32 pattern, copied on upload. Bit x in row y names window
/// (x, y). Rejects use inside Begin/End.
pub fn sgi_gl_polygon_stipple(c: &mut SGIGLContext, rows: &[u32; 32]) -> bool {
    if c.in_begin != 0 {
        crate::qlog!("sgi-gl: polygon stipple inside glBegin: invalid operation");
        return false;
    }
    c.polygon_stipple = *rows;
    true
}

pub fn sgi_gl_get_polygon_stipple(c: &SGIGLContext, rows: &mut [u32; 32]) -> bool {
    if c.in_begin != 0 {
        crate::qlog!("sgi-gl: polygon stipple query inside glBegin: invalid operation");
        return false;
    }
    *rows = c.polygon_stipple;
    true
}

/// Point the context at a drawable. A bound half-known extent is refused
/// without changing the current drawable. Unbinding clears both dimensions.
pub fn sgi_gl_set_drawable(c: &mut SGIGLContext, d: &SGIGLDrawable) -> bool {
    if d.bound != 0 && (d.width != 0) != (d.height != 0) {
        return false;
    }
    c.cur.drawable = *d;
    if d.bound == 0 {
        c.cur.drawable.width = 0;
        c.cur.drawable.height = 0;
    }
    true
}

/// Put the context in colour-index mode with index_bits bits a colour
/// index, or in RGBA mode for 0.
pub fn sgi_gl_set_index_mode(c: &mut SGIGLContext, index_bits: u32) {
    c.cur.index_bits = std::cmp::min(index_bits, 32);
}

/// A colour index as a colour-index buffer of index_bits bits stores it
/// (OpenGL 1.2.1, 2.13.9): the integer part, masked to the buffer's bits.
/// 2.13.9 converts an index to fixed point with an unspecified number of
/// fraction bits and keeps the integer part, which for a value already in
/// fixed point is its floor; a value beyond 32 bits, or not a number,
/// keeps nothing a buffer could hold and stores 0.
pub fn sgi_gl_index_value(index_bits: u32, index: f64) -> u32 {
    let mask: u32 = if index_bits >= 32 { u32::MAX } else { (1u32 << index_bits) - 1 };
    let f = index.floor();

    if !(f >= -2147483648.0 && f < 4294967296.0) {
        return 0;
    }
    (if f < 0.0 { (f as i32) as u32 } else { f as u32 }) & mask
}

/// m = m * b, both column-major as glLoadMatrixf lays them out (2.10.2),
/// formed in double precision and stored at single.
pub fn sgi_gl_multiply(m: &mut [f32; 16], b: &[f64; 16]) {
    let mut r = [0.0f32; 16];

    for col in 0..4 {
        for row in 0..4 {
            let mut sum = 0.0f64;

            for k in 0..4 {
                sum += (m[k * 4 + row] as f64) * b[col * 4 + k];
            }
            r[col * 4 + row] = sum as f32;
        }
    }
    *m = r;
}

/// inv = m^-1, both column-major, by cofactors in double precision; false,
/// with inv unset, for a singular or non-finite m.
pub fn sgi_gl_invert(m: &[f32; 16], inv: &mut [f64; 16]) -> bool {
    let mut a = [0.0f64; 16];
    let mut det = 0.0f64;

    for i in 0..16 {
        a[i] = m[i] as f64;
    }
    for col in 0..4usize {
        for row in 0..4usize {
            let mut minor = [0.0f64; 9];
            let mut n = 0;

            for c in 0..4usize {
                for r in 0..4usize {
                    if c != col && r != row {
                        minor[n] = a[c * 4 + r];
                        n += 1;
                    }
                }
            }
            // The cofactor of a[col][row] is the adjugate's [row][col].
            let sign: f64 = if (row + col) & 1 != 0 { -1.0 } else { 1.0 };
            inv[row * 4 + col] = sign
                * (minor[0] * (minor[4] * minor[8] - minor[5] * minor[7])
                    - minor[3] * (minor[1] * minor[8] - minor[2] * minor[7])
                    + minor[6] * (minor[1] * minor[5] - minor[2] * minor[4]));
        }
    }
    for k in 0..4 {
        det += a[k * 4] * inv[k];
    }
    if det == 0.0 || !det.is_finite() {
        return false;
    }
    for i in 0..16 {
        inv[i] /= det;
    }
    true
}

/// The current matrix of the stack a matrix mode names, or None.
pub fn sgi_gl_slot_matrix(c: &mut SGIGLContext, slot: u32) -> Option<&mut [f32; 16]> {
    match slot {
        SGI_GL_MODELVIEW => Some(&mut c.cur.modelview),
        SGI_GL_PROJECTION => Some(&mut c.cur.projection),
        SGI_GL_TEXTURE => Some(&mut c.texture),
        _ => {
            crate::qlog!(
                "sgi-gl: matrix mode {:#x} is not modelled; matrix command ignored",
                slot
            );
            None
        }
    }
}

pub fn sgi_gl_current_matrix(c: &mut SGIGLContext) -> Option<&mut [f32; 16]> {
    let mode = c.matrix_mode;
    sgi_gl_slot_matrix(c, mode)
}

/// Load m, column-major, as the current matrix of the stack slot names:
/// SGI_GL_MODELVIEW, SGI_GL_PROJECTION or SGI_GL_TEXTURE, whatever the
/// context's matrix mode. False, and logged, for a slot the context does
/// not keep.
pub fn sgi_gl_set_matrix(c: &mut SGIGLContext, slot: u32, m: &[f32; 16]) -> bool {
    match sgi_gl_slot_matrix(c, slot) {
        None => false,
        Some(dst) => {
            *dst = *m;
            true
        }
    }
}

/// glOrtho's and glFrustum's matrices (OpenGL 1.2.1, 2.10.2), from the six
/// arguments the client sends as floats. Arguments the client itself would
/// have rejected with GL_INVALID_VALUE leave the matrix unchanged.
pub fn sgi_gl_projection_matrix(op: u16, p: &[u32], m: &mut [f64; 16]) -> bool {
    let l = sgi_gl_float(sgi_gl_arg(p, 0)) as f64;
    let r = sgi_gl_float(sgi_gl_arg(p, 1)) as f64;
    let b = sgi_gl_float(sgi_gl_arg(p, 2)) as f64;
    let t = sgi_gl_float(sgi_gl_arg(p, 3)) as f64;
    let n = sgi_gl_float(sgi_gl_arg(p, 4)) as f64;
    let f = sgi_gl_float(sgi_gl_arg(p, 5)) as f64;

    if l == r || b == t || n == f || (op as u32 == SGI_GL_OP_FRUSTUM && (n <= 0.0 || f <= 0.0)) {
        crate::qlog!("sgi-gl: projection with invalid bounds ignored");
        return false;
    }
    *m = [0.0; 16];
    if op as u32 == SGI_GL_OP_ORTHO {
        m[0] = 2.0 / (r - l);
        m[5] = 2.0 / (t - b);
        m[10] = -2.0 / (f - n);
        m[12] = -(r + l) / (r - l);
        m[13] = -(t + b) / (t - b);
        m[14] = -(f + n) / (f - n);
        m[15] = 1.0;
    } else {
        m[0] = 2.0 * n / (r - l);
        m[5] = 2.0 * n / (t - b);
        m[8] = (r + l) / (r - l);
        m[9] = (t + b) / (t - b);
        m[10] = -(f + n) / (f - n);
        m[11] = -1.0;
        m[14] = -2.0 * f * n / (f - n);
    }
    true
}

/// glTranslatef's, glScalef's and glRotatef's matrices (OpenGL 1.2.1,
/// 2.10.2). The rotation is by the angle in degrees, counter-clockwise
/// looking from the axis toward the origin, about the axis scaled to unit
/// length. A zero axis leaves the matrix unchanged.
pub fn sgi_gl_transform_matrix(op: u16, p: &[u32], m: &mut [f64; 16]) -> bool {
    *m = [0.0; 16];
    m[0] = 1.0;
    m[5] = 1.0;
    m[10] = 1.0;
    m[15] = 1.0;
    match op as u32 {
        SGI_GL_OP_TRANSLATE => {
            m[12] = sgi_gl_float(sgi_gl_arg(p, 0)) as f64;
            m[13] = sgi_gl_float(sgi_gl_arg(p, 1)) as f64;
            m[14] = sgi_gl_float(sgi_gl_arg(p, 2)) as f64;
            true
        }
        SGI_GL_OP_SCALE => {
            m[0] = sgi_gl_float(sgi_gl_arg(p, 0)) as f64;
            m[5] = sgi_gl_float(sgi_gl_arg(p, 1)) as f64;
            m[10] = sgi_gl_float(sgi_gl_arg(p, 2)) as f64;
            true
        }
        _ => {
            let a = sgi_gl_float(sgi_gl_arg(p, 0)) as f64 * std::f64::consts::PI / 180.0;
            let mut x = sgi_gl_float(sgi_gl_arg(p, 1)) as f64;
            let mut y = sgi_gl_float(sgi_gl_arg(p, 2)) as f64;
            let mut z = sgi_gl_float(sgi_gl_arg(p, 3)) as f64;
            let len = (x * x + y * y + z * z).sqrt();

            if !(len > 0.0) {
                crate::qlog!("sgi-gl: glRotatef about a zero axis ignored");
                return false;
            }
            x /= len;
            y /= len;
            z /= len;
            let c = a.cos();
            let sn = a.sin();
            // Column-major: m[col * 4 + row].
            m[0] = x * x * (1.0 - c) + c;
            m[1] = y * x * (1.0 - c) + z * sn;
            m[2] = x * z * (1.0 - c) - y * sn;
            m[4] = x * y * (1.0 - c) - z * sn;
            m[5] = y * y * (1.0 - c) + c;
            m[6] = y * z * (1.0 - c) + x * sn;
            m[8] = x * z * (1.0 - c) + y * sn;
            m[9] = y * z * (1.0 - c) - x * sn;
            m[10] = z * z * (1.0 - c) + c;
            true
        }
    }
}

/// glPushMatrix and glPopMatrix on the stack the slot names (2.10.2). Each
/// stack holds SGI_GL_STACK_DEPTH matrices, the current one included, the
/// least OpenGL 1.2.1 allows for the modelview stack (table 6.25). A push
/// onto a full stack and a pop of the last matrix change nothing, which is
/// what the specification has the stack-overflow and stack-underflow errors
/// do; the client reports the errors itself, if at all. (The C passes the
/// current matrix's pointer; here the slot it came from.)
pub fn sgi_gl_stack_command(c: &mut SGIGLContext, op: u16, slot: u32) {
    let (m, stack, depth) = if slot == SGI_GL_MODELVIEW {
        (&mut c.cur.modelview, &mut c.mv_stack, &mut c.mv_depth)
    } else if slot == SGI_GL_PROJECTION {
        (&mut c.cur.projection, &mut c.pr_stack, &mut c.pr_depth)
    } else {
        (&mut c.texture, &mut c.tx_stack, &mut c.tx_depth)
    };

    if op as u32 == SGI_GL_OP_PUSH_MATRIX {
        if *depth >= SGI_GL_STACK_DEPTH - 1 {
            crate::qlog!("sgi-gl: glPushMatrix on a full stack ignored");
            return;
        }
        stack[*depth as usize] = *m;
        *depth += 1;
        return;
    }
    if *depth == 0 {
        crate::qlog!("sgi-gl: glPopMatrix of the last matrix ignored");
        return;
    }
    *depth -= 1;
    *m = stack[*depth as usize];
}

/// Apply one matrix command (load, identity, multiply, translate, scale,
/// rotate, frustum or ortho) to `m` as the context does to its current
/// matrix. False for any other command.
pub fn sgi_gl_matrix_apply(m: &mut [f32; 16], cmd: &SGIGLCmd) -> bool {
    let args = cmd.args();
    let mut arg = [0.0f64; 16];

    match cmd.op as u32 {
        SGI_GL_OP_LOAD_MATRIX => {
            // The words are the floats' bits; copying keeps every bit.
            for i in 0..16 {
                m[i] = sgi_gl_float(sgi_gl_arg(args, i));
            }
            true
        }
        SGI_GL_OP_TRANSLATE | SGI_GL_OP_SCALE | SGI_GL_OP_ROTATE => {
            if sgi_gl_transform_matrix(cmd.op, args, &mut arg) {
                sgi_gl_multiply(m, &arg);
            }
            true
        }
        SGI_GL_OP_LOAD_IDENTITY => {
            sgi_gl_identity(m);
            true
        }
        SGI_GL_OP_MULT_MATRIX => {
            for i in 0..16 {
                arg[i] = sgi_gl_float(sgi_gl_arg(args, i)) as f64;
            }
            sgi_gl_multiply(m, &arg);
            true
        }
        SGI_GL_OP_FRUSTUM | SGI_GL_OP_ORTHO => {
            if sgi_gl_projection_matrix(cmd.op, args, &mut arg) {
                sgi_gl_multiply(m, &arg);
            }
            true
        }
        _ => false,
    }
}

pub fn sgi_gl_matrix_command(c: &mut SGIGLContext, cmd: &SGIGLCmd) {
    if cmd.op as u32 == SGI_GL_OP_LOAD_MATRIX {
        // The words are the floats' bits; copying keeps every bit.
        let mut load = [0.0f32; 16];
        let args = cmd.args();

        for i in 0..16 {
            load[i] = sgi_gl_float(sgi_gl_arg(args, i));
        }
        let mode = c.matrix_mode;
        sgi_gl_set_matrix(c, mode, &load);
        return;
    }
    if sgi_gl_current_matrix(c).is_none() {
        return;
    }
    if cmd.op as u32 == SGI_GL_OP_PUSH_MATRIX || cmd.op as u32 == SGI_GL_OP_POP_MATRIX {
        let mode = c.matrix_mode;
        sgi_gl_stack_command(c, cmd.op, mode);
        return;
    }
    let m = sgi_gl_current_matrix(c).unwrap();
    sgi_gl_matrix_apply(m, cmd);
}

/// The arguments each command takes.
pub const SGI_GL_NARGS_LEN: usize = 74;
pub static sgi_gl_nargs: [u8; SGI_GL_NARGS_LEN] = {
    let mut t = [0u8; SGI_GL_NARGS_LEN];
    t[SGI_GL_OP_HINT as usize] = 2;
    t[SGI_GL_OP_VIEWPORT as usize] = 4;
    t[SGI_GL_OP_CLEAR_COLOR as usize] = 4;
    t[SGI_GL_OP_CLEAR_DEPTH as usize] = 1;
    t[SGI_GL_OP_SHADE_MODEL as usize] = 1;
    t[SGI_GL_OP_MATRIX_MODE as usize] = 1;
    t[SGI_GL_OP_LOAD_MATRIX as usize] = 16;
    t[SGI_GL_OP_LOAD_IDENTITY as usize] = 0;
    t[SGI_GL_OP_MULT_MATRIX as usize] = 16;
    t[SGI_GL_OP_FRUSTUM as usize] = 6;
    t[SGI_GL_OP_ORTHO as usize] = 6;
    t[SGI_GL_OP_PUSH_MATRIX as usize] = 0;
    t[SGI_GL_OP_POP_MATRIX as usize] = 0;
    t[SGI_GL_OP_ROTATE as usize] = 4;
    t[SGI_GL_OP_SCALE as usize] = 3;
    t[SGI_GL_OP_TRANSLATE as usize] = 3;
    t[SGI_GL_OP_ENABLE as usize] = 1;
    t[SGI_GL_OP_DISABLE as usize] = 1;
    t[SGI_GL_OP_DEPTH_FUNC as usize] = 1;
    t[SGI_GL_OP_DEPTH_MASK as usize] = 1;
    t[SGI_GL_OP_DRAW_BUFFERS as usize] = 1;
    t[SGI_GL_OP_COLOR3F as usize] = 3;
    t[SGI_GL_OP_COLOR4F as usize] = 4;
    t[SGI_GL_OP_COLOR3UB as usize] = 3;
    t[SGI_GL_OP_COLOR4UB as usize] = 4;
    t[SGI_GL_OP_NORMAL3F as usize] = 3;
    t[SGI_GL_OP_EDGE_FLAG as usize] = 1;
    t[SGI_GL_OP_BEGIN as usize] = 1;
    t[SGI_GL_OP_END as usize] = 0;
    t[SGI_GL_OP_VERTEX2F as usize] = 2;
    t[SGI_GL_OP_VERTEX3F as usize] = 3;
    t[SGI_GL_OP_VERTEX4F as usize] = 4;
    t[SGI_GL_OP_VERTEX2I as usize] = 2;
    t[SGI_GL_OP_FLUSH as usize] = 0;
    t[SGI_GL_OP_CLEAR as usize] = 1;
    t[SGI_GL_OP_FINISH as usize] = 0;
    t[SGI_GL_OP_RASTER_POS2F as usize] = 2;
    t[SGI_GL_OP_RASTER_POS3F as usize] = 3;
    t[SGI_GL_OP_RASTER_POS4F as usize] = 4;
    t[SGI_GL_OP_INDEXF as usize] = 1;
    t[SGI_GL_OP_INDEX_MASK as usize] = 1;
    t[SGI_GL_OP_CLEAR_INDEX as usize] = 1;
    t[SGI_GL_OP_DEPTH_RANGE as usize] = 2;
    t[SGI_GL_OP_LIGHT as usize] = 6;
    t[SGI_GL_OP_MATERIAL as usize] = 6;
    t[SGI_GL_OP_LIGHT_MODEL as usize] = 5;
    t[SGI_GL_OP_COLOR_MATERIAL as usize] = 2;
    t[SGI_GL_OP_CULL_FACE as usize] = 1;
    t[SGI_GL_OP_FRONT_FACE as usize] = 1;
    t[SGI_GL_OP_SCISSOR as usize] = 4;
    t[SGI_GL_OP_ALPHA_FUNC as usize] = 2;
    t[SGI_GL_OP_STENCIL_FUNC as usize] = 3;
    t[SGI_GL_OP_STENCIL_OP as usize] = 3;
    t[SGI_GL_OP_STENCIL_MASK as usize] = 1;
    t[SGI_GL_OP_CLEAR_STENCIL as usize] = 1;
    t[SGI_GL_OP_BLEND_FUNC as usize] = 2;
    t[SGI_GL_OP_BLEND_EQUATION as usize] = 1;
    t[SGI_GL_OP_BLEND_COLOR as usize] = 4;
    t[SGI_GL_OP_LOGIC_OP as usize] = 1;
    t[SGI_GL_OP_COLOR_MASK as usize] = 4;
    t[SGI_GL_OP_FOG as usize] = 5;
    t[SGI_GL_OP_POLYGON_OFFSET as usize] = 2;
    t[SGI_GL_OP_POLYGON_OFFSET_EXT as usize] = 2;
    t[SGI_GL_OP_CLIP_PLANE as usize] = 5;
    t[SGI_GL_OP_POINT_SIZE as usize] = 1;
    t[SGI_GL_OP_LINE_WIDTH as usize] = 1;
    t[SGI_GL_OP_LINE_STIPPLE as usize] = 2;
    t[SGI_GL_OP_POLYGON_MODE as usize] = 2;
    t[SGI_GL_OP_TEXCOORD2F as usize] = 2;
    t[SGI_GL_OP_TEXCOORD3F as usize] = 3;
    t[SGI_GL_OP_TEXCOORD4F as usize] = 4;
    t[SGI_GL_OP_TEX_ENV as usize] = 5;
    t[SGI_GL_OP_TEX_GEN as usize] = 6;
    t[SGI_GL_OP_SWAP_TMESH as usize] = 0;
    t
};

/// The colour targets the draw state's draw buffers select, in bit order
/// and each once, into out; returns how many.
pub fn sgi_gl_draw_targets(
    st: &SGIGLDrawState,
    out: &mut [SGIColourTarget; SGI_GL_MAX_TARGETS as usize],
) -> u32 {
    let pick: [SGIColourTarget; SGI_GL_MAX_TARGETS as usize] = [
        st.drawable.front,
        st.drawable.back,
        st.drawable.extra[0],
        st.drawable.extra[1],
    ];
    let mut n: u32 = 0;

    if (st.draw_buffers & SGI_GL_DRAW_OVERLAY) != 0 && st.drawable.overlay != 0 {
        out[0] = st.drawable.overlay;
        return 1;
    }
    for bit in 0..SGI_GL_MAX_TARGETS as usize {
        let mut dup = false;

        if st.draw_buffers & (1u32 << bit) == 0 {
            continue;
        }
        if bit >= 2 && pick[bit] == 0 {
            crate::qlog!(
                "sgi-gl: draw buffer bit {} of {:#x} has no buffer; ignored",
                bit,
                st.draw_buffers
            );
            continue;
        }
        // A drawable whose buffers are one target, as a single-buffered one
        // may be, is stored to once: a read-modify-write stage in front of
        // the store must not apply twice.
        for i in 0..n as usize {
            dup |= out[i] == pick[bit];
        }
        if !dup {
            out[n as usize] = pick[bit];
            n += 1;
        }
    }
    if st.draw_buffers & !0xfu32 != 0 {
        crate::qlog!("sgi-gl: draw buffers {:#x} beyond bit 3 ignored", st.draw_buffers);
    }
    n
}

/// The pixels glClear reaches (OpenGL 1.2.1, 4.2.3): the drawable's
/// rectangles into op's clip list and bounding rectangle, narrowed to the
/// scissor box while the test is enabled. False when no pixel is left.
pub fn sgi_gl_clear_area(c: &SGIGLContext, op: &mut SGIRasterOp) -> bool {
    let d = &c.cur.drawable;

    if d.bound == 0 || d.clip.n == 0 {
        return false;
    }
    op.clip = d.clip;
    op.x0 = d.clip.r[0].x0;
    op.y0 = d.clip.r[0].y0;
    op.x1 = d.clip.r[0].x1;
    op.y1 = d.clip.r[0].y1;
    for i in 1..std::cmp::min(d.clip.n, SGI_CLIP_MAX as u32) as usize {
        op.x0 = std::cmp::min(op.x0, d.clip.r[i].x0);
        op.y0 = std::cmp::min(op.y0, d.clip.r[i].y0);
        op.x1 = std::cmp::max(op.x1, d.clip.r[i].x1);
        op.y1 = std::cmp::max(op.y1, d.clip.r[i].y1);
    }
    if c.cur.scissor_test != 0 {
        let sx = d.origin[0] as i64 + c.cur.scissor[0] as i64;
        let sy = d.origin[1] as i64 + c.cur.scissor[1] as i64;
        let x0 = std::cmp::max(op.x0 as i64, sx);
        let y0 = std::cmp::max(op.y0 as i64, sy);
        let x1 = std::cmp::min(op.x1 as i64, sx + c.cur.scissor[2] as i64 - 1);
        let y1 = std::cmp::min(op.y1 as i64, sy + c.cur.scissor[3] as i64 - 1);

        if x0 > x1 || y0 > y1 {
            return false;
        }
        op.x0 = x0 as i32;
        op.y0 = y0 as i32;
        op.x1 = x1 as i32;
        op.y1 = y1 as i32;
    }
    true
}

/// glClear (OpenGL 1.2.1, 4.2.3) as a raster op over the drawable's
/// rectangles. A clear passes only the pixel ownership, scissor and dither
/// steps, and the viewport does not bound it; the clear colour is not
/// dithered. The depth buffer is cleared only when the depth mask allows,
/// the stencil buffer under the stencil write mask, the colour buffers under
/// the colour or index mask, and the accumulation buffer is not modelled.
pub fn sgi_gl_clear(c: &mut SGIGLContext, sink: &SGIGLSink, mask: u32) {
    let mut op = SGIRasterOp::default();
    op.kind = SGI_RASTER_CLEAR;

    if mask & !(SGI_GL_COLOR_BUFFER_BIT | SGI_GL_DEPTH_BUFFER_BIT | SGI_GL_STENCIL_BUFFER_BIT) != 0
    {
        crate::qlog!(
            "sgi-gl: glClear of buffers {:#x} beyond colour, depth and stencil ignored",
            mask
        );
    }
    op.ntargets = sgi_gl_draw_targets(&c.cur, &mut op.targets);
    if c.cur.drawable.bound == 0 {
        crate::qlog!("sgi-gl: glClear with no window bound dropped");
        return;
    }
    // A window others wholly cover, or a scissor off it: nothing.
    if !sgi_gl_clear_area(c, &mut op) {
        return;
    }
    if mask & SGI_GL_COLOR_BUFFER_BIT != 0 {
        op.clear_buffers |= SGI_GL_COLOR_BUFFER_BIT;
        op.clear_rgba = c.clear_color;
        op.clear_colour_mask = c.cur.colour_mask;
        // In colour-index mode the clear index, under glIndexMask (4.2.3).
        op.index_bits = c.cur.index_bits;
        op.clear_index = sgi_gl_index_value(c.cur.index_bits, c.clear_index as f64);
        op.index_mask = c.cur.index_mask;
    }
    if (mask & SGI_GL_DEPTH_BUFFER_BIT) != 0 && c.cur.depth_mask != 0 {
        op.clear_buffers |= SGI_GL_DEPTH_BUFFER_BIT;
        op.clear_depth24 = c.clear_depth;
    }
    if mask & SGI_GL_STENCIL_BUFFER_BIT != 0 {
        op.clear_buffers |= SGI_GL_STENCIL_BUFFER_BIT;
        op.clear_stencil = c.clear_stencil;
        op.stencil_mask = c.cur.stencil_write_mask;
    }
    if op.clear_buffers != 0 {
        if let Some(f) = sink.raster {
            f(sink.opaque, &op);
        }
    }
}

/// Whether GL_LIGHTING supplies the vertex colour or colour index.
pub fn sgi_gl_lit(c: &SGIGLContext) -> bool {
    c.lighting != 0
}

pub fn sgi_gl_dot3(a: &[f32; 3], b: &[f32; 3]) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

/// v scaled to unit length in place; a zero vector, whose direction the
/// specification leaves undefined, stays zero and so lights nothing.
pub fn sgi_gl_unit3(v: &mut [f32; 3]) {
    let len = sgi_gl_dot3(v, v).sqrt();

    if len > 0.0 {
        for i in 0..3 {
            v[i] /= len;
        }
    }
}

/// The normal in eye coordinates (OpenGL 1.2.1, 2.10.3): n' = n M^-1 for M
/// the modelview's upper left 3x3, rescaled when GL_RESCALE_NORMAL is
/// enabled and then made unit length when GL_NORMALIZE is. n M^-1 is M's
/// cofactor matrix applied to n and divided by M's determinant. A singular
/// M, for which the specification gives no normal, leaves n unchanged.
/// A family's own normal matrix N replaces both M^-1 and the rescale:
/// n' = N n, as given.
pub fn sgi_gl_eye_normal(c: &SGIGLContext, n: &[f32; 3], out: &mut [f32; 3]) {
    let m = &c.cur.modelview;

    if c.normal_matrix_given != 0 {
        let nm = &c.normal_matrix;

        for j in 0..3 {
            out[j] = nm[j] * n[0] + nm[3 + j] * n[1] + nm[6 + j] * n[2];
        }
        if c.normalize != 0 || c.normalize_always != 0 {
            sgi_gl_unit3(out);
        }
        return;
    }
    // M[row][col] is m[col * 4 + row].
    let mm = |r: usize, k: usize| -> f32 { m[k * 4 + r] };
    let cof: [[f32; 3]; 3] = [
        [
            mm(1, 1) * mm(2, 2) - mm(1, 2) * mm(2, 1),
            mm(1, 2) * mm(2, 0) - mm(1, 0) * mm(2, 2),
            mm(1, 0) * mm(2, 1) - mm(1, 1) * mm(2, 0),
        ],
        [
            mm(0, 2) * mm(2, 1) - mm(0, 1) * mm(2, 2),
            mm(0, 0) * mm(2, 2) - mm(0, 2) * mm(2, 0),
            mm(0, 1) * mm(2, 0) - mm(0, 0) * mm(2, 1),
        ],
        [
            mm(0, 1) * mm(1, 2) - mm(0, 2) * mm(1, 1),
            mm(0, 2) * mm(1, 0) - mm(0, 0) * mm(1, 2),
            mm(0, 0) * mm(1, 1) - mm(0, 1) * mm(1, 0),
        ],
    ];
    let det: f32 = mm(0, 0) * cof[0][0] + mm(0, 1) * cof[0][1] + mm(0, 2) * cof[0][2];

    if det == 0.0 || !det.is_finite() {
        *out = *n;
    } else {
        // (n M^-1)_j = sum_i n_i cof[j][i] / det: M^-1[i][j] = cof[j][i].
        for j in 0..3 {
            out[j] = (n[0] * cof[j][0] + n[1] * cof[j][1] + n[2] * cof[j][2]) / det;
        }
        if c.rescale_normal != 0 {
            // 1 / |row 3 of M^-1|: row 3 is cof[.][2] / det.
            let r: [f32; 3] = [cof[0][2] / det, cof[1][2] / det, cof[2][2] / det];
            let len = sgi_gl_dot3(&r, &r).sqrt();

            if len > 0.0 {
                for i in 0..3 {
                    out[i] /= len;
                }
            }
        }
    }
    if c.normalize != 0 || c.normalize_always != 0 {
        sgi_gl_unit3(out);
    }
}

/// The vector from V to light l (2.13.1's P1P2), unnormalized.
pub fn sgi_gl_light_vp(p: &[f32; 4], eye: &[f32; 4], vp: &mut [f32; 3]) {
    for k in 0..3 {
        if p[3] == 0.0 && eye[3] == 0.0 {
            vp[k] = p[k] - eye[k];
        } else if p[3] == 0.0 {
            vp[k] = p[k];
        } else if eye[3] == 0.0 {
            vp[k] = -eye[k];
        } else {
            vp[k] = p[k] / p[3] - eye[k] / eye[3];
        }
    }
}

/// The unit vector toward the eye from V, added to VP for the half vector.
pub fn sgi_gl_light_half(c: &SGIGLContext, eye: &[f32; 4], vp: &[f32; 3], h: &mut [f32; 3]) {
    for k in 0..3 {
        let mut ve: f32 = if k == 2 { 1.0 } else { 0.0 };

        if c.light_model_local_viewer != 0 {
            ve = if eye[3] == 0.0 { -eye[k] } else { -eye[k] / eye[3] };
        }
        h[k] = ve;
    }
    if c.light_model_local_viewer != 0 {
        sgi_gl_unit3(h);
    }
    for k in 0..3 {
        h[k] += vp[k];
    }
    sgi_gl_unit3(h);
}

/// One face's lit colour (OpenGL 1.2.1, 2.13.1), clamped to [0, 1] (2.13.6):
///
///   c = e + a * as + sum over enabled lights i of
///       att_i spot_i (a * a_i + (n . VP_i) d * d_i +
///                     f_i (n . h_i)^s s * s_i)
///
/// with the face's material e, a, d, s and shininess, the light model's
/// ambient as, and alpha the material's diffuse alpha.
///
/// Approximated: this is the specification's equation in single-precision
/// arithmetic in the order written, not a head's geometry-engine
/// arithmetic, whose precision and ordering are not published.
pub fn sgi_gl_light_face(
    c: &SGIGLContext,
    eye: &[f32; 4],
    n: &[f32; 3],
    m: &SGIGLMaterial,
    out: &mut [f32; 4],
) {
    let mut sum = [0.0f32; 3];

    for k in 0..3 {
        sum[k] = m.emission[k] + m.ambient[k] * c.light_model_ambient[k];
    }
    for i in 0..SGI_GL_LIGHTS as usize {
        let l = &c.light[i];
        let p = &l.position;
        let mut vp = [0.0f32; 3];
        let mut h = [0.0f32; 3];
        let mut sd = [0.0f32; 3];
        let mut att: f32 = 1.0;
        let mut spot: f32 = 1.0;

        if c.lights_enabled & (1u32 << i) == 0 {
            continue;
        }
        // The vector from V to the light (2.13.1's P1P2).
        sgi_gl_light_vp(p, eye, &mut vp);
        if p[3] != 0.0 {
            let d = sgi_gl_dot3(&vp, &vp).sqrt();

            att = 1.0 / (l.attenuation[0] + l.attenuation[1] * d + l.attenuation[2] * d * d);
        }
        sgi_gl_unit3(&mut vp);
        if l.spot_cutoff != 180.0 {
            sd = l.spot_direction;
            sgi_gl_unit3(&mut sd);
            let s = -sgi_gl_dot3(&vp, &sd);
            spot = if s >= l.spot_cos_cutoff {
                sgi_gl_c_max(s, 0.0).powf(l.spot_exponent)
            } else {
                0.0
            };
        }
        let ndl = sgi_gl_c_max(sgi_gl_dot3(n, &vp), 0.0);
        // The half vector, with the eye's direction from V.
        sgi_gl_light_half(c, eye, &vp, &mut h);
        let ndh: f32 = if ndl != 0.0 {
            sgi_gl_c_max(sgi_gl_dot3(n, &h), 0.0).powf(m.shininess)
        } else {
            0.0
        };
        for k in 0..3 {
            sum[k] += att
                * spot
                * (m.ambient[k] * l.ambient[k]
                    + ndl * m.diffuse[k] * l.diffuse[k]
                    + ndh * m.specular[k] * l.specular[k]);
        }
    }
    for k in 0..3 {
        out[k] = sgi_gl_c_min(sgi_gl_c_max(sum[k], 0.0), 1.0);
    }
    out[3] = sgi_gl_c_min(sgi_gl_c_max(m.diffuse[3], 0.0), 1.0);
}

/// One colour-index lighting result (OpenGL 1.2.1, 2.13.5). The material's
/// COLOR INDEXES supply am, dm and sm; diffuse and specular light RGB become
/// scalar intensities with weights 0.30, 0.59 and 0.11. Keep c fractional:
/// mask its integer bits here, before vertex interpolation.
pub fn sgi_gl_light_index_value(c: &SGIGLContext, index: f32) -> f32 {
    let integer = (index as f64).floor();

    // Follow the existing 32-bit conversion for values outside its range.
    if !(integer >= -2147483648.0 && integer < 4294967296.0) {
        return 0.0;
    }
    (sgi_gl_index_value(c.cur.index_bits, index as f64) as f64 + (index as f64 - integer)) as f32
}

pub fn sgi_gl_light_index_face(
    c: &SGIGLContext,
    eye: &[f32; 4],
    n: &[f32; 3],
    m: &SGIGLMaterial,
) -> f32 {
    let mut d: f32 = 0.0;
    let mut s: f32 = 0.0;

    for i in 0..SGI_GL_LIGHTS as usize {
        let l = &c.light[i];
        let p = &l.position;
        let mut vp = [0.0f32; 3];
        let mut h = [0.0f32; 3];
        let mut sd = [0.0f32; 3];
        let mut att: f32 = 1.0;
        let mut spot: f32 = 1.0;

        if c.lights_enabled & (1u32 << i) == 0 {
            continue;
        }
        sgi_gl_light_vp(p, eye, &mut vp);
        if p[3] != 0.0 {
            let distance = sgi_gl_dot3(&vp, &vp).sqrt();

            att = 1.0
                / (l.attenuation[0] + l.attenuation[1] * distance + l.attenuation[2] * distance * distance);
        }
        sgi_gl_unit3(&mut vp);
        if l.spot_cutoff != 180.0 {
            sd = l.spot_direction;
            sgi_gl_unit3(&mut sd);
            let cosine = -sgi_gl_dot3(&vp, &sd);
            spot = if cosine >= l.spot_cos_cutoff {
                sgi_gl_c_max(cosine, 0.0).powf(l.spot_exponent)
            } else {
                0.0
            };
        }

        // Use the clamped direction product for diffuse and specular terms.
        let ndotl = sgi_gl_c_max(sgi_gl_dot3(n, &vp), 0.0);
        let diffuse: f32 = 0.30 * l.diffuse[0] + 0.59 * l.diffuse[1] + 0.11 * l.diffuse[2];
        let specular: f32 = 0.30 * l.specular[0] + 0.59 * l.specular[1] + 0.11 * l.specular[2];
        d += att * spot * diffuse * ndotl;

        // Equation 2.2 tests this clamped direction product.
        let mut ndh: f32 = if ndotl != 0.0 { 1.0 } else { 0.0 };
        sgi_gl_light_half(c, eye, &vp, &mut h);
        if ndh != 0.0 {
            ndh = sgi_gl_c_max(sgi_gl_dot3(n, &h), 0.0).powf(m.shininess);
        }
        s += att * spot * specular * ndh;
    }

    s = sgi_gl_c_min(s, 1.0);
    sgi_gl_light_index_value(
        c,
        sgi_gl_c_min(
            m.color_indexes[0]
                + d * (1.0 - s) * (m.color_indexes[1] - m.color_indexes[0])
                + s * (m.color_indexes[2] - m.color_indexes[0]),
            m.color_indexes[2],
        ),
    )
}

/// An object position to eye coordinates by the modelview.
pub fn sgi_gl_to_eye(c: &SGIGLContext, obj: &[f32; 4]) -> [f32; 4] {
    let mv = &c.cur.modelview;
    let mut eye = [0.0f32; 4];

    for row in 0..4 {
        eye[row] = mv[row] * obj[0] + mv[4 + row] * obj[1] + mv[8 + row] * obj[2] + mv[12 + row] * obj[3];
    }
    eye
}

/// Light both faces of one vertex in colour-index mode.
pub fn sgi_gl_light_index_vertex(
    c: &SGIGLContext,
    obj: &[f32; 4],
    n: &[f32; 3],
    front: &mut f32,
    back: &mut f32,
) {
    let mut normal = [0.0f32; 3];
    let eye = sgi_gl_to_eye(c, obj);

    sgi_gl_eye_normal(c, n, &mut normal);
    *front = sgi_gl_light_index_face(c, &eye, &normal, &c.material[0]);
    if c.light_model_two_side == 0 {
        *back = *front;
        return;
    }
    for k in 0..3 {
        normal[k] = -normal[k];
    }
    *back = sgi_gl_light_index_face(c, &eye, &normal, &c.material[1]);
}

/// Light a vertex at object position obj with normal n: its front colour
/// into front and, with two-sided lighting, its back colour into back,
/// from the back material and the reversed normal (2.13.1). Without
/// two-sided lighting a back-facing polygon takes the front colour, and
/// back is the front colour.
pub fn sgi_gl_light_vertex(
    c: &SGIGLContext,
    obj: &[f32; 4],
    n: &[f32; 3],
    front: &mut [f32; 4],
    back: &mut [f32; 4],
) {
    let mut normal = [0.0f32; 3];
    let eye = sgi_gl_to_eye(c, obj);

    sgi_gl_eye_normal(c, n, &mut normal);
    sgi_gl_light_face(c, &eye, &normal, &c.material[0], front);
    if c.light_model_two_side == 0 {
        *back = *front;
        return;
    }
    for k in 0..3 {
        normal[k] = -normal[k];
    }
    sgi_gl_light_face(c, &eye, &normal, &c.material[1], back);
}

/// Send one piece of the open primitive: count of v0 to v3.
pub fn sgi_gl_submit(
    c: &mut SGIGLContext,
    sink: &SGIGLSink,
    mode: u32,
    count: u32,
    v0: Option<&[f32; SGI_GL_VERTEX_FLOATS]>,
    v1: Option<&[f32; SGI_GL_VERTEX_FLOATS]>,
    v2: Option<&[f32; SGI_GL_VERTEX_FLOATS]>,
    v3: Option<&[f32; SGI_GL_VERTEX_FLOATS]>,
) {
    let stipple = if mode == SGI_GL_LINES { c.standalone_stipple } else { c.polygon.enabled != 0 };
    let v = [v0, v1, v2, v3];
    let mut verts = [[0.0f32; SGI_GL_VERTEX_FLOATS]; 4];
    let mut d = SGIGLDraw::default();

    d.context = c.hw_id;
    d.mode = mode;
    d.count = count;
    d.vertices = P(verts.as_mut_ptr() as *mut f32);
    d.back_colors = sgi_gl_lit(c) && c.light_model_two_side != 0;
    d.state = P(&mut c.cur as *mut SGIGLDrawState);
    d.point_size = c.point_size;
    d.line_width = c.line_width;
    d.point_smooth = c.raster_enables & SGI_GL_RASTER_POINT_SMOOTH != 0;
    d.line_smooth = c.raster_enables & SGI_GL_RASTER_LINE_SMOOTH != 0;
    d.polygon_smooth = c.raster_enables & SGI_GL_RASTER_POLYGON_SMOOTH != 0;
    d.polygon_modes = if c.polygon.enabled != 0 {
        P(c.polygon_mode.as_mut_ptr())
    } else {
        P::null()
    };
    d.line_stipple_enabled = stipple && (c.raster_enables & SGI_GL_RASTER_LINE_STIPPLE != 0);
    d.line_stipple_repeat = if stipple { c.line_stipple_repeat } else { 0 };
    d.line_stipple_pattern = if stipple { c.line_stipple_pattern } else { 0 };
    d.line_phase = if mode == SGI_GL_LINES && c.standalone_stipple {
        P(&mut c.line_phase as *mut u32)
    } else {
        P::null()
    };
    d.line_phase_reset = c.begin_mode == SGI_GL_LINES;
    d.polygon_stipple = P(c.polygon_stipple.as_mut_ptr());
    d.polygon_stipple_enabled = c.raster_enables & SGI_GL_RASTER_POLYGON_STIPPLE != 0;

    for i in 0..count as usize {
        if let Some(src) = v[i] {
            verts[i] = *src;
        }
    }
    sgi_gl_draw_result(c, sink, &d);
}

/// Texture coordinate generation (OpenGL 1.2.1, 2.10.4) for a vertex at
/// object position obj, which the modelview takes to the eye from pos,
/// with normal n: each enabled coordinate replaces the current one. pos
/// is obj but for SGIX_sprite, whose T A R lie between object and eye
/// coordinates (MM = M T A R), so pos is T A R obj and n is A R's turn of
/// the current normal. OBJECT_LINEAR is the object plane's product with
/// obj, the vertex as given, EYE_LINEAR the eye plane's with M pos, and
/// SPHERE_MAP, for S and T only, takes the reflection r of the unit vector
/// u from the eye to the vertex about the eye normal n', r = u - 2 n'
/// (n' . u), and gives r_x / m + 1/2 and r_y / m + 1/2 with m = 2
/// sqrt(r_x^2 + r_y^2 + (r_z + 1)^2). Single-precision floats, like the
/// lighting equation; a head's own arithmetic is unknown.
pub fn sgi_gl_tex_gen(
    c: &SGIGLContext,
    obj: &[f32; 4],
    pos: &[f32; 4],
    n: &[f32; 3],
    strq: &mut [f32; 4],
) {
    let eye = sgi_gl_to_eye(c, pos);

    for i in 0..4usize {
        if c.tex_gen_enabled & (1u32 << i) == 0 {
            continue;
        }
        match c.tex_gen_mode[i] {
            SGI_GL_OBJECT_LINEAR => {
                let p = &c.tex_gen_object_plane[i];
                strq[i] = p[0] * obj[0] + p[1] * obj[1] + p[2] * obj[2] + p[3] * obj[3];
            }
            SGI_GL_EYE_LINEAR => {
                let p = &c.tex_gen_eye_plane[i];
                strq[i] = p[0] * eye[0] + p[1] * eye[1] + p[2] * eye[2] + p[3] * eye[3];
            }
            SGI_GL_SPHERE_MAP => {
                let mut u = [0.0f32; 3];
                let mut normal = [0.0f32; 3];
                let mut r = [0.0f32; 3];

                for k in 0..3 {
                    u[k] = if eye[3] != 0.0 { eye[k] / eye[3] } else { eye[k] };
                }
                sgi_gl_unit3(&mut u);
                sgi_gl_eye_normal(c, n, &mut normal);
                let nu = sgi_gl_dot3(&normal, &u);
                for k in 0..3 {
                    r[k] = u[k] - 2.0 * normal[k] * nu;
                }
                let m: f32 = 2.0 * (r[0] * r[0] + r[1] * r[1] + (r[2] + 1.0) * (r[2] + 1.0)).sqrt();
                // Sphere map is valid for S and T only (i < 2).
                let ri = r.get(i).copied().unwrap_or(0.0);
                strq[i] = if m != 0.0 { ri / m + 0.5 } else { 0.5 };
            }
            _ => {}
        }
    }
}

/// A vertex's texture coordinates: the current ones, or the generated
/// ones of the enabled coordinates, then the texture matrix, before
/// clipping (OpenGL 1.2.1, 2.10.2).
pub fn sgi_gl_texture_coord(
    c: &SGIGLContext,
    obj: &[f32; 4],
    pos: &[f32; 4],
    n: &[f32; 3],
    out: &mut [f32; 4],
) {
    let mut strq = c.cur_texcoord;

    if c.tex_gen_enabled != 0 {
        sgi_gl_tex_gen(c, obj, pos, n, &mut strq);
    }
    for row in 0..4 {
        let mut value = 0.0f64;

        for k in 0..4 {
            value += (c.texture[k * 4 + row] as f64) * strq[k] as f64;
        }
        out[row] = value as f32;
    }
}

/// Hold a vertex in slot n of the three a context keeps.
pub fn sgi_gl_hold(c: &mut SGIGLContext, n: u32, v: &[f32; SGI_GL_VERTEX_FLOATS]) {
    c.held[n as usize] = *v;
}

/// r, row-major, turns +Z to the unit vector v about Z x v (Rodrigues).
pub fn sgi_gl_sprite_tilt(v: &[f64; 3], r: &mut [f64; 9]) {
    let s = v[0].hypot(v[1]);
    let mut k = [0.0f64; 3];
    let cs = v[2];

    if !(s > 0.0) {
        // Along Z already, or opposite it: a half turn about X.
        let half: [f64; 9] = [1.0, 0.0, 0.0, 0.0, -1.0, 0.0, 0.0, 0.0, -1.0];
        let none: [f64; 9] = [1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0];

        *r = if cs < 0.0 { half } else { none };
        return;
    }
    k[0] = -v[1] / s;
    k[1] = v[0] / s;
    k[2] = 0.0;
    for i in 0..3 {
        for j in 0..3 {
            r[i * 3 + j] = (if i == j { cs } else { 0.0 }) + (1.0 - cs) * k[i] * k[j];
        }
    }
    // sin(theta) [k]x, with sin(theta) = s.
    r[1] -= s * k[2];
    r[2] += s * k[1];
    r[3] += s * k[2];
    r[5] -= s * k[0];
    r[6] -= s * k[1];
    r[7] += s * k[0];
}

/// out = a b for row-major 3x3 matrices.
pub fn sgi_gl_mul3(a: &[f64; 9], b: &[f64; 9], out: &mut [f64; 9]) {
    for i in 0..3 {
        for j in 0..3 {
            out[i * 3 + j] = a[i * 3] * b[j] + a[i * 3 + 1] * b[3 + j] + a[i * 3 + 2] * b[6 + j];
        }
    }
}

/// Where the eye-coordinate origin is in the frame M T rot, as a point,
/// for M the modelview, T the sprite translation and rot a row-major
/// rotation. False when that frame has no inverse.
pub fn sgi_gl_sprite_eye(c: &SGIGLContext, rot: &[f64; 9], eye: &mut [f64; 3]) -> bool {
    let m = &c.cur.modelview;
    let t = &c.sprite_translation;
    let mut f = [0.0f32; 16];
    let mut inv = [0.0f64; 16];

    // f = M T rot, column-major like M: f[col * 4 + row].
    for row in 0..4 {
        for col in 0..3 {
            f[col * 4 + row] = (m[row] as f64 * rot[col]
                + m[4 + row] as f64 * rot[3 + col]
                + m[8 + row] as f64 * rot[6 + col]) as f32;
        }
        f[12 + row] = m[row] * t[0] + m[4 + row] * t[1] + m[8 + row] * t[2] + m[12 + row];
    }
    if !sgi_gl_invert(&f, &mut inv) || inv[15] == 0.0 {
        return false;
    }
    for k in 0..3 {
        eye[k] = inv[12 + k] / inv[15];
    }
    true
}

/// The SGIX_sprite transformation of the current state: the sprite
/// geometry's object coordinates go through s4 (column-major, T A R)
/// before the modelview, and its normals through rot (row-major, A R).
///
/// A turns the canonical up vector, +Z, to the sprite axis, and R turns
/// the geometry about +Z so that its front vector, -Y, faces the eye: R's
/// cosine is the eye vector's dot product with -Y and its sine its dot
/// product with +X, the eye vector made unit length in the plane normal
/// to +Z. In the axial mode the axis is the one given and the eye vector
/// is found in the frame M T A; in the two aligned modes it is found in
/// M T, the axis is the given one less its component along the eye
/// vector, given in object coordinates or, eye-aligned, in eye
/// coordinates and taken back through M, and R uses the eye vector in the
/// frame after A. The eye vector runs from the sprite's origin to the
/// eye coordinates' origin.
pub fn sgi_gl_sprite_transform(c: &SGIGLContext, s4: &mut [f64; 16], rot: &mut [f64; 9]) {
    let ident: [f64; 9] = [1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0];
    let mut axis = [0.0f64; 3];
    let mut a = [0.0f64; 9];
    let mut eye = [0.0f64; 3];
    let mut spin: [f64; 9] = [1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0];

    for k in 0..3 {
        axis[k] = c.sprite_axis[k] as f64;
    }
    let mut len = (axis[0] * axis[0] + axis[1] * axis[1] + axis[2] * axis[2]).sqrt();
    if c.sprite_mode == SGI_GL_SPRITE_AXIAL_SGIX {
        if len > 0.0 {
            for k in 0..3 {
                axis[k] /= len;
            }
            sgi_gl_sprite_tilt(&axis, &mut a);
        } else {
            a = ident;
        }
    } else if sgi_gl_sprite_eye(c, &ident, &mut eye) {
        let e = (eye[0] * eye[0] + eye[1] * eye[1] + eye[2] * eye[2]).sqrt();
        let mut dot = 0.0f64;
        let mut v = [0.0f64; 3];

        if c.sprite_mode == SGI_GL_SPRITE_EYE_ALIGNED_SGIX {
            let mut m3 = c.cur.modelview;
            let mut inv = [0.0f64; 16];

            // The axis back through M's upper 3x3: a direction.
            m3[12] = 0.0;
            m3[13] = 0.0;
            m3[14] = 0.0;
            if sgi_gl_invert(&m3, &mut inv) {
                for k in 0..3 {
                    v[k] = inv[k] * c.sprite_axis[0] as f64
                        + inv[4 + k] * c.sprite_axis[1] as f64
                        + inv[8 + k] * c.sprite_axis[2] as f64;
                }
                axis = v;
            }
        }
        if e > 0.0 {
            for k in 0..3 {
                eye[k] /= e;
                dot += eye[k] * axis[k];
            }
        }
        for k in 0..3 {
            v[k] = axis[k] - dot * eye[k];
        }
        len = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
        if len > 0.0 {
            for k in 0..3 {
                v[k] /= len;
            }
            sgi_gl_sprite_tilt(&v, &mut a);
        } else {
            a = ident;
        }
    } else {
        a = ident;
    }
    if sgi_gl_sprite_eye(c, &a, &mut eye) {
        let h = eye[0].hypot(eye[1]);

        if h > 0.0 {
            let cs = -eye[1] / h;
            let sn = eye[0] / h;

            spin[0] = cs;
            spin[1] = -sn;
            spin[3] = sn;
            spin[4] = cs;
        }
    }
    sgi_gl_mul3(&a, &spin, rot);
    for row in 0..3 {
        for col in 0..3 {
            s4[col * 4 + row] = rot[row * 3 + col];
        }
        s4[12 + row] = c.sprite_translation[row] as f64;
        s4[row * 4 + 3] = 0.0;
    }
    s4[15] = 1.0;
}

/// glSpriteParameter*SGIX: the axis or translation's three values. Refuses,
/// changing nothing, a pname SGIX_sprite does not name and a non-finite
/// value.
pub fn sgi_gl_sprite_parameter(c: &mut SGIGLContext, pname: u32, v: &[f32; 3]) -> bool {
    let dst = if pname == SGI_GL_SPRITE_AXIS_SGIX {
        &mut c.sprite_axis
    } else if pname == SGI_GL_SPRITE_TRANSLATION_SGIX {
        &mut c.sprite_translation
    } else {
        return false;
    };

    if !v[0].is_finite() || !v[1].is_finite() || !v[2].is_finite() {
        return false;
    }
    *dst = *v;
    true
}

pub fn sgi_gl_sprite_mode(c: &mut SGIGLContext, mode: u32) -> bool {
    if mode != SGI_GL_SPRITE_AXIAL_SGIX
        && mode != SGI_GL_SPRITE_OBJECT_ALIGNED_SGIX
        && mode != SGI_GL_SPRITE_EYE_ALIGNED_SGIX
    {
        return false;
    }
    c.sprite_mode = mode;
    true
}

/// Capture one vertex with current attributes and lighting, without
/// drawing. `bits` holds `components` words.
pub fn sgi_gl_capture_vertex(
    c: &mut SGIGLContext,
    mode: u32,
    bits: &[u32],
    components: u32,
    out: &mut [f32; SGI_GL_VERTEX_FLOATS],
) {
    let mut normal = [0.0f32; 3];
    let mut obj = [0.0f32; 4];

    *out = [0.0; SGI_GL_VERTEX_FLOATS];
    out[SGI_GL_V_POSITION + 3] = 1.0;
    for i in 0..std::cmp::min(components as usize, bits.len()) {
        out[SGI_GL_V_POSITION + i] = sgi_gl_float(bits[i]);
    }
    out[SGI_GL_V_COLOR..SGI_GL_V_COLOR + 4].copy_from_slice(&c.cur_color);
    normal = c.cur_normal;
    obj.copy_from_slice(&out[SGI_GL_V_POSITION..SGI_GL_V_POSITION + 4]);
    if c.sprite != 0 {
        let mut s4 = [0.0f64; 16];
        let mut rot = [0.0f64; 9];
        let mut p = [0.0f64; 4];

        sgi_gl_sprite_transform(c, &mut s4, &mut rot);
        for row in 0..4 {
            p[row] = s4[row] * out[SGI_GL_V_POSITION] as f64
                + s4[4 + row] * out[SGI_GL_V_POSITION + 1] as f64
                + s4[8 + row] * out[SGI_GL_V_POSITION + 2] as f64
                + s4[12 + row] * out[SGI_GL_V_POSITION + 3] as f64;
        }
        for row in 0..4 {
            out[SGI_GL_V_POSITION + row] = p[row] as f32;
        }
        for row in 0..3 {
            normal[row] = (rot[row * 3] * c.cur_normal[0] as f64
                + rot[row * 3 + 1] * c.cur_normal[1] as f64
                + rot[row * 3 + 2] * c.cur_normal[2] as f64) as f32;
        }
    }
    out[SGI_GL_V_NORMAL..SGI_GL_V_NORMAL + 3].copy_from_slice(&normal);
    let mut pos = [0.0f32; 4];
    pos.copy_from_slice(&out[SGI_GL_V_POSITION..SGI_GL_V_POSITION + 4]);
    let mut tc = [0.0f32; 4];
    sgi_gl_texture_coord(c, &obj, &pos, &normal, &mut tc);
    out[SGI_GL_V_TEXCOORD..SGI_GL_V_TEXCOORD + 4].copy_from_slice(&tc);
    out[SGI_GL_V_EDGE] = f32::from_bits(c.cur_edge);
    out[SGI_GL_V_INDEX] = c.cur_index;
    // Material and lighting belong to the instant this vertex arrived.
    if sgi_gl_lit(c) && c.cur.index_bits != 0 {
        let mut front = 0.0f32;
        let mut back = 0.0f32;

        sgi_gl_light_index_vertex(c, &pos, &normal, &mut front, &mut back);
        out[SGI_GL_V_INDEX] = front;
        out[SGI_GL_V_BACK_INDEX] = back;
    } else if sgi_gl_lit(c) {
        let mut front = [0.0f32; 4];
        let mut back = [0.0f32; 4];

        sgi_gl_light_vertex(c, &pos, &normal, &mut front, &mut back);
        out[SGI_GL_V_COLOR..SGI_GL_V_COLOR + 4].copy_from_slice(&front);
        out[SGI_GL_V_BACK_COLOR..SGI_GL_V_BACK_COLOR + 4].copy_from_slice(&back);
        out[SGI_GL_V_BACK_INDEX] = out[SGI_GL_V_INDEX];
    } else {
        out[SGI_GL_V_BACK_COLOR..SGI_GL_V_BACK_COLOR + 4].copy_from_slice(&c.cur_color);
        out[SGI_GL_V_BACK_INDEX] = out[SGI_GL_V_INDEX];
    }
    if c.polygon.enabled != 0
        && (mode == SGI_GL_TRIANGLE_STRIP || mode == SGI_GL_TRIANGLE_FAN || mode == SGI_GL_QUAD_STRIP)
    {
        out[SGI_GL_V_EDGE] = f32::from_bits(1);
    }
}

/// One vertex of the primitive open since glBegin (OpenGL 1.2.1, 2.6.1),
/// carrying the current attributes. The context holds at most three
/// earlier vertices, in held[] with nheld of them in use, and every piece
/// is sent as its own draw as soon as its last vertex arrives, so a
/// context switch between two vertices loses nothing. A line loop's
/// closing segment is sent at glEnd. Vertices that complete no piece
/// before glEnd draw nothing.
pub fn sgi_gl_vertex(c: &mut SGIGLContext, sink: &SGIGLSink, pos: &[f32; 4], components: u32) {
    let mut cur = [0.0f32; SGI_GL_VERTEX_FLOATS];
    let h0 = c.held[0];
    let h1 = c.held[1];
    let h2 = c.held[2];
    let n = c.nheld;

    if c.in_begin == 0 {
        crate::qlog!("sgi-gl: vertex outside glBegin ignored");
        return;
    }
    let bits = [pos[0].to_bits(), pos[1].to_bits(), pos[2].to_bits(), pos[3].to_bits()];
    let mode = c.begin_mode;
    sgi_gl_capture_vertex(c, mode, &bits, components, &mut cur);
    if c.polygon.enabled != 0 && c.begin_mode == SGI_GL_POLYGON {
        let p = &mut c.polygon;

        if p.failed != 0 {
            return;
        }
        if p.count == u32::MAX || !sgi_gl_polygon_reserve(p, p.count as usize + 1) {
            sgi_gl_polygon_release(p);
            p.failed = 1;
            if p.error == 0 {
                p.error = SGI_GL_OUT_OF_MEMORY;
            }
            return;
        }
        let at = p.count as usize * SGI_GL_VERTEX_FLOATS;
        p.vertices[at..at + SGI_GL_VERTEX_FLOATS].copy_from_slice(&cur);
        p.count += 1;
        return;
    }

    match c.begin_mode {
        SGI_GL_POINTS => {
            sgi_gl_submit(c, sink, SGI_GL_POINTS, 1, Some(&cur), None, None, None);
            return;
        }
        SGI_GL_LINES => {
            if n >= 1 {
                sgi_gl_submit(c, sink, SGI_GL_LINES, 2, Some(&h0), Some(&cur), None, None);
                c.nheld = 0;
                return;
            }
        }
        SGI_GL_LINE_STRIP => {
            if n >= 1 {
                sgi_gl_submit(c, sink, SGI_GL_LINES, 2, Some(&h0), Some(&cur), None, None);
                sgi_gl_hold(c, 0, &cur);
                return;
            }
        }
        SGI_GL_LINE_LOOP => {
            // Slot 0 keeps the first vertex for the closing segment.
            if n >= 1 {
                let prev = c.held[n as usize - 1];
                sgi_gl_submit(c, sink, SGI_GL_LINES, 2, Some(&prev), Some(&cur), None, None);
                sgi_gl_hold(c, 1, &cur);
                c.nheld = 2;
                return;
            }
        }
        SGI_GL_TRIANGLES => {
            if n >= 2 {
                sgi_gl_submit(c, sink, SGI_GL_TRIANGLES, 3, Some(&h0), Some(&h1), Some(&cur), None);
                c.nheld = 0;
                return;
            }
        }
        SGI_GL_TRIANGLE_STRIP => {
            if n >= 2 {
                if c.strip_swap != 0 && c.strip_prev != 0 {
                    // After a swap the vertex replaces the middle one of the
                    // three the strip keeps, so its triangle shares the
                    // strip's oldest and newest vertices, and keeps the
                    // facing of the triangle before it, as a fan's does. The
                    // oldest stays the oldest for the next swap.
                    if c.strip_odd != 0 {
                        sgi_gl_submit(c, sink, SGI_GL_TRIANGLES, 3, Some(&h2), Some(&h1), Some(&cur), None);
                    } else {
                        sgi_gl_submit(c, sink, SGI_GL_TRIANGLES, 3, Some(&h1), Some(&h2), Some(&cur), None);
                    }
                    c.strip_swap = 0;
                    sgi_gl_hold(c, 0, &h1);
                    sgi_gl_hold(c, 1, &cur);
                    return;
                }
                c.strip_swap = 0;
                if c.strip_odd != 0 {
                    sgi_gl_submit(c, sink, SGI_GL_TRIANGLES, 3, Some(&h1), Some(&h0), Some(&cur), None);
                } else {
                    sgi_gl_submit(c, sink, SGI_GL_TRIANGLES, 3, Some(&h0), Some(&h1), Some(&cur), None);
                }
                c.strip_odd = (c.strip_odd == 0) as u32;
                sgi_gl_hold(c, 2, &h0);
                c.strip_prev = 1;
                sgi_gl_hold(c, 0, &h1);
                sgi_gl_hold(c, 1, &cur);
                return;
            }
        }
        SGI_GL_TRIANGLE_FAN => {
            if n >= 2 {
                sgi_gl_submit(c, sink, SGI_GL_TRIANGLES, 3, Some(&h0), Some(&h1), Some(&cur), None);
                sgi_gl_hold(c, 1, &cur);
                return;
            }
        }
        SGI_GL_POLYGON => {
            if n >= 2 {
                sgi_gl_submit(c, sink, SGI_GL_POLYGON, 3, Some(&h0), Some(&h1), Some(&cur), None);
                sgi_gl_hold(c, 1, &cur);
                return;
            }
        }
        SGI_GL_QUADS => {
            if n >= 3 {
                sgi_gl_submit(c, sink, SGI_GL_QUADS, 4, Some(&h0), Some(&h1), Some(&h2), Some(&cur));
                c.nheld = 0;
                return;
            }
        }
        SGI_GL_QUAD_STRIP => {
            if n >= 3 {
                sgi_gl_submit(c, sink, SGI_GL_QUAD_STRIP, 4, Some(&h0), Some(&h1), Some(&h2), Some(&cur));
                sgi_gl_hold(c, 0, &h2);
                sgi_gl_hold(c, 1, &cur);
                c.nheld = 2;
                return;
            }
        }
        _ => unreachable!("sgi-gl: begin mode past GL_POLYGON"),
    }
    sgi_gl_hold(c, n, &cur);
    c.nheld = n + 1;
}

/// glRasterPos (OpenGL 1.2.1, 2.12): the point goes through the modelview
/// and projection matrices like a vertex. Inside the view volume and on
/// the kept side of every enabled client clip plane (2.11) it
/// becomes the current raster position in window coordinates, keeping
/// clip w, with the eye distance, the current colour clamped to [0, 1]
/// (2.13.6), or with lighting enabled the front colour the current normal
/// lights there, and the current colour index.
/// Outside, only the valid bit is reset: the specification leaves the
/// rest undefined, and keeping it is the model's choice. Window depth goes
/// through the depth range, as for a draw. The eye distance is the
/// exact Euclidean one of the eye x, y and z. A window matrix, when
/// the draw state has one, takes the projection's, the viewport's and the
/// depth range's places here as in a draw.
pub fn sgi_gl_raster_pos(c: &mut SGIGLContext, obj: &[f32; 4]) {
    let mv = &c.cur.modelview;
    let mut eye = [0.0f64; 4];
    let mut clip = [0.0f64; 4];
    let mut win = [0.0f64; 3];

    for row in 0..4 {
        eye[row] = 0.0;
        for k in 0..4 {
            eye[row] += (mv[k * 4 + row] as f64) * obj[k] as f64;
        }
    }
    sgi_gl_eye_to_clip(&c.cur, &eye, &mut clip);
    // Clip (0, 0, 0, 0) passes the view-volume test and has no window
    // position; it is culled, the model's choice for a point with no image.
    for i in 0..3 {
        if !(clip[3] > 0.0) || !(-clip[3] <= clip[i] && clip[i] <= clip[3]) {
            c.raster_valid = 0;
            return;
        }
    }
    for i in 0..SGI_GL_CLIP_PLANES as usize {
        let p = &c.cur.clip_plane[i];

        if (c.cur.clip_planes & (1u32 << i)) != 0
            && !(p[0] as f64 * eye[0] + p[1] as f64 * eye[1] + p[2] as f64 * eye[2] + p[3] as f64 * eye[3]
                >= 0.0)
        {
            c.raster_valid = 0;
            return;
        }
    }
    c.raster_valid = 1;
    sgi_gl_clip_to_window(&c.cur, &clip, &mut win);
    c.raster_pos[0] = win[0] as f32;
    c.raster_pos[1] = win[1] as f32;
    c.raster_pos[2] = (if c.cur.window_matrix_given != 0 {
        win[2]
    } else {
        c.cur.depth_range[0] as f64
            + ((c.cur.depth_range[1] - c.cur.depth_range[0]) as f64) * win[2]
    }) as f32;
    c.raster_pos[3] = clip[3] as f32;
    c.raster_distance = (eye[0] * eye[0] + eye[1] * eye[1] + eye[2] * eye[2]).sqrt() as f32;
    let normal = c.cur_normal;
    if sgi_gl_lit(c) && c.cur.index_bits != 0 {
        let mut idx = 0.0f32;
        let mut back = 0.0f32;

        sgi_gl_light_index_vertex(c, obj, &normal, &mut idx, &mut back);
        c.raster_index = idx;
    } else if sgi_gl_lit(c) {
        let mut front = [0.0f32; 4];
        let mut back = [0.0f32; 4];

        sgi_gl_light_vertex(c, obj, &normal, &mut front, &mut back);
        c.raster_color = front;
    } else {
        for i in 0..4 {
            c.raster_color[i] = sgi_gl_c_min(sgi_gl_c_max(c.cur_color[i], 0.0), 1.0);
        }
        c.raster_index = c.cur_index;
    }
    let mut tc = [0.0f32; 4];
    sgi_gl_texture_coord(c, obj, obj, &normal, &mut tc);
    c.raster_texcoord = tc;
}

/// glDepthRange (OpenGL 1.2.1, 2.10.1): near and far are each clamped to
/// [0, 1] and kept, far may be below near, and a value that is not a number
/// clamps to 0, the model's choice. Draws and glRasterPos map normalized
/// depth through the range.
pub fn sgi_gl_depth_range(c: &mut SGIGLContext, n: f32, f: f32) {
    let mut range = [n, f];

    for i in 0..2 {
        range[i] = if range[i] >= 1.0 {
            1.0
        } else if range[i] > 0.0 {
            range[i]
        } else {
            0.0
        };
        c.cur.depth_range[i] = range[i];
    }
}

/// The faces a glMaterial, glColorMaterial or glCullFace face names, as a
/// mask of material[] entries, or 0 for a value that names none.
pub fn sgi_gl_faces(face: u32) -> u32 {
    match face {
        SGI_GL_FRONT => 1,
        SGI_GL_BACK => 2,
        SGI_GL_FRONT_AND_BACK => 3,
        _ => 0,
    }
}

/// One material parameter of the faces in the mask faces (2.13.2). v holds
/// four floats; GL_SHININESS uses the first and GL_COLOR_INDEXES three.
/// Shininess outside [0, 128] is an error and changes nothing.
pub fn sgi_gl_material(c: &mut SGIGLContext, faces: u32, pname: u32, v: &[f32; 4]) {
    if pname == SGI_GL_SHININESS && !(v[0] >= 0.0 && v[0] <= 128.0) {
        crate::qlog!("sgi-gl: material shininess {} ignored", v[0]);
        return;
    }
    for i in 0..2usize {
        let m = &mut c.material[i];

        if faces & (1u32 << i) == 0 {
            continue;
        }
        match pname {
            SGI_GL_AMBIENT => m.ambient = *v,
            SGI_GL_DIFFUSE => m.diffuse = *v,
            SGI_GL_AMBIENT_AND_DIFFUSE => {
                m.ambient = *v;
                m.diffuse = *v;
            }
            SGI_GL_SPECULAR => m.specular = *v,
            SGI_GL_EMISSION => m.emission = *v,
            SGI_GL_SHININESS => m.shininess = v[0],
            SGI_GL_COLOR_INDEXES => m.color_indexes.copy_from_slice(&v[..3]),
            _ => {
                crate::qlog!("sgi-gl: material parameter {:#x} ignored", pname);
                return;
            }
        }
    }
}

/// With GL_COLOR_MATERIAL enabled the material parameter glColorMaterial
/// chose tracks the current colour (2.13.3): each colour command sets it,
/// and so do enabling and a new glColorMaterial while enabled, since from
/// then on the parameter is the current colour. Colour-index lighting
/// takes no part.
pub fn sgi_gl_track_colour(c: &mut SGIGLContext) {
    if c.color_material != 0 {
        let faces = sgi_gl_faces(c.color_material_face);
        let mode = c.color_material_mode;
        let colour = c.cur_color;

        sgi_gl_material(c, faces, mode, &colour);
    }
}

pub fn sgi_gl_transform(m: &[f32; 16], input: &[f32; 4], n: u32, out: &mut [f32; 4]) {
    for row in 0..4 {
        let mut v = 0.0f64;

        for k in 0..n as usize {
            v += (m[k * 4 + row] as f64) * input[k] as f64;
        }
        out[row] = v as f32;
    }
}

/// The cosine glLightfv stores for a spot cutoff of degrees: -1 for 180,
/// no spot, and otherwise the single-precision cosine.
pub fn sgi_gl_spot_cos(degrees: f32) -> f32 {
    if degrees == 180.0 {
        -1.0
    } else {
        (degrees * (std::f64::consts::PI as f32) / 180.0).cos()
    }
}

/// glLightfv (2.13.2). The position is transformed by the modelview
/// current at the call and the spot direction by its upper left 3x3, so
/// both are kept in eye coordinates. A spot exponent outside [0, 128], a
/// cutoff outside [0, 90] other than 180 or a negative attenuation is an
/// error and changes nothing.
pub fn sgi_gl_light(c: &mut SGIGLContext, n: u32, pname: u32, v: &[f32; 4]) {
    let mut eye = [0.0f32; 4];

    if n >= SGI_GL_LIGHTS {
        crate::qlog!("sgi-gl: light {} ignored", n);
        return;
    }
    let mv = c.cur.modelview;
    let l = &mut c.light[n as usize];
    match pname {
        SGI_GL_AMBIENT => {
            l.ambient = *v;
            return;
        }
        SGI_GL_DIFFUSE => {
            l.diffuse = *v;
            return;
        }
        SGI_GL_SPECULAR => {
            l.specular = *v;
            return;
        }
        SGI_GL_POSITION => {
            sgi_gl_transform(&mv, v, 4, &mut l.position);
            return;
        }
        SGI_GL_SPOT_DIRECTION => {
            sgi_gl_transform(&mv, v, 3, &mut eye);
            l.spot_direction.copy_from_slice(&eye[..3]);
            return;
        }
        SGI_GL_SPOT_EXPONENT => {
            if v[0] >= 0.0 && v[0] <= 128.0 {
                l.spot_exponent = v[0];
                return;
            }
        }
        SGI_GL_SPOT_CUTOFF => {
            if (v[0] >= 0.0 && v[0] <= 90.0) || v[0] == 180.0 {
                l.spot_cutoff = v[0];
                l.spot_cos_cutoff = sgi_gl_spot_cos(v[0]);
                return;
            }
        }
        SGI_GL_CONSTANT_ATTENUATION | SGI_GL_LINEAR_ATTENUATION | SGI_GL_QUADRATIC_ATTENUATION => {
            if v[0] >= 0.0 {
                l.attenuation[(pname - SGI_GL_CONSTANT_ATTENUATION) as usize] = v[0];
                return;
            }
        }
        _ => {}
    }
    crate::qlog!("sgi-gl: light parameter {:#x} value {} ignored", pname, v[0]);
}

/// Set light number light (0 for GL_LIGHT0) from values a family has
/// decoded from its board's own encoding, already in eye coordinates. False,
/// and logged, for a light the context does not keep.
pub fn sgi_gl_set_light(c: &mut SGIGLContext, light: u32, e: &SGIGLEyeLight) -> bool {
    if light >= SGI_GL_LIGHTS {
        crate::qlog!("sgi-gl: light {} ignored", light);
        return false;
    }
    let l = &mut c.light[light as usize];
    l.position = e.position;
    l.spot_direction = e.spot_direction;
    l.spot_exponent = e.spot_exponent;
    l.attenuation = e.attenuation;
    l.ambient = e.ambient;
    l.diffuse = e.diffuse;
    l.specular = e.specular;
    // The cosine is kept as given, so the cone's edge falls where the
    // family's board puts it; the degrees are only glGet's answer.
    if e.spot_cos_cutoff <= -1.0 {
        l.spot_cos_cutoff = -1.0;
        l.spot_cutoff = 180.0;
    } else {
        l.spot_cos_cutoff = sgi_gl_c_min(e.spot_cos_cutoff, 1.0);
        l.spot_cutoff = ((l.spot_cos_cutoff as f64).acos() * 180.0 / std::f64::consts::PI) as f32;
    }
    true
}

/// Take normals to eye coordinates by m, column-major 3x3, used as given;
/// None returns to the modelview's inverse transpose (2.10.3).
pub fn sgi_gl_set_normal_matrix(c: &mut SGIGLContext, m: Option<&[f32; 9]>) {
    c.normal_matrix_given = m.is_some() as u32;
    match m {
        Some(m) => c.normal_matrix = *m,
        None => c.normal_matrix = [0.0; 9],
    }
}

/// Draw through the window matrix m, column-major, with depth scale
/// depth_scale and rows running down when rows_down; None returns to the
/// projection and viewport, and clears all three.
pub fn sgi_gl_set_window_matrix(
    c: &mut SGIGLContext,
    m: Option<&[f32; 16]>,
    depth_scale: f32,
    rows_down: bool,
) {
    let st = &mut c.cur;

    st.window_matrix_given = m.is_some() as u32;
    match m {
        Some(m) => st.window_matrix = *m,
        None => st.window_matrix = [0.0; 16],
    }
    st.window_depth_scale = if m.is_some() { depth_scale } else { 0.0 };
    st.window_rows_down = (m.is_some() && rows_down) as u32;
}

/// An eye position's clip coordinates (2.10) under the draw state: by the
/// projection, or under a window matrix the window matrix's h with x and y
/// scaled by 1 / SGI_GL_WINDOW_GUARD and z by 2 window_depth_scale, less w.
pub fn sgi_gl_eye_to_clip(st: &SGIGLDrawState, eye: &[f64; 4], clip: &mut [f64; 4]) {
    let m = if st.window_matrix_given != 0 { &st.window_matrix } else { &st.projection };

    for row in 0..4 {
        clip[row] = 0.0;
        for k in 0..4 {
            clip[row] += (m[k * 4 + row] as f64) * eye[k];
        }
    }
    if st.window_matrix_given != 0 {
        clip[0] /= SGI_GL_WINDOW_GUARD as f64;
        clip[1] /= SGI_GL_WINDOW_GUARD as f64;
        clip[2] = 2.0 * st.window_depth_scale as f64 * clip[2] - clip[3];
    }
}

/// Clip coordinates as sgi_gl_eye_to_clip() gives them to window x and y
/// and normalized depth, (z / w + 1) / 2, in out. w must be positive.
pub fn sgi_gl_clip_to_window(st: &SGIGLDrawState, clip: &[f64; 4], out: &mut [f64; 3]) {
    let vp = &st.viewport;

    if st.window_matrix_given != 0 {
        out[0] = clip[0] * SGI_GL_WINDOW_GUARD as f64 / clip[3];
        out[1] = clip[1] * SGI_GL_WINDOW_GUARD as f64 / clip[3];
    } else {
        // 2.10.1: xw = (px / 2) xd + ox, ox = x + w / 2.
        out[0] = (clip[0] / clip[3] + 1.0) * vp[2] as f64 / 2.0 + vp[0] as f64;
        out[1] = (clip[1] / clip[3] + 1.0) * vp[3] as f64 / 2.0 + vp[1] as f64;
    }
    out[2] = (clip[2] / clip[3] + 1.0) / 2.0;
}

/// Make every eye normal unit length, whatever GL_NORMALIZE's enable.
pub fn sgi_gl_set_normalize_always(c: &mut SGIGLContext, on: bool) {
    c.normalize_always = on as u32;
}

pub fn sgi_gl_light_model(c: &mut SGIGLContext, pname: u32, v: &[f32; 4]) {
    match pname {
        SGI_GL_LIGHT_MODEL_AMBIENT => c.light_model_ambient = *v,
        SGI_GL_LIGHT_MODEL_LOCAL_VIEWER => c.light_model_local_viewer = (v[0] != 0.0) as u32,
        SGI_GL_LIGHT_MODEL_TWO_SIDE => c.light_model_two_side = (v[0] != 0.0) as u32,
        _ => {
            crate::qlog!("sgi-gl: light model parameter {:#x} ignored", pname);
        }
    }
}

/// glFogfv (3.10). A negative density or a mode other than GL_LINEAR,
/// GL_EXP or GL_EXP2 is an error and changes nothing; the colour is
/// clamped to [0, 1].
pub fn sgi_gl_fog(c: &mut SGIGLContext, pname: u32, v: &[f32; 4]) {
    let mode: u32 = if v[0] >= 0.0 && v[0] < 65536.0 { v[0] as u32 } else { 0 };

    match pname {
        SGI_GL_FOG_MODE => {
            if mode == SGI_GL_LINEAR || mode == SGI_GL_EXP || mode == SGI_GL_EXP2 {
                c.cur.fog_mode = mode;
                return;
            }
        }
        SGI_GL_FOG_DENSITY => {
            if v[0] >= 0.0 {
                c.cur.fog_density = v[0];
                return;
            }
        }
        SGI_GL_FOG_START => {
            c.cur.fog_start = v[0];
            return;
        }
        SGI_GL_FOG_END => {
            c.cur.fog_end = v[0];
            return;
        }
        SGI_GL_FOG_INDEX => {
            c.cur.fog_index = v[0];
            return;
        }
        SGI_GL_FOG_COLOR => {
            for i in 0..4 {
                c.cur.fog_color[i] = if v[i] >= 1.0 {
                    1.0
                } else if v[i] > 0.0 {
                    v[i]
                } else {
                    0.0
                };
            }
            return;
        }
        _ => {}
    }
    crate::qlog!("sgi-gl: glFog parameter {:#x} value {} ignored", pname, v[0]);
}

pub fn sgi_gl_color_material(c: &mut SGIGLContext, face: u32, mode: u32) {
    let mode = match mode {
        SGI_GL_AMBIENT | SGI_GL_DIFFUSE | SGI_GL_SPECULAR | SGI_GL_EMISSION
        | SGI_GL_AMBIENT_AND_DIFFUSE => mode,
        _ => 0,
    };
    if sgi_gl_faces(face) == 0 || mode == 0 {
        crate::qlog!("sgi-gl: glColorMaterial({:#x}, {:#x}) ignored", face, mode);
        return;
    }
    c.color_material_face = face;
    c.color_material_mode = mode;
    sgi_gl_track_colour(c);
}

/// Whether op is one of glStencilOp's operations (4.1.5).
pub fn sgi_gl_stencil_op_valid(op: u32) -> bool {
    matches!(op, 0 | SGI_GL_KEEP | SGI_GL_REPLACE | SGI_GL_INCR | SGI_GL_DECR | SGI_GL_INVERT)
}

/// Whether glBlendFunc accepts factor as a source factor (OpenGL 1.2.1,
/// table 4.1, with the imaging subset's constant factors) or, with dst,
/// as a destination one (table 4.2).
pub fn sgi_gl_blend_factor_valid(factor: u32, dst: bool) -> bool {
    match factor {
        0
        | 1
        | SGI_GL_SRC_ALPHA
        | SGI_GL_ONE_MINUS_SRC_ALPHA
        | SGI_GL_DST_ALPHA
        | SGI_GL_ONE_MINUS_DST_ALPHA
        | SGI_GL_CONSTANT_COLOR
        | SGI_GL_ONE_MINUS_CONSTANT_COLOR
        | SGI_GL_CONSTANT_ALPHA
        | SGI_GL_ONE_MINUS_CONSTANT_ALPHA => true,
        SGI_GL_SRC_COLOR | SGI_GL_ONE_MINUS_SRC_COLOR => dst,
        SGI_GL_DST_COLOR | SGI_GL_ONE_MINUS_DST_COLOR | SGI_GL_SRC_ALPHA_SATURATE => !dst,
        _ => false,
    }
}

/// Whether glBlendEquation accepts equation.
pub fn sgi_gl_blend_equation_valid(equation: u32) -> bool {
    matches!(
        equation,
        SGI_GL_FUNC_ADD
            | SGI_GL_MIN
            | SGI_GL_MAX
            | SGI_GL_FUNC_SUBTRACT
            | SGI_GL_FUNC_REVERSE_SUBTRACT
            | SGI_GL_LOGIC_OP
    )
}

/// glClipPlane (2.11): the plane p in object coordinates becomes p M^-1 in
/// eye coordinates, M the modelview current at the call. A plane number
/// the context does not keep, or a modelview with no inverse, which the
/// specification leaves undefined, changes nothing.
pub fn sgi_gl_clip_plane(c: &mut SGIGLContext, plane: u32, p: &[f32; 4]) {
    let mut inv = [0.0f64; 16];

    if plane >= SGI_GL_CLIP_PLANES || !sgi_gl_invert(&c.cur.modelview, &mut inv) {
        crate::qlog!("sgi-gl: glClipPlane of plane {} ignored", plane);
        return;
    }
    for j in 0..4 {
        let mut v = 0.0f64;

        // (p M^-1)_j = sum_i p_i M^-1[i][j], M^-1[i][j] at inv[j * 4 + i].
        for i in 0..4 {
            v += p[i] as f64 * inv[j * 4 + i];
        }
        c.cur.clip_plane[plane as usize][j] = v as f32;
    }
}

/// The rasterization enables kept as raster_enables bits; 0 for any other
/// capability.
pub fn sgi_gl_raster_enable(cap: u32) -> u32 {
    match cap {
        SGI_GL_POINT_SMOOTH => SGI_GL_RASTER_POINT_SMOOTH,
        SGI_GL_LINE_SMOOTH => SGI_GL_RASTER_LINE_SMOOTH,
        SGI_GL_LINE_STIPPLE => SGI_GL_RASTER_LINE_STIPPLE,
        SGI_GL_POLYGON_SMOOTH => SGI_GL_RASTER_POLYGON_SMOOTH,
        SGI_GL_POLYGON_STIPPLE => SGI_GL_RASTER_POLYGON_STIPPLE,
        _ => 0,
    }
}

/// glPointSize, glLineWidth, glLineStipple and glPolygonMode (3.3 to 3.5):
/// a size or width not above 0 or a face or mode the specification lacks
/// is an error and changes nothing, and the repeat clamps to [1, 256].
/// The state is kept and answered, and a value the rasterizer does not
/// draw is logged.
pub fn sgi_gl_raster_state(c: &mut SGIGLContext, op: u16, a: &[u32]) {
    let f = sgi_gl_float(sgi_gl_arg(a, 0));

    match op as u32 {
        SGI_GL_OP_POINT_SIZE | SGI_GL_OP_LINE_WIDTH => {
            if c.in_begin != 0 {
                crate::qlog!("sgi-gl: size setter inside glBegin: invalid operation");
                return;
            }
            if !f.is_finite() || !(f > 0.0) {
                crate::qlog!(
                    "sgi-gl: {}({}) ignored",
                    if op as u32 == SGI_GL_OP_POINT_SIZE { "glPointSize" } else { "glLineWidth" },
                    f
                );
                return;
            }
            if op as u32 == SGI_GL_OP_POINT_SIZE {
                c.point_size = f;
            } else {
                c.line_width = f;
            }
        }
        SGI_GL_OP_LINE_STIPPLE => {
            if c.in_begin != 0 {
                crate::qlog!("sgi-gl: line stipple inside glBegin: invalid operation");
                return;
            }
            let repeat = sgi_gl_arg(a, 0) as i32;
            c.line_stipple_repeat = std::cmp::min(std::cmp::max(repeat, 1), 256) as u32;
            c.line_stipple_pattern = sgi_gl_arg(a, 1) & 0xffff;
        }
        _ => {
            if c.in_begin != 0 {
                crate::qlog!("sgi-gl: polygon mode inside glBegin: invalid operation");
                return;
            }
            let a0 = sgi_gl_arg(a, 0);
            let a1 = sgi_gl_arg(a, 1);
            let faces: u32 = if a0 == SGI_GL_FRONT_AND_BACK {
                3
            } else if a0 == SGI_GL_FRONT {
                1
            } else if a0 == SGI_GL_BACK {
                2
            } else {
                0
            };
            if faces == 0 || a1.wrapping_sub(SGI_GL_POINT) > SGI_GL_FILL - SGI_GL_POINT {
                crate::qlog!("sgi-gl: glPolygonMode({:#x}, {:#x}) ignored", a0, a1);
                return;
            }
            for i in 0..2usize {
                if faces & (1u32 << i) != 0 {
                    c.polygon_mode[i] = a1;
                }
            }
            if a1 != SGI_GL_FILL && c.polygon.enabled == 0 {
                crate::qlog!("sgi-gl: polygon mode {:#x} is kept; polygons are drawn filled", a1);
            }
        }
    }
}

pub fn sgi_gl_capability(c: &mut SGIGLContext, cap: u32, on: bool) {
    let raster = sgi_gl_raster_enable(cap);
    let onv = on as u32;

    if raster != 0 {
        if (cap == SGI_GL_POLYGON_STIPPLE || cap == SGI_GL_LINE_STIPPLE) && c.in_begin != 0 {
            crate::qlog!("sgi-gl: stipple enable inside glBegin: invalid operation");
            return;
        }
        c.raster_enables = if on { c.raster_enables | raster } else { c.raster_enables & !raster };
        if on && cap == SGI_GL_LINE_STIPPLE && !c.standalone_stipple {
            crate::qlog!("sgi-gl: standalone line stipple is kept; standalone lines are drawn without it");
        }
        return;
    }
    match cap {
        SGI_GL_DEPTH_TEST => c.cur.depth_test = onv,
        SGI_GL_DITHER => c.cur.dither = onv,
        SGI_GL_CULL_FACE => c.cur.cull_face = onv,
        SGI_GL_SCISSOR_TEST => c.cur.scissor_test = onv,
        SGI_GL_ALPHA_TEST => c.cur.alpha_test = onv,
        SGI_GL_STENCIL_TEST => c.cur.stencil_test = onv,
        SGI_GL_BLEND => c.cur.blend = onv,
        SGI_GL_INDEX_LOGIC_OP => c.cur.index_logic_op = onv,
        SGI_GL_COLOR_LOGIC_OP => c.cur.color_logic_op = onv,
        SGI_GL_TEXTURE_1D => c.cur.texture_1d = onv,
        SGI_GL_TEXTURE_2D => c.cur.texture_2d = onv,
        SGI_GL_TEXTURE_3D => c.cur.texture_3d = onv,
        SGI_GL_FOG => c.cur.fog = onv,
        SGI_GL_POLYGON_OFFSET_FILL => c.cur.polygon_offset = deposit32(c.cur.polygon_offset, 0, 1, onv),
        SGI_GL_POLYGON_OFFSET_LINE => c.cur.polygon_offset = deposit32(c.cur.polygon_offset, 1, 1, onv),
        SGI_GL_POLYGON_OFFSET_POINT => c.cur.polygon_offset = deposit32(c.cur.polygon_offset, 2, 1, onv),
        SGI_GL_CLIP_PLANE0..=SGI_GL_CLIP_PLANE_LAST => {
            c.cur.clip_planes = deposit32(c.cur.clip_planes, cap - SGI_GL_CLIP_PLANE0, 1, onv);
        }
        SGI_GL_COLOR_MATERIAL => {
            c.color_material = onv;
            sgi_gl_track_colour(c);
        }
        SGI_GL_LIGHTING => {
            c.lighting = onv;
            if on && c.cur.index_bits != 0 {
                crate::qlog!("sgi-gl: colour-index lighting is not modelled; indices stay unlit");
            }
        }
        SGI_GL_NORMALIZE => c.normalize = onv,
        SGI_GL_SPRITE_SGIX => c.sprite = onv,
        SGI_GL_RESCALE_NORMAL => c.rescale_normal = onv,
        SGI_GL_LIGHT0..=SGI_GL_LIGHT_LAST => {
            c.lights_enabled = deposit32(c.lights_enabled, cap - SGI_GL_LIGHT0, 1, onv);
        }
        SGI_GL_TEXTURE_GEN_S..=SGI_GL_TEXTURE_GEN_Q => {
            c.tex_gen_enabled = deposit32(c.tex_gen_enabled, cap - SGI_GL_TEXTURE_GEN_S, 1, onv);
        }
        _ => {
            crate::qlog!(
                "sgi-gl: capability {:#x} is not modelled; {} ignored",
                cap,
                if on { "glEnable" } else { "glDisable" }
            );
        }
    }
}

/// GL_LIGHTING's enable and GL_LIGHT_MODEL_LOCAL_VIEWER, for a family whose
/// board sets them in a state word.
pub fn sgi_gl_set_lighting(c: &mut SGIGLContext, on: bool) {
    sgi_gl_capability(c, SGI_GL_LIGHTING, on);
}

pub fn sgi_gl_set_local_viewer(c: &mut SGIGLContext, on: bool) {
    c.light_model_local_viewer = on as u32;
}

/// glBitmap's placement and move (OpenGL 1.2.1, 3.7). With the raster
/// position valid, the image's lower-left corner goes into *b and the
/// raster position moves by (xmove, ymove); returns true. With it invalid
/// nothing is drawn or moved; returns false.
pub fn sgi_gl_bitmap_place(
    c: &mut SGIGLContext,
    width: u32,
    height: u32,
    xorig: f32,
    yorig: f32,
    xmove: f32,
    ymove: f32,
    b: &mut SGIGLBitmap,
) -> bool {
    if c.raster_valid == 0 {
        return false;
    }
    if !c.raster_pos[0].is_finite()
        || !c.raster_pos[1].is_finite()
        || !xorig.is_finite()
        || !yorig.is_finite()
        || !xmove.is_finite()
        || !ymove.is_finite()
    {
        crate::qlog!("sgi-gl: nonfinite bitmap placement dropped");
        return false;
    }
    // Beyond 2^30 a corner lies off any plane; keep it there.
    let fx = (c.raster_pos[0] as f64 - xorig as f64).floor();
    let fy = (c.raster_pos[1] as f64 - yorig as f64).floor();
    b.x = (if fx > -1073741824.0 { fx } else { -1073741824.0 }).min(1073741824.0) as i32;
    b.y = (if fy > -1073741824.0 { fy } else { -1073741824.0 }).min(1073741824.0) as i32;
    b.width = width;
    b.height = height;
    c.raster_pos[0] += xmove;
    c.raster_pos[1] += ymove;
    true
}

/// The fragments of glBitmap's set bits take the raster position's depth
/// and its colour or colour index (3.7) and pass the per-fragment
/// operations like a primitive's: pixel ownership against the drawable,
/// the depth test and the draw buffers. The viewport does not bound them.
pub fn sgi_gl_bitmap_bytes(
    c: &mut SGIGLContext,
    sink: &SGIGLSink,
    b: &SGIGLBitmap,
    first: u32,
    bytes: &[u8],
    count: u32,
) {
    let mut op = SGIRasterOp::default();
    op.kind = SGI_RASTER_BITMAP;
    op.bitmap.state = P(&mut c.cur as *mut SGIGLDrawState);
    op.bitmap.x = b.x;
    op.bitmap.y = b.y;
    op.bitmap.width = b.width;
    op.bitmap.height = b.height;
    op.bitmap.first = first;
    op.bitmap.count = count;
    op.bitmap.bytes = P(bytes.as_ptr() as *mut u8);
    op.bitmap.z = c.raster_pos[2];
    op.bitmap.index = c.raster_index;
    op.bitmap.distance = c.raster_distance;

    if c.cur.drawable.bound == 0 {
        crate::qlog!("sgi-gl: glBitmap with no window bound dropped");
        return;
    }
    if c.cur.drawable.clip.n == 0 {
        // A window others wholly cover: the image draws nothing.
        return;
    }
    op.bitmap.rgba = c.raster_color;
    op.bitmap.texcoord = c.raster_texcoord;
    op.ntargets = sgi_gl_draw_targets(&c.cur, &mut op.targets);
    op.clip = c.cur.drawable.clip;
    if let Some(f) = sink.raster {
        f(sink.opaque, &op);
    }
}

/// Draw tightly packed RGBA8 pixels (0xRRGGBBAA), bottom row first, at the
/// current raster position with unit zoom and no pixel transfer.
pub fn sgi_gl_draw_pixels_rgba(
    c: &mut SGIGLContext,
    sink: &SGIGLSink,
    width: u32,
    height: u32,
    rgba: &[u32],
) {
    sgi_gl_draw_pixels_rgba_zoom(c, sink, width, height, rgba, 1.0, 1.0);
}

/// As above with explicit pixel zoom; zero scales produce no fragments.
pub fn sgi_gl_draw_pixels_rgba_zoom(
    c: &mut SGIGLContext,
    sink: &SGIGLSink,
    width: u32,
    height: u32,
    rgba: &[u32],
    zoom_x: f32,
    zoom_y: f32,
) {
    sgi_gl_draw_pixels_rgba_transfer(c, sink, width, height, rgba, zoom_x, zoom_y, None);
}

pub type SGIGLPixelRectangleResult = u32;
pub const SGI_GL_PIXELS_REFUSED: SGIGLPixelRectangleResult = 0;
pub const SGI_GL_PIXELS_EMPTY: SGIGLPixelRectangleResult = 1;
pub const SGI_GL_PIXELS_READY: SGIGLPixelRectangleResult = 2;

pub fn sgi_gl_pixel_rectangle(
    c: &mut SGIGLContext,
    sink: Option<&SGIGLSink>,
    call: &str,
    width: u32,
    height: u32,
    zoom_x: f32,
    zoom_y: f32,
    transfer: Option<&SGIGLPixelTransfer>,
    colour: bool,
    op: &mut SGIRasterOp,
) -> SGIGLPixelRectangleResult {
    *op = SGIRasterOp::default();
    op.kind = SGI_RASTER_PIXELS;
    op.pixels.state = P(&mut c.cur as *mut SGIGLDrawState);
    op.pixels.x = c.raster_pos[0] as f64;
    op.pixels.y = c.raster_pos[1] as f64;
    op.pixels.zoom = [zoom_x, zoom_y];
    op.pixels.transfer.scale = [1.0, 1.0, 1.0, 1.0];
    op.pixels.width = width;
    op.pixels.height = height;
    op.pixels.z = c.raster_pos[2];
    op.pixels.distance = c.raster_distance;
    if c.in_begin != 0 {
        crate::qlog!("sgi-gl: {} inside glBegin dropped", call);
        return SGI_GL_PIXELS_REFUSED;
    }
    if c.raster_valid == 0 || width == 0 || height == 0 {
        return SGI_GL_PIXELS_EMPTY;
    }
    if !zoom_x.is_finite() || !zoom_y.is_finite() || !op.pixels.x.is_finite() || !op.pixels.y.is_finite() {
        crate::qlog!("sgi-gl: nonfinite {} placement dropped", call);
        return SGI_GL_PIXELS_REFUSED;
    }
    if zoom_x == 0.0 || zoom_y == 0.0 {
        return SGI_GL_PIXELS_EMPTY;
    }
    if let Some(t) = transfer {
        for k in 0..4 {
            if !t.scale[k].is_finite() || !t.bias[k].is_finite() {
                crate::qlog!("sgi-gl: nonfinite pixel transfer dropped");
                return SGI_GL_PIXELS_REFUSED;
            }
        }
        op.pixels.transfer = *t;
    }
    if colour && c.cur.index_bits != 0 {
        crate::qlog!("sgi-gl: colour-index {} is unsupported; image dropped", call);
        return SGI_GL_PIXELS_REFUSED;
    }
    if c.cur.drawable.bound == 0 {
        crate::qlog!("sgi-gl: {} with no window bound dropped", call);
        return SGI_GL_PIXELS_REFUSED;
    }
    match sink {
        Some(s) if s.raster.is_some() => {}
        _ => {
            crate::qlog!("sgi-gl: {} with no raster sink dropped", call);
            return SGI_GL_PIXELS_REFUSED;
        }
    }
    if c.cur.drawable.clip.n == 0 {
        return SGI_GL_PIXELS_EMPTY;
    }
    op.pixels.texcoord = c.raster_texcoord;
    op.ntargets = sgi_gl_draw_targets(&c.cur, &mut op.targets);
    op.clip = c.cur.drawable.clip;
    SGI_GL_PIXELS_READY
}

/// Apply RGBA scale and bias before fragment operations; None is identity.
pub fn sgi_gl_draw_pixels_rgba_transfer(
    c: &mut SGIGLContext,
    sink: &SGIGLSink,
    width: u32,
    height: u32,
    rgba: &[u32],
    zoom_x: f32,
    zoom_y: f32,
    transfer: Option<&SGIGLPixelTransfer>,
) {
    let mut op = SGIRasterOp::default();

    if sgi_gl_pixel_rectangle(c, Some(sink), "glDrawPixels", width, height, zoom_x, zoom_y, transfer, true, &mut op)
        != SGI_GL_PIXELS_READY
    {
        return;
    }
    if rgba.is_empty() {
        crate::qlog!("sgi-gl: glDrawPixels with no image bound dropped");
        return;
    }
    op.pixels.rgba = P(rgba.as_ptr() as *mut u32);
    (sink.raster.unwrap())(sink.opaque, &op);
}

pub fn sgi_gl_draw_pixels_unquantized_transfer(
    c: &mut SGIGLContext,
    sink: &SGIGLSink,
    width: u32,
    height: u32,
    rgba_float: Option<&[[f32; 4]]>,
    rgba_double: Option<&[[f64; 4]]>,
    zoom_x: f32,
    zoom_y: f32,
    transfer: Option<&SGIGLPixelTransfer>,
) -> bool {
    let mut op = SGIRasterOp::default();
    let count = width as u64 * height as u64;

    let result = sgi_gl_pixel_rectangle(
        c, Some(sink), "glDrawPixels", width, height, zoom_x, zoom_y, transfer, true, &mut op,
    );
    if result != SGI_GL_PIXELS_READY {
        return result == SGI_GL_PIXELS_EMPTY;
    }
    let esize: u64 = if rgba_double.is_some() { 32 } else { 16 };
    if (rgba_float.is_none() && rgba_double.is_none()) || count > u64::MAX / esize {
        return false;
    }
    // Validate the complete image before publishing any fragment.
    for i in 0..count as usize {
        for k in 0..4usize {
            let component: f64 = match rgba_double {
                Some(d) => d[i][k],
                None => rgba_float.unwrap()[i][k] as f64,
            };

            if !component.is_finite() {
                crate::qlog!("sgi-gl: nonfinite glDrawPixels source dropped");
                return false;
            }
        }
    }
    if let Some(f) = rgba_float {
        op.pixels.rgba_float = P(f.as_ptr() as *mut [f32; 4]);
    }
    if let Some(d) = rgba_double {
        op.pixels.rgba_double = P(d.as_ptr() as *mut [f64; 4]);
    }
    (sink.raster.unwrap())(sink.opaque, &op);
    true
}

/// Finite float sources remain unquantized until fragment operations.
pub fn sgi_gl_draw_pixels_float_transfer(
    c: &mut SGIGLContext,
    sink: &SGIGLSink,
    width: u32,
    height: u32,
    rgba: &[[f32; 4]],
    zoom_x: f32,
    zoom_y: f32,
    transfer: Option<&SGIGLPixelTransfer>,
) -> bool {
    sgi_gl_draw_pixels_unquantized_transfer(c, sink, width, height, Some(rgba), None, zoom_x, zoom_y, transfer)
}

/// Preserve normalized integer source precision through pixel transfer.
pub fn sgi_gl_draw_pixels_double_transfer(
    c: &mut SGIGLContext,
    sink: &SGIGLSink,
    width: u32,
    height: u32,
    rgba: &[[f64; 4]],
    zoom_x: f32,
    zoom_y: f32,
    transfer: Option<&SGIGLPixelTransfer>,
) -> bool {
    sgi_gl_draw_pixels_unquantized_transfer(c, sink, width, height, None, Some(rgba), zoom_x, zoom_y, transfer)
}

/// Transferred colour indices use the context's index fragment operations.
pub fn sgi_gl_draw_pixels_index(
    c: &mut SGIGLContext,
    sink: &SGIGLSink,
    width: u32,
    height: u32,
    index: &[u32],
    zoom_x: f32,
    zoom_y: f32,
) -> bool {
    let mut op = SGIRasterOp::default();

    let result = sgi_gl_pixel_rectangle(
        c, Some(sink), "glDrawPixels", width, height, zoom_x, zoom_y, None, false, &mut op,
    );
    if result != SGI_GL_PIXELS_READY {
        return result == SGI_GL_PIXELS_EMPTY;
    }
    if index.is_empty() || c.cur.index_bits == 0 || (width as u64 * height as u64) > u64::MAX / 4 {
        return false;
    }
    op.pixels.index = P(index.as_ptr() as *mut u32);
    (sink.raster.unwrap())(sink.opaque, &op);
    true
}

/// Stencil pixels bypass fragments and obey ownership, scissor and mask.
pub fn sgi_gl_draw_pixels_stencil(
    c: &mut SGIGLContext,
    sink: &SGIGLSink,
    width: u32,
    height: u32,
    stencil: &[u32],
    zoom_x: f32,
    zoom_y: f32,
) -> bool {
    let mut op = SGIRasterOp::default();

    let result = sgi_gl_pixel_rectangle(
        c, Some(sink), "glDrawPixels", width, height, zoom_x, zoom_y, None, false, &mut op,
    );
    if result != SGI_GL_PIXELS_READY {
        return result == SGI_GL_PIXELS_EMPTY;
    }
    if stencil.is_empty() {
        return false;
    }
    op.pixels.stencil = P(stencil.as_ptr() as *mut u32);
    op.ntargets = 0;
    (sink.raster.unwrap())(sink.opaque, &op);
    true
}

/// Depth sources use raster colour and explicit depth scale/bias.
pub fn sgi_gl_draw_pixels_depth_transfer(
    c: &mut SGIGLContext,
    sink: &SGIGLSink,
    width: u32,
    height: u32,
    depth: &[f64],
    zoom_x: f32,
    zoom_y: f32,
    scale: f32,
    bias: f32,
) -> bool {
    let mut op = SGIRasterOp::default();
    let count = width as u64 * height as u64;

    let result = sgi_gl_pixel_rectangle(
        c, Some(sink), "glDrawPixels", width, height, zoom_x, zoom_y, None, false, &mut op,
    );
    if result != SGI_GL_PIXELS_READY {
        return result == SGI_GL_PIXELS_EMPTY;
    }
    if depth.is_empty() || count > u64::MAX / 8 || !scale.is_finite() || !bias.is_finite() {
        return false;
    }
    for i in 0..count as usize {
        if !depth[i].is_finite() {
            crate::qlog!("sgi-gl: nonfinite glDrawPixels depth source dropped");
            return false;
        }
    }
    op.pixels.depth = P(depth.as_ptr() as *mut f64);
    op.pixels.depth_scale = scale;
    op.pixels.depth_bias = bias;
    op.pixels.colour = c.raster_color;
    op.pixels.colour_index = c.raster_index;
    (sink.raster.unwrap())(sink.opaque, &op);
    true
}

/// Copy an RGBA8 source snapshot through pixel transfer and fragment
/// operations. read_rgba gets window-local coordinates and writes one raw
/// RRGGBBAA pixel, returning false on failure. Failed reads or allocation
/// write no fragments. Raster position stays unchanged. False means
/// refusal.
pub fn sgi_gl_copy_pixels_rgba_transfer(
    c: &mut SGIGLContext,
    sink: &SGIGLSink,
    x: i32,
    y: i32,
    width: u32,
    height: u32,
    read_rgba: Option<fn(Opaque, i32, i32, &mut u32) -> bool>,
    source: Opaque,
    zoom_x: f32,
    zoom_y: f32,
    transfer: Option<&SGIGLPixelTransfer>,
) -> bool {
    let mut op = SGIRasterOp::default();
    let count = width as u64 * height as u64;

    let result = sgi_gl_pixel_rectangle(
        c, Some(sink), "glCopyPixels", width, height, zoom_x, zoom_y, transfer, true, &mut op,
    );
    if result != SGI_GL_PIXELS_READY {
        return result == SGI_GL_PIXELS_EMPTY;
    }
    let read_rgba = match read_rgba {
        Some(f)
            if (x as i64 + width as i64 - 1) <= i32::MAX as i64
                && (y as i64 + height as i64 - 1) <= i32::MAX as i64 =>
        {
            f
        }
        _ => {
            crate::qlog!("sgi-gl: glCopyPixels source unavailable or outside coordinate range");
            return false;
        }
    };
    let mut pixels: Vec<u32> = Vec::new();
    if count > (usize::MAX / 4) as u64
        || pixels.try_reserve_exact(count as usize).is_err()
    {
        if c.polygon.error == 0 {
            c.polygon.error = SGI_GL_OUT_OF_MEMORY;
        }
        return false;
    }
    pixels.resize(count as usize, 0);
    // Complete the snapshot before a destination can overwrite its source.
    for row in 0..height {
        for col in 0..width {
            if !read_rgba(
                source,
                (x as i64 + col as i64) as i32,
                (y as i64 + row as i64) as i32,
                &mut pixels[(row as u64 * width as u64 + col as u64) as usize],
            ) {
                crate::qlog!("sgi-gl: glCopyPixels source read failed; image dropped");
                return false;
            }
        }
    }
    op.pixels.rgba = P(pixels.as_mut_ptr());
    (sink.raster.unwrap())(sink.opaque, &op);
    true
}

/// glTexGen (OpenGL 1.2.1, 2.10.4): a coordinate's mode, SPHERE_MAP only
/// for S and T, or its object or eye plane; the eye plane is kept as p M^-1,
/// M the matrix taking object to eye coordinates at the call: the
/// modelview, or under SGIX_sprite's enable the compound M T A R, which
/// the extension says eye planes given inside the enable are transformed
/// by. Anything else, or an eye plane under a matrix with no inverse,
/// changes nothing.
pub fn sgi_gl_tex_gen_param(c: &mut SGIGLContext, a: &[u32]) {
    let a0 = sgi_gl_arg(a, 0);
    let a1 = sgi_gl_arg(a, 1);
    let i = a0.wrapping_sub(SGI_GL_S);
    let mut v = [0.0f32; 4];
    let mut mm = [0.0f32; 16];
    let mut inv = [0.0f64; 16];

    for k in 0..4 {
        v[k] = sgi_gl_float(sgi_gl_arg(a, k + 2));
    }
    if a0 >= SGI_GL_S && a0 <= SGI_GL_Q {
        match a1 {
            SGI_GL_TEXTURE_GEN_MODE => {
                if v[0] == SGI_GL_OBJECT_LINEAR as f32
                    || v[0] == SGI_GL_EYE_LINEAR as f32
                    || (v[0] == SGI_GL_SPHERE_MAP as f32 && i < 2)
                {
                    c.tex_gen_mode[i as usize] = v[0] as u32;
                    return;
                }
            }
            SGI_GL_OBJECT_PLANE => {
                c.tex_gen_object_plane[i as usize] = v;
                return;
            }
            SGI_GL_EYE_PLANE => {
                mm = c.cur.modelview;
                if c.sprite != 0 {
                    let mv = c.cur.modelview;
                    let mut s4 = [0.0f64; 16];
                    let mut rot = [0.0f64; 9];

                    sgi_gl_sprite_transform(c, &mut s4, &mut rot);
                    for col in 0..4 {
                        for row in 0..4 {
                            let mut e = 0.0f64;

                            for k in 0..4 {
                                e += mv[k * 4 + row] as f64 * s4[col * 4 + k];
                            }
                            mm[col * 4 + row] = e as f32;
                        }
                    }
                }
                if sgi_gl_invert(&mm, &mut inv) {
                    for j in 0..4 {
                        let mut p = 0.0f64;

                        for k in 0..4 {
                            p += v[k] as f64 * inv[j * 4 + k];
                        }
                        c.tex_gen_eye_plane[i as usize][j] = p as f32;
                    }
                    return;
                }
            }
            _ => {}
        }
    }
    crate::qlog!("sgi-gl: glTexGen of coordinate {:#x} parameter {:#x} ignored", a0, a1);
}

pub fn sgi_gl_hint_mode(mode: u32) -> bool {
    mode == SGI_GL_DONT_CARE || mode == SGI_GL_FASTEST || mode == SGI_GL_NICEST
}

/// Four floats from command words `a[from..from + 4]`.
pub fn sgi_gl_words4(a: &[u32], from: usize) -> [f32; 4] {
    [
        sgi_gl_float(sgi_gl_arg(a, from)),
        sgi_gl_float(sgi_gl_arg(a, from + 1)),
        sgi_gl_float(sgi_gl_arg(a, from + 2)),
        sgi_gl_float(sgi_gl_arg(a, from + 3)),
    ]
}

/// Execute one command against the context, sending any work to sink.
pub fn sgi_gl_command(c: &mut SGIGLContext, sink: &SGIGLSink, cmd: &SGIGLCmd) {
    let a = cmd.args();
    let mut pos = [0.0f32; 4];

    if cmd.op as usize >= sgi_gl_nargs.len() || cmd.nargs != sgi_gl_nargs[cmd.op as usize] as u16 {
        crate::qlog!("sgi-gl: command {} with {} arguments ignored", cmd.op, cmd.nargs);
        return;
    }
    match cmd.op as u32 {
        SGI_GL_OP_HINT => {
            if c.in_begin != 0
                || a[0] < SGI_GL_PERSPECTIVE_CORRECTION_HINT
                || a[0] > SGI_GL_FOG_HINT
                || !sgi_gl_hint_mode(a[1])
            {
                crate::qlog!(
                    "sgi-gl: hint target {:#x} mode {:#x} ignored{}",
                    a[0],
                    a[1],
                    if c.in_begin != 0 { " inside glBegin" } else { "" }
                );
                return;
            }
            c.hints[(a[0] - SGI_GL_PERSPECTIVE_CORRECTION_HINT) as usize] = a[1];
        }
        SGI_GL_OP_TEXCOORD2F | SGI_GL_OP_TEXCOORD3F | SGI_GL_OP_TEXCOORD4F => {
            c.cur_texcoord[2] = 0.0;
            c.cur_texcoord[3] = 1.0;
            for i in 0..cmd.nargs as usize {
                c.cur_texcoord[i] = sgi_gl_float(a[i]);
            }
        }
        SGI_GL_OP_TEX_GEN => sgi_gl_tex_gen_param(c, a),
        SGI_GL_OP_TEX_ENV => {
            if a[0] == SGI_GL_TEXTURE_ENV_MODE {
                let mode = sgi_gl_float(a[1]);

                if mode == SGI_GL_REPLACE as f32
                    || mode == SGI_GL_MODULATE as f32
                    || mode == SGI_GL_DECAL as f32
                    || mode == SGI_GL_BLEND as f32
                {
                    c.cur.texture_env_mode = mode as u32;
                    return;
                }
            } else if a[0] == SGI_GL_TEXTURE_ENV_COLOR {
                for i in 0..4 {
                    let v = sgi_gl_float(a[i + 1]);

                    c.cur.texture_env_color[i] = if v >= 1.0 {
                        1.0
                    } else if v > 0.0 {
                        v
                    } else {
                        0.0
                    };
                }
                return;
            }
            crate::qlog!(
                "sgi-gl: texture environment parameter {:#x} value {} ignored",
                a[0],
                sgi_gl_float(a[1])
            );
        }
        SGI_GL_OP_VIEWPORT => {
            for i in 0..4 {
                c.cur.viewport[i] = a[i] as i32;
            }
        }
        SGI_GL_OP_CLEAR_COLOR => {
            for i in 0..4 {
                c.clear_color[i] = sgi_gl_float(a[i]);
            }
        }
        SGI_GL_OP_CLEAR_DEPTH => c.clear_depth = a[0],
        SGI_GL_OP_SHADE_MODEL => c.cur.shade_model = a[0],
        SGI_GL_OP_MATRIX_MODE => c.matrix_mode = a[0],
        SGI_GL_OP_LOAD_MATRIX
        | SGI_GL_OP_LOAD_IDENTITY
        | SGI_GL_OP_MULT_MATRIX
        | SGI_GL_OP_FRUSTUM
        | SGI_GL_OP_ORTHO
        | SGI_GL_OP_PUSH_MATRIX
        | SGI_GL_OP_POP_MATRIX
        | SGI_GL_OP_ROTATE
        | SGI_GL_OP_SCALE
        | SGI_GL_OP_TRANSLATE => sgi_gl_matrix_command(c, cmd),
        SGI_GL_OP_ENABLE | SGI_GL_OP_DISABLE => {
            sgi_gl_capability(c, a[0], cmd.op as u32 == SGI_GL_OP_ENABLE);
        }
        SGI_GL_OP_DEPTH_FUNC => {
            if a[0].wrapping_sub(SGI_GL_NEVER_ENUM) > SGI_GL_ALWAYS {
                crate::qlog!("sgi-gl: depth function {:#x} ignored", a[0]);
                return;
            }
            c.cur.depth_func = a[0] - SGI_GL_NEVER_ENUM;
        }
        SGI_GL_OP_DEPTH_MASK => c.cur.depth_mask = (a[0] != 0) as u32,
        SGI_GL_OP_CULL_FACE => {
            if sgi_gl_faces(a[0]) == 0 {
                crate::qlog!("sgi-gl: glCullFace({:#x}) ignored", a[0]);
                return;
            }
            c.cur.cull_face_mode = a[0];
        }
        SGI_GL_OP_FRONT_FACE => {
            if a[0] != SGI_GL_CW && a[0] != SGI_GL_CCW {
                crate::qlog!("sgi-gl: glFrontFace({:#x}) ignored", a[0]);
                return;
            }
            c.cur.front_face = a[0];
        }
        SGI_GL_OP_DRAW_BUFFERS => c.cur.draw_buffers = a[0],
        SGI_GL_OP_SCISSOR => {
            // A negative width or height is an error and changes nothing.
            if (a[2] as i32) < 0 || (a[3] as i32) < 0 {
                crate::qlog!("sgi-gl: glScissor of {} by {} ignored", a[2] as i32, a[3] as i32);
                return;
            }
            for i in 0..4 {
                c.cur.scissor[i] = a[i] as i32;
            }
        }
        SGI_GL_OP_ALPHA_FUNC => {
            let r = sgi_gl_float(a[1]);

            if a[0].wrapping_sub(SGI_GL_NEVER_ENUM) > SGI_GL_ALWAYS {
                crate::qlog!("sgi-gl: alpha function {:#x} ignored", a[0]);
                return;
            }
            c.cur.alpha_func = a[0] - SGI_GL_NEVER_ENUM;
            // Clamped to [0, 1] (4.1.4); not a number clamps to 0.
            c.cur.alpha_ref = if r >= 1.0 {
                1.0
            } else if r > 0.0 {
                r
            } else {
                0.0
            };
        }
        SGI_GL_OP_STENCIL_FUNC => {
            if a[0].wrapping_sub(SGI_GL_NEVER_ENUM) > SGI_GL_ALWAYS {
                crate::qlog!("sgi-gl: stencil function {:#x} ignored", a[0]);
                return;
            }
            c.cur.stencil_func = a[0] - SGI_GL_NEVER_ENUM;
            c.cur.stencil_ref = a[1] as i32;
            c.cur.stencil_value_mask = a[2];
        }
        SGI_GL_OP_STENCIL_OP => {
            for i in 0..3 {
                if !sgi_gl_stencil_op_valid(a[i]) {
                    crate::qlog!("sgi-gl: glStencilOp({:#x}, {:#x}, {:#x}) ignored", a[0], a[1], a[2]);
                    return;
                }
            }
            c.cur.stencil_op.copy_from_slice(&a[..3]);
        }
        SGI_GL_OP_STENCIL_MASK => c.cur.stencil_write_mask = a[0],
        SGI_GL_OP_CLEAR_STENCIL => c.clear_stencil = a[0],
        SGI_GL_OP_BLEND_FUNC => {
            if !sgi_gl_blend_factor_valid(a[0], false) || !sgi_gl_blend_factor_valid(a[1], true) {
                crate::qlog!("sgi-gl: glBlendFunc({:#x}, {:#x}) ignored", a[0], a[1]);
                return;
            }
            c.cur.blend_src = a[0];
            c.cur.blend_dst = a[1];
        }
        SGI_GL_OP_BLEND_EQUATION => {
            if !sgi_gl_blend_equation_valid(a[0]) {
                crate::qlog!("sgi-gl: glBlendEquation({:#x}) ignored", a[0]);
                return;
            }
            c.cur.blend_equation = a[0];
        }
        SGI_GL_OP_BLEND_COLOR => {
            for i in 0..4 {
                let v = sgi_gl_float(a[i]);

                c.cur.blend_color[i] = if v >= 1.0 {
                    1.0
                } else if v > 0.0 {
                    v
                } else {
                    0.0
                };
            }
        }
        SGI_GL_OP_LOGIC_OP => {
            if a[0].wrapping_sub(SGI_GL_CLEAR) > SGI_GL_SET - SGI_GL_CLEAR {
                crate::qlog!("sgi-gl: glLogicOp({:#x}) ignored", a[0]);
                return;
            }
            c.cur.logic_op_mode = a[0];
        }
        SGI_GL_OP_FOG => {
            pos = sgi_gl_words4(a, 1);
            sgi_gl_fog(c, a[0], &pos);
        }
        SGI_GL_OP_POLYGON_OFFSET | SGI_GL_OP_POLYGON_OFFSET_EXT => {
            c.cur.offset_factor = sgi_gl_float(a[0]);
            c.cur.offset_units =
                if cmd.op as u32 == SGI_GL_OP_POLYGON_OFFSET { sgi_gl_float(a[1]) } else { 0.0 };
            c.cur.offset_bias =
                if cmd.op as u32 == SGI_GL_OP_POLYGON_OFFSET_EXT { sgi_gl_float(a[1]) } else { 0.0 };
        }
        SGI_GL_OP_CLIP_PLANE => {
            pos = sgi_gl_words4(a, 1);
            sgi_gl_clip_plane(c, a[0], &pos);
        }
        SGI_GL_OP_POINT_SIZE | SGI_GL_OP_LINE_WIDTH | SGI_GL_OP_LINE_STIPPLE | SGI_GL_OP_POLYGON_MODE => {
            sgi_gl_raster_state(c, cmd.op, a);
        }
        SGI_GL_OP_COLOR_MASK => {
            c.cur.colour_mask = 0;
            for i in 0..4 {
                c.cur.colour_mask |= ((a[i] != 0) as u32) << i;
            }
        }
        SGI_GL_OP_COLOR3F => {
            for i in 0..3 {
                c.cur_color[i] = sgi_gl_float(a[i]);
            }
            c.cur_color[3] = 1.0;
            sgi_gl_track_colour(c);
        }
        SGI_GL_OP_COLOR4F => {
            for i in 0..4 {
                c.cur_color[i] = sgi_gl_float(a[i]);
            }
            sgi_gl_track_colour(c);
        }
        SGI_GL_OP_COLOR3UB | SGI_GL_OP_COLOR4UB => {
            // A byte c is c / 255 (OpenGL 1.2.1, table 2.6); 3ub's alpha is 1.
            for i in 0..4usize {
                c.cur_color[i] =
                    if i == cmd.nargs as usize { 1.0 } else { (a[i] & 0xff) as f32 / 255.0 };
            }
            sgi_gl_track_colour(c);
        }
        SGI_GL_OP_NORMAL3F => {
            for i in 0..3 {
                c.cur_normal[i] = sgi_gl_float(a[i]);
            }
        }
        SGI_GL_OP_EDGE_FLAG => c.cur_edge = (a[0] != 0) as u32,
        SGI_GL_OP_BEGIN => {
            if c.in_begin != 0 {
                crate::qlog!("sgi-gl: nested glBegin ignored");
                return;
            }
            if a[0] > SGI_GL_POLYGON {
                crate::qlog!("sgi-gl: glBegin of mode {} ignored", a[0]);
                return;
            }
            c.line_phase = 0;
            c.in_begin = 1;
            c.begin_mode = a[0];
            c.nheld = 0;
            c.strip_odd = 0;
            c.strip_prev = 0;
            c.strip_swap = 0;
        }
        SGI_GL_OP_SWAP_TMESH => {
            if c.in_begin == 0 || c.begin_mode != SGI_GL_TRIANGLE_STRIP {
                crate::qlog!("sgi-gl: a mesh swap outside an open triangle strip ignored");
                return;
            }
            if c.strip_prev == 0 {
                crate::qlog!("sgi-gl: a mesh swap before the strip's first triangle ignored");
                return;
            }
            // Two swaps before a vertex undo each other.
            c.strip_swap ^= 1;
        }
        SGI_GL_OP_END => {
            if c.in_begin == 0 && c.polygon.enabled != 0 {
                return;
            }
            if c.polygon.enabled != 0 && c.begin_mode == SGI_GL_POLYGON {
                let mut d = SGIGLDraw::default();

                d.context = c.hw_id;
                d.mode = SGI_GL_POLYGON;
                d.count = c.polygon.count;
                d.vertices = P(c.polygon.vertices.as_mut_ptr());
                d.state = P(&mut c.cur as *mut SGIGLDrawState);
                d.back_colors = sgi_gl_lit(c) && c.light_model_two_side != 0;
                d.point_size = c.point_size;
                d.line_width = c.line_width;
                d.point_smooth = c.raster_enables & SGI_GL_RASTER_POINT_SMOOTH != 0;
                d.line_smooth = c.raster_enables & SGI_GL_RASTER_LINE_SMOOTH != 0;
                d.polygon_smooth = c.raster_enables & SGI_GL_RASTER_POLYGON_SMOOTH != 0;
                d.polygon_modes = P(c.polygon_mode.as_mut_ptr());
                d.fill_by_triangle = true;
                d.polygon_stipple = P(c.polygon_stipple.as_mut_ptr());
                d.polygon_stipple_enabled = c.raster_enables & SGI_GL_RASTER_POLYGON_STIPPLE != 0;
                d.line_stipple_enabled = c.raster_enables & SGI_GL_RASTER_LINE_STIPPLE != 0;
                d.line_stipple_repeat = c.line_stipple_repeat;
                d.line_stipple_pattern = c.line_stipple_pattern;

                if c.polygon.failed == 0 && d.count >= 3 {
                    sgi_gl_draw_result(c, sink, &d);
                }
                sgi_gl_polygon_release(&mut c.polygon);
            }
            // A line loop closes from its last vertex to its first, in the
            // first vertex's colour when flat (2.13.7).
            if c.in_begin != 0 && c.begin_mode == SGI_GL_LINE_LOOP && c.nheld == 2 {
                let h1 = c.held[1];
                let h0 = c.held[0];
                sgi_gl_submit(c, sink, SGI_GL_LINES, 2, Some(&h1), Some(&h0), None, None);
            }
            c.line_phase = 0;
            c.in_begin = 0;
            c.begin_mode = 0;
            c.nheld = 0;
            c.strip_odd = 0;
            c.strip_prev = 0;
            c.strip_swap = 0;
        }
        SGI_GL_OP_VERTEX2F | SGI_GL_OP_VERTEX3F | SGI_GL_OP_VERTEX4F => {
            for i in 0..cmd.nargs as usize {
                pos[i] = sgi_gl_float(a[i]);
            }
            sgi_gl_vertex(c, sink, &pos, cmd.nargs as u32);
        }
        SGI_GL_OP_VERTEX2I => {
            // Integer components convert to float unscaled (2.7).
            pos[0] = (a[0] as i32) as f32;
            pos[1] = (a[1] as i32) as f32;
            sgi_gl_vertex(c, sink, &pos, 2);
        }
        SGI_GL_OP_RASTER_POS2F | SGI_GL_OP_RASTER_POS3F | SGI_GL_OP_RASTER_POS4F => {
            pos[2] = 0.0;
            pos[3] = 1.0;
            for i in 0..cmd.nargs as usize {
                pos[i] = sgi_gl_float(a[i]);
            }
            sgi_gl_raster_pos(c, &pos);
        }
        SGI_GL_OP_INDEXF => c.cur_index = sgi_gl_float(a[0]),
        SGI_GL_OP_INDEX_MASK => c.cur.index_mask = a[0],
        SGI_GL_OP_CLEAR_INDEX => c.clear_index = sgi_gl_float(a[0]),
        SGI_GL_OP_DEPTH_RANGE => {
            sgi_gl_depth_range(c, sgi_gl_float(a[0]), sgi_gl_float(a[1]));
        }
        SGI_GL_OP_LIGHT => {
            pos = sgi_gl_words4(a, 2);
            sgi_gl_light(c, a[0], a[1], &pos);
        }
        SGI_GL_OP_MATERIAL => {
            pos = sgi_gl_words4(a, 2);
            if sgi_gl_faces(a[0]) == 0 {
                crate::qlog!("sgi-gl: material face {:#x} ignored", a[0]);
                return;
            }
            sgi_gl_material(c, sgi_gl_faces(a[0]), a[1], &pos);
        }
        SGI_GL_OP_LIGHT_MODEL => {
            pos = sgi_gl_words4(a, 1);
            sgi_gl_light_model(c, a[0], &pos);
        }
        SGI_GL_OP_COLOR_MATERIAL => sgi_gl_color_material(c, a[0], a[1]),
        SGI_GL_OP_FLUSH => {
            // Every piece has already been sent.
        }
        SGI_GL_OP_CLEAR => sgi_gl_clear(c, sink, a[0]),
        SGI_GL_OP_FINISH => {
            // Every piece has already been sent; the fence follows them.
            if let Some(f) = sink.fence {
                f(sink.opaque, c.hw_id);
            }
        }
        _ => {
            // g_assert_not_reached() in the C: every op the table lists is
            // handled above.
        }
    }
}

/// Return the first draw failure from one command.
pub fn sgi_gl_command_checked(
    c: &mut SGIGLContext,
    sink: &SGIGLSink,
    cmd: &SGIGLCmd,
) -> SGIGLDrawResult {
    let mut result: SGIGLDrawResult = SGI_GL_DRAW_OK;
    let mut checked = *sink;

    checked.result = P(&mut result as *mut SGIGLDrawResult);
    sgi_gl_command(c, &checked, cmd);
    result
}

pub fn sgi_gl_get_floats(v: &[f32], count: usize, out: &mut [u32], n: &mut u32) -> bool {
    for i in 0..count {
        out[i] = sgi_gl_bits(v[i]);
    }
    *n = count as u32;
    true
}

/// glGetLightfv of light number light (0 for GL_LIGHT0) and glGetMaterialfv
/// of GL_FRONT or GL_BACK: the answer as *n float words into out (which
/// holds SGI_GL_GET_WORDS). False, with nothing written, for a light, face
/// or parameter the context does not keep.
pub fn sgi_gl_get_light(
    c: &SGIGLContext,
    light: u32,
    pname: u32,
    out: &mut [u32],
    n: &mut u32,
) -> bool {
    if light >= SGI_GL_LIGHTS {
        return false;
    }
    let l = &c.light[light as usize];
    match pname {
        SGI_GL_AMBIENT => sgi_gl_get_floats(&l.ambient, 4, out, n),
        SGI_GL_DIFFUSE => sgi_gl_get_floats(&l.diffuse, 4, out, n),
        SGI_GL_SPECULAR => sgi_gl_get_floats(&l.specular, 4, out, n),
        SGI_GL_POSITION => sgi_gl_get_floats(&l.position, 4, out, n),
        SGI_GL_SPOT_DIRECTION => sgi_gl_get_floats(&l.spot_direction, 3, out, n),
        SGI_GL_SPOT_EXPONENT => sgi_gl_get_floats(std::slice::from_ref(&l.spot_exponent), 1, out, n),
        SGI_GL_SPOT_CUTOFF => sgi_gl_get_floats(std::slice::from_ref(&l.spot_cutoff), 1, out, n),
        SGI_GL_CONSTANT_ATTENUATION | SGI_GL_LINEAR_ATTENUATION | SGI_GL_QUADRATIC_ATTENUATION => {
            sgi_gl_get_floats(
                std::slice::from_ref(&l.attenuation[(pname - SGI_GL_CONSTANT_ATTENUATION) as usize]),
                1,
                out,
                n,
            )
        }
        _ => false,
    }
}

/// glGetClipPlane of plane number plane (0 for GL_CLIP_PLANE0): its four
/// coefficients in eye coordinates as float words. False for a plane the
/// context does not keep.
pub fn sgi_gl_get_clip_plane(c: &SGIGLContext, plane: u32, out: &mut [u32; 4]) -> bool {
    let mut n = 0u32;

    plane < SGI_GL_CLIP_PLANES
        && sgi_gl_get_floats(&c.cur.clip_plane[plane as usize], 4, out, &mut n)
}

pub fn sgi_gl_get_material(
    c: &SGIGLContext,
    face: u32,
    pname: u32,
    out: &mut [u32],
    n: &mut u32,
) -> bool {
    if face != SGI_GL_FRONT && face != SGI_GL_BACK {
        return false;
    }
    let m = &c.material[(face == SGI_GL_BACK) as usize];
    match pname {
        SGI_GL_AMBIENT => sgi_gl_get_floats(&m.ambient, 4, out, n),
        SGI_GL_DIFFUSE => sgi_gl_get_floats(&m.diffuse, 4, out, n),
        SGI_GL_SPECULAR => sgi_gl_get_floats(&m.specular, 4, out, n),
        SGI_GL_EMISSION => sgi_gl_get_floats(&m.emission, 4, out, n),
        SGI_GL_SHININESS => sgi_gl_get_floats(std::slice::from_ref(&m.shininess), 1, out, n),
        SGI_GL_COLOR_INDEXES => sgi_gl_get_floats(&m.color_indexes, 3, out, n),
        _ => false,
    }
}

/// One word answer.
pub fn sgi_gl_get_one(out: &mut [u32], n: &mut u32, v: u32) -> bool {
    out[0] = v;
    *n = 1;
    true
}

/// The glGet answer for pname as *n 32-bit words into out, which holds
/// SGI_GL_GET_WORDS; floats as their bits, enumerants and integers as
/// themselves, a matrix column-major. False for a pname the context does
/// not keep.
///
/// Texture coordinates are not kept, so GL_CURRENT_TEXTURE_COORDS and
/// GL_CURRENT_RASTER_TEXTURE_COORDS answer OpenGL's initial (0, 0, 0, 1)
/// (2.7, 2.12), wrong once a client has sent glTexCoord.
pub fn sgi_gl_get(c: &SGIGLContext, pname: u32, out: &mut [u32], n: &mut u32) -> bool {
    if c.in_begin != 0
        && (pname == SGI_GL_LINE_STIPPLE
            || pname == SGI_GL_LINE_STIPPLE_REPEAT
            || pname == SGI_GL_LINE_STIPPLE_PATTERN)
    {
        crate::qlog!("sgi-gl: line stipple query inside glBegin: invalid operation");
        return false;
    }
    match pname {
        SGI_GL_PERSPECTIVE_CORRECTION_HINT..=SGI_GL_FOG_HINT => {
            sgi_gl_get_one(out, n, c.hints[(pname - SGI_GL_PERSPECTIVE_CORRECTION_HINT) as usize])
        }
        SGI_GL_CURRENT_COLOR => {
            for i in 0..4 {
                out[i] = sgi_gl_bits(c.cur_color[i]);
            }
            *n = 4;
            true
        }
        SGI_GL_CURRENT_NORMAL => {
            for i in 0..3 {
                out[i] = sgi_gl_bits(c.cur_normal[i]);
            }
            *n = 3;
            true
        }
        SGI_GL_CURRENT_TEXTURE_COORDS => sgi_gl_get_floats(&c.cur_texcoord, 4, out, n),
        SGI_GL_CURRENT_RASTER_TEXTURE_COORDS => sgi_gl_get_floats(&c.raster_texcoord, 4, out, n),
        SGI_GL_TEXTURE_1D => sgi_gl_get_one(out, n, c.cur.texture_1d),
        SGI_GL_TEXTURE_2D => sgi_gl_get_one(out, n, c.cur.texture_2d),
        SGI_GL_TEXTURE_3D => sgi_gl_get_one(out, n, c.cur.texture_3d),
        SGI_GL_TEXTURE_ENV_MODE => sgi_gl_get_one(out, n, c.cur.texture_env_mode),
        SGI_GL_TEXTURE_ENV_COLOR => sgi_gl_get_floats(&c.cur.texture_env_color, 4, out, n),
        SGI_GL_EDGE_FLAG => sgi_gl_get_one(out, n, c.cur_edge),
        SGI_GL_SHADE_MODEL => sgi_gl_get_one(out, n, c.cur.shade_model),
        SGI_GL_MATRIX_MODE => sgi_gl_get_one(out, n, c.matrix_mode),
        SGI_GL_VIEWPORT => {
            for i in 0..4 {
                out[i] = c.cur.viewport[i] as u32;
            }
            *n = 4;
            true
        }
        // Column-major, the order glLoadMatrixf takes (6.1.2).
        SGI_GL_MODELVIEW_MATRIX | SGI_GL_PROJECTION_MATRIX => {
            for i in 0..16 {
                out[i] = sgi_gl_bits(if pname == SGI_GL_MODELVIEW_MATRIX {
                    c.cur.modelview[i]
                } else {
                    c.cur.projection[i]
                });
            }
            *n = 16;
            true
        }
        SGI_GL_CURRENT_RASTER_POSITION | SGI_GL_CURRENT_RASTER_COLOR => {
            for i in 0..4 {
                out[i] = sgi_gl_bits(if pname == SGI_GL_CURRENT_RASTER_POSITION {
                    c.raster_pos[i]
                } else {
                    c.raster_color[i]
                });
            }
            *n = 4;
            true
        }
        SGI_GL_CURRENT_RASTER_POSITION_VALID => sgi_gl_get_one(out, n, c.raster_valid),
        SGI_GL_CURRENT_RASTER_DISTANCE => sgi_gl_get_one(out, n, sgi_gl_bits(c.raster_distance)),
        // The indices as floats, the form the context keeps them in.
        SGI_GL_CURRENT_INDEX | SGI_GL_CURRENT_RASTER_INDEX | SGI_GL_INDEX_CLEAR_VALUE => {
            sgi_gl_get_one(
                out,
                n,
                sgi_gl_bits(if pname == SGI_GL_CURRENT_INDEX {
                    c.cur_index
                } else if pname == SGI_GL_CURRENT_RASTER_INDEX {
                    c.raster_index
                } else {
                    c.clear_index
                }),
            )
        }
        SGI_GL_INDEX_WRITEMASK => sgi_gl_get_one(out, n, c.cur.index_mask),
        SGI_GL_COLOR_CLEAR_VALUE => {
            for i in 0..4 {
                out[i] = sgi_gl_bits(c.clear_color[i]);
            }
            *n = 4;
            true
        }
        // glGetBooleanv of a capability is glIsEnabled (6.1.1).
        SGI_GL_DEPTH_TEST => sgi_gl_get_one(out, n, c.cur.depth_test),
        SGI_GL_DITHER => sgi_gl_get_one(out, n, c.cur.dither),
        SGI_GL_CULL_FACE => sgi_gl_get_one(out, n, c.cur.cull_face),
        SGI_GL_CULL_FACE_MODE => sgi_gl_get_one(out, n, c.cur.cull_face_mode),
        SGI_GL_FRONT_FACE => sgi_gl_get_one(out, n, c.cur.front_face),
        SGI_GL_SCISSOR_TEST => sgi_gl_get_one(out, n, c.cur.scissor_test),
        SGI_GL_SCISSOR_BOX => {
            for i in 0..4 {
                out[i] = c.cur.scissor[i] as u32;
            }
            *n = 4;
            true
        }
        SGI_GL_ALPHA_TEST => sgi_gl_get_one(out, n, c.cur.alpha_test),
        SGI_GL_ALPHA_TEST_FUNC => sgi_gl_get_one(out, n, SGI_GL_NEVER_ENUM + c.cur.alpha_func),
        SGI_GL_ALPHA_TEST_REF => sgi_gl_get_floats(std::slice::from_ref(&c.cur.alpha_ref), 1, out, n),
        SGI_GL_STENCIL_TEST => sgi_gl_get_one(out, n, c.cur.stencil_test),
        SGI_GL_STENCIL_FUNC => sgi_gl_get_one(out, n, SGI_GL_NEVER_ENUM + c.cur.stencil_func),
        SGI_GL_STENCIL_REF => sgi_gl_get_one(out, n, c.cur.stencil_ref as u32),
        SGI_GL_STENCIL_VALUE_MASK => sgi_gl_get_one(out, n, c.cur.stencil_value_mask),
        SGI_GL_STENCIL_WRITEMASK => sgi_gl_get_one(out, n, c.cur.stencil_write_mask),
        SGI_GL_STENCIL_FAIL | SGI_GL_STENCIL_PASS_DEPTH_FAIL | SGI_GL_STENCIL_PASS_DEPTH_PASS => {
            sgi_gl_get_one(out, n, c.cur.stencil_op[(pname - SGI_GL_STENCIL_FAIL) as usize])
        }
        SGI_GL_STENCIL_CLEAR_VALUE => sgi_gl_get_one(out, n, c.clear_stencil),
        SGI_GL_BLEND => sgi_gl_get_one(out, n, c.cur.blend),
        SGI_GL_BLEND_SRC | SGI_GL_BLEND_DST => sgi_gl_get_one(
            out,
            n,
            if pname == SGI_GL_BLEND_SRC { c.cur.blend_src } else { c.cur.blend_dst },
        ),
        SGI_GL_BLEND_EQUATION => sgi_gl_get_one(out, n, c.cur.blend_equation),
        SGI_GL_BLEND_COLOR => sgi_gl_get_floats(&c.cur.blend_color, 4, out, n),
        SGI_GL_INDEX_LOGIC_OP | SGI_GL_COLOR_LOGIC_OP => sgi_gl_get_one(
            out,
            n,
            if pname == SGI_GL_INDEX_LOGIC_OP { c.cur.index_logic_op } else { c.cur.color_logic_op },
        ),
        SGI_GL_LOGIC_OP_MODE => sgi_gl_get_one(out, n, c.cur.logic_op_mode),
        SGI_GL_COLOR_WRITEMASK => {
            for i in 0..4 {
                out[i] = extract32(c.cur.colour_mask, i as u32, 1);
            }
            *n = 4;
            true
        }
        SGI_GL_FOG => sgi_gl_get_one(out, n, c.cur.fog),
        SGI_GL_FOG_MODE => sgi_gl_get_one(out, n, c.cur.fog_mode),
        SGI_GL_FOG_DENSITY => sgi_gl_get_floats(std::slice::from_ref(&c.cur.fog_density), 1, out, n),
        SGI_GL_FOG_START => sgi_gl_get_floats(std::slice::from_ref(&c.cur.fog_start), 1, out, n),
        SGI_GL_FOG_END => sgi_gl_get_floats(std::slice::from_ref(&c.cur.fog_end), 1, out, n),
        SGI_GL_FOG_COLOR => sgi_gl_get_floats(&c.cur.fog_color, 4, out, n),
        SGI_GL_FOG_INDEX => sgi_gl_get_floats(std::slice::from_ref(&c.cur.fog_index), 1, out, n),
        SGI_GL_POLYGON_OFFSET_FILL | SGI_GL_POLYGON_OFFSET_LINE | SGI_GL_POLYGON_OFFSET_POINT => {
            sgi_gl_get_one(
                out,
                n,
                extract32(
                    c.cur.polygon_offset,
                    if pname == SGI_GL_POLYGON_OFFSET_FILL {
                        0
                    } else if pname == SGI_GL_POLYGON_OFFSET_LINE {
                        1
                    } else {
                        2
                    },
                    1,
                ),
            )
        }
        SGI_GL_POLYGON_OFFSET_FACTOR => {
            sgi_gl_get_floats(std::slice::from_ref(&c.cur.offset_factor), 1, out, n)
        }
        SGI_GL_CLIP_PLANE0..=SGI_GL_CLIP_PLANE_LAST => {
            sgi_gl_get_one(out, n, extract32(c.cur.clip_planes, pname - SGI_GL_CLIP_PLANE0, 1))
        }
        SGI_GL_POINT_SMOOTH
        | SGI_GL_LINE_SMOOTH
        | SGI_GL_LINE_STIPPLE
        | SGI_GL_POLYGON_SMOOTH
        | SGI_GL_POLYGON_STIPPLE => {
            if pname == SGI_GL_POLYGON_STIPPLE && c.in_begin != 0 {
                crate::qlog!("sgi-gl: polygon stipple query inside glBegin: invalid operation");
                return false;
            }
            sgi_gl_get_one(out, n, (c.raster_enables & sgi_gl_raster_enable(pname) != 0) as u32)
        }
        SGI_GL_POINT_SIZE => sgi_gl_get_floats(std::slice::from_ref(&c.point_size), 1, out, n),
        SGI_GL_LINE_WIDTH => sgi_gl_get_floats(std::slice::from_ref(&c.line_width), 1, out, n),
        SGI_GL_LINE_STIPPLE_REPEAT => sgi_gl_get_one(out, n, c.line_stipple_repeat),
        SGI_GL_LINE_STIPPLE_PATTERN => sgi_gl_get_one(out, n, c.line_stipple_pattern),
        SGI_GL_POLYGON_MODE => {
            out[0] = c.polygon_mode[0];
            out[1] = c.polygon_mode[1];
            *n = 2;
            true
        }
        SGI_GL_POLYGON_OFFSET_UNITS => {
            sgi_gl_get_floats(std::slice::from_ref(&c.cur.offset_units), 1, out, n)
        }
        SGI_GL_POLYGON_OFFSET_BIAS => {
            sgi_gl_get_floats(std::slice::from_ref(&c.cur.offset_bias), 1, out, n)
        }
        SGI_GL_DEPTH_FUNC => sgi_gl_get_one(out, n, SGI_GL_NEVER_ENUM + c.cur.depth_func),
        SGI_GL_DEPTH_WRITEMASK => sgi_gl_get_one(out, n, c.cur.depth_mask),
        // The matrices a stack holds, the current one included (6.1.2).
        SGI_GL_MODELVIEW_STACK_DEPTH => sgi_gl_get_one(out, n, c.mv_depth + 1),
        SGI_GL_PROJECTION_STACK_DEPTH => sgi_gl_get_one(out, n, c.pr_depth + 1),
        SGI_GL_TEXTURE_STACK_DEPTH => sgi_gl_get_one(out, n, c.tx_depth + 1),
        SGI_GL_TEXTURE_MATRIX => sgi_gl_get_floats(&c.texture, 16, out, n),
        SGI_GL_DEPTH_RANGE => {
            for i in 0..2 {
                out[i] = sgi_gl_bits(c.cur.depth_range[i]);
            }
            *n = 2;
            true
        }
        SGI_GL_LIGHT_MODEL_AMBIENT => sgi_gl_get_floats(&c.light_model_ambient, 4, out, n),
        SGI_GL_LIGHT_MODEL_LOCAL_VIEWER => sgi_gl_get_one(out, n, c.light_model_local_viewer),
        SGI_GL_LIGHT_MODEL_TWO_SIDE => sgi_gl_get_one(out, n, c.light_model_two_side),
        SGI_GL_COLOR_MATERIAL => sgi_gl_get_one(out, n, c.color_material),
        SGI_GL_LIGHTING => sgi_gl_get_one(out, n, c.lighting),
        SGI_GL_NORMALIZE => sgi_gl_get_one(out, n, c.normalize),
        SGI_GL_RESCALE_NORMAL => sgi_gl_get_one(out, n, c.rescale_normal),
        SGI_GL_LIGHT0..=SGI_GL_LIGHT_LAST => {
            sgi_gl_get_one(out, n, extract32(c.lights_enabled, pname - SGI_GL_LIGHT0, 1))
        }
        SGI_GL_TEXTURE_GEN_S..=SGI_GL_TEXTURE_GEN_Q => {
            sgi_gl_get_one(out, n, extract32(c.tex_gen_enabled, pname - SGI_GL_TEXTURE_GEN_S, 1))
        }
        SGI_GL_COLOR_MATERIAL_FACE => sgi_gl_get_one(out, n, c.color_material_face),
        SGI_GL_COLOR_MATERIAL_PARAMETER => sgi_gl_get_one(out, n, c.color_material_mode),
        _ => false,
    }
}

/// The most vertices each primitive mode leaves held between pieces, as
/// sgi_gl_vertex() keeps them: a line strip keeps its last vertex, a line
/// loop its first and last, a quad strip three until its first quad.
pub fn sgi_gl_held_max(mode: u32) -> u32 {
    match mode {
        SGI_GL_POINTS => 0,
        SGI_GL_LINES | SGI_GL_LINE_STRIP => 1,
        SGI_GL_QUADS | SGI_GL_QUAD_STRIP => 3,
        _ => 2,
    }
}

/// How many vertices a primitive of this mode holds after count vertices
/// since glBegin, or -1 when k cannot be that number. Only count mod 2,
/// 3 and 4 and k itself decide it, never count's absolute size, so a
/// count that is right only modulo 12 is enough: once a mode has all it
/// will ever hold, k alone says so.
pub fn sgi_gl_resume_held_ok(mode: u32, k: u32, count: u32) -> bool {
    match mode {
        SGI_GL_POINTS => k == 0,
        SGI_GL_LINES => k == count % 2,
        SGI_GL_TRIANGLES => k == count % 3,
        SGI_GL_QUADS => k == count % 4,
        SGI_GL_QUAD_STRIP => {
            if k >= 2 {
                k == 2 + count % 2
            } else {
                k == count
            }
        }
        SGI_GL_LINE_STRIP => k <= 1 && (k == 1 || count == 0),
        // Strips of triangles, fans, loops, polygons: first and last.
        _ => k <= 2 && (k == 2 || k == count),
    }
}

/// Resume a primitive a context switch interrupted inside glBegin/glEnd:
/// begin mode and take k of the vertices the primitive held, oldest first,
/// without drawing. count is how many vertices had been sent since glBegin.
/// Refused, with a log, inside glBegin, for a (k, count, mode) that cannot
/// occur, and for glPolygon while polygon mode assembly is on. `v` holds k
/// vertices; None when k is 0.
pub fn sgi_gl_resume_primitive(
    c: &mut SGIGLContext,
    mode: u32,
    v: Option<&[[f32; SGI_GL_VERTEX_FLOATS]]>,
    k: u32,
    count: u32,
) -> bool {
    if c.in_begin != 0 {
        crate::qlog!("sgi-gl: resume inside glBegin ignored");
        return false;
    }
    if mode > SGI_GL_POLYGON
        || k > SGI_GL_HELD_VERTICES
        || (k != 0 && v.map_or(true, |s| s.len() < k as usize))
        || !sgi_gl_resume_held_ok(mode, k, count)
        || (c.polygon.enabled != 0 && mode == SGI_GL_POLYGON)
    {
        crate::qlog!("sgi-gl: resume of mode {} with {} held of {} sent refused", mode, k, count);
        return false;
    }
    // line_phase is deliberately left alone, unlike glBegin.
    c.in_begin = 1;
    c.begin_mode = mode;
    c.nheld = k;
    for i in 0..k {
        sgi_gl_hold(c, i, &v.unwrap()[i as usize]);
    }
    // The strip's next triangle faces as its count's parity says.
    c.strip_odd = if mode == SGI_GL_TRIANGLE_STRIP && k == 2 { count & 1 } else { 0 };
    // The oldest retained vertex of a resumed strip is not sent.
    c.strip_prev = 0;
    c.strip_swap = 0;
    true
}

/// Close an interrupted primitive without drawing a line-loop edge.
pub fn sgi_gl_discard_primitive(c: &mut SGIGLContext) {
    if c.polygon.enabled != 0 && c.begin_mode == SGI_GL_POLYGON {
        sgi_gl_polygon_release(&mut c.polygon);
    }
    c.in_begin = 0;
    c.begin_mode = 0;
    c.nheld = 0;
    c.strip_odd = 0;
    c.strip_prev = 0;
    c.strip_swap = 0;
}

/// Whether c holds state the context's own commands could have left. Err
/// carries the reason (the C's errp).
pub fn sgi_gl_context_valid(c: &SGIGLContext) -> Result<(), String> {
    let st = &c.cur;
    let flags: [(&str, u32); 33] = [
        ("depth_test", st.depth_test),
        ("depth_mask", st.depth_mask),
        ("cull_face", st.cull_face),
        ("scissor_test", st.scissor_test),
        ("alpha_test", st.alpha_test),
        ("stencil_test", st.stencil_test),
        ("blend", st.blend),
        ("index_logic_op", st.index_logic_op),
        ("color_logic_op", st.color_logic_op),
        ("fog", st.fog),
        ("texture_1d", st.texture_1d),
        ("texture_2d", st.texture_2d),
        ("texture_3d", st.texture_3d),
        ("drawable.bound", st.drawable.bound),
        ("drawable.clip.empty", st.drawable.clip.empty),
        ("window_matrix_given", st.window_matrix_given),
        ("window_rows_down", st.window_rows_down),
        ("dither", st.dither),
        ("light_model_local_viewer", c.light_model_local_viewer),
        ("light_model_two_side", c.light_model_two_side),
        ("color_material", c.color_material),
        ("lighting", c.lighting),
        ("normalize", c.normalize),
        ("rescale_normal", c.rescale_normal),
        ("normal_matrix_given", c.normal_matrix_given),
        ("normalize_always", c.normalize_always),
        ("cur_edge", c.cur_edge),
        ("raster_valid", c.raster_valid),
        ("strip_odd", c.strip_odd),
        ("strip_prev", c.strip_prev),
        ("strip_swap", c.strip_swap),
        ("in_begin", c.in_begin),
        ("sprite", c.sprite),
    ];
    let stacks: [(&str, u32); 3] =
        [("mv_depth", c.mv_depth), ("pr_depth", c.pr_depth), ("tx_depth", c.tx_depth)];

    for i in 0..SGI_GL_HINTS as usize {
        if !sgi_gl_hint_mode(c.hints[i]) {
            return Err(format!("sgi-gl: hint {} mode {:#x}", i, c.hints[i]));
        }
    }
    if c.sprite_mode != SGI_GL_SPRITE_AXIAL_SGIX
        && c.sprite_mode != SGI_GL_SPRITE_OBJECT_ALIGNED_SGIX
        && c.sprite_mode != SGI_GL_SPRITE_EYE_ALIGNED_SGIX
    {
        return Err(format!("sgi-gl: sprite mode {:#x}", c.sprite_mode));
    }

    for f in flags.iter() {
        if f.1 > 1 {
            return Err(format!("sgi-gl: {} is {}, not 0 or 1", f.0, f.1));
        }
    }
    if (st.drawable.width != 0) != (st.drawable.height != 0) {
        return Err(format!(
            "sgi-gl: drawable extent {} by {} is half-known",
            st.drawable.width, st.drawable.height
        ));
    }
    if st.drawable.bound == 0 && st.drawable.width != 0 {
        return Err(format!(
            "sgi-gl: unbound drawable extent {} by {}",
            st.drawable.width, st.drawable.height
        ));
    }
    for s in stacks.iter() {
        if s.1 >= SGI_GL_STACK_DEPTH {
            return Err(format!("sgi-gl: {} {} is past the stack's {}", s.0, s.1, SGI_GL_STACK_DEPTH));
        }
    }
    if st.depth_func > SGI_GL_ALWAYS {
        return Err(format!("sgi-gl: depth function {}", st.depth_func));
    }
    for i in 0..2 {
        if !(st.depth_range[i] >= 0.0 && st.depth_range[i] <= 1.0) {
            return Err(format!("sgi-gl: depth range {} is outside [0, 1]", st.depth_range[i]));
        }
    }
    if st.index_bits > 32 {
        return Err(format!("sgi-gl: {} bits a colour index", st.index_bits));
    }
    if sgi_gl_faces(st.cull_face_mode) == 0 {
        return Err(format!("sgi-gl: cull face {:#x}", st.cull_face_mode));
    }
    if st.front_face != SGI_GL_CW && st.front_face != SGI_GL_CCW {
        return Err(format!("sgi-gl: front face {:#x}", st.front_face));
    }
    if st.scissor[2] < 0 || st.scissor[3] < 0 {
        return Err(format!("sgi-gl: scissor box of {} by {}", st.scissor[2], st.scissor[3]));
    }
    if st.alpha_func > SGI_GL_ALWAYS {
        return Err(format!("sgi-gl: alpha function {}", st.alpha_func));
    }
    if !(st.alpha_ref >= 0.0 && st.alpha_ref <= 1.0) {
        return Err(format!("sgi-gl: alpha reference {} is outside [0, 1]", st.alpha_ref));
    }
    if st.stencil_func > SGI_GL_ALWAYS {
        return Err(format!("sgi-gl: stencil function {}", st.stencil_func));
    }
    for i in 0..3 {
        if !sgi_gl_stencil_op_valid(st.stencil_op[i]) {
            return Err(format!("sgi-gl: stencil operation {:#x}", st.stencil_op[i]));
        }
    }
    if !sgi_gl_blend_factor_valid(st.blend_src, false) || !sgi_gl_blend_factor_valid(st.blend_dst, true) {
        return Err(format!("sgi-gl: blend factors {:#x} and {:#x}", st.blend_src, st.blend_dst));
    }
    if !sgi_gl_blend_equation_valid(st.blend_equation) {
        return Err(format!("sgi-gl: blend equation {:#x}", st.blend_equation));
    }
    for i in 0..4 {
        if !(st.blend_color[i] >= 0.0 && st.blend_color[i] <= 1.0) {
            return Err(format!("sgi-gl: blend colour {} is outside [0, 1]", st.blend_color[i]));
        }
    }
    if st.logic_op_mode.wrapping_sub(SGI_GL_CLEAR) > SGI_GL_SET - SGI_GL_CLEAR {
        return Err(format!("sgi-gl: logical operation {:#x}", st.logic_op_mode));
    }
    if st.colour_mask > SGI_GL_COLOUR_MASK_ALL {
        return Err(format!("sgi-gl: colour mask {:#x}", st.colour_mask));
    }
    if st.texture_env_mode != SGI_GL_REPLACE
        && st.texture_env_mode != SGI_GL_MODULATE
        && st.texture_env_mode != SGI_GL_DECAL
        && st.texture_env_mode != SGI_GL_BLEND
    {
        return Err(format!("sgi-gl: texture environment mode {:#x}", st.texture_env_mode));
    }
    for i in 0..4 {
        if !(st.texture_env_color[i] >= 0.0 && st.texture_env_color[i] <= 1.0) {
            return Err(format!(
                "sgi-gl: texture environment colour {} is outside [0, 1]",
                st.texture_env_color[i]
            ));
        }
    }
    if st.fog_mode != SGI_GL_LINEAR && st.fog_mode != SGI_GL_EXP && st.fog_mode != SGI_GL_EXP2 {
        return Err(format!("sgi-gl: fog mode {:#x}", st.fog_mode));
    }
    if !(st.fog_density >= 0.0) {
        return Err(format!("sgi-gl: fog density {}", st.fog_density));
    }
    for i in 0..4 {
        if !(st.fog_color[i] >= 0.0 && st.fog_color[i] <= 1.0) {
            return Err(format!("sgi-gl: fog colour {} is outside [0, 1]", st.fog_color[i]));
        }
    }
    if st.polygon_offset & !(SGI_GL_OFFSET_FILL | SGI_GL_OFFSET_LINE | SGI_GL_OFFSET_POINT) != 0 {
        return Err(format!("sgi-gl: polygon offset enables {:#x}", st.polygon_offset));
    }
    if (st.clip_planes >> SGI_GL_CLIP_PLANES) != 0 {
        return Err(format!("sgi-gl: clip plane enables {:#x}", st.clip_planes));
    }
    if st.drawable.clip.n > SGI_CLIP_MAX as u32 {
        return Err(format!("sgi-gl: {} clip rectangles", st.drawable.clip.n));
    }
    if (c.lights_enabled >> SGI_GL_LIGHTS) != 0 {
        return Err(format!("sgi-gl: light enables {:#x}", c.lights_enabled));
    }
    if (c.tex_gen_enabled >> 4) != 0 {
        return Err(format!("sgi-gl: texture generation enables {:#x}", c.tex_gen_enabled));
    }
    for i in 0..4usize {
        let mode = c.tex_gen_mode[i];

        if mode != SGI_GL_EYE_LINEAR
            && mode != SGI_GL_OBJECT_LINEAR
            && (mode != SGI_GL_SPHERE_MAP || i >= 2)
        {
            return Err(format!("sgi-gl: texture generation mode {} {:#x}", i, mode));
        }
    }
    if sgi_gl_faces(c.color_material_face) == 0 {
        return Err(format!("sgi-gl: colour material face {:#x}", c.color_material_face));
    }
    match c.color_material_mode {
        SGI_GL_AMBIENT | SGI_GL_DIFFUSE | SGI_GL_SPECULAR | SGI_GL_EMISSION
        | SGI_GL_AMBIENT_AND_DIFFUSE => {}
        _ => {
            return Err(format!("sgi-gl: colour material parameter {:#x}", c.color_material_mode));
        }
    }
    if !c.point_size.is_finite()
        || !c.line_width.is_finite()
        || !(c.point_size > 0.0)
        || !(c.line_width > 0.0)
    {
        return Err(format!(
            "sgi-gl: point size {} or line width {}",
            c.point_size, c.line_width
        ));
    }
    if c.line_stipple_repeat.wrapping_sub(1) > 255 || c.line_stipple_pattern > 0xffff {
        return Err(format!(
            "sgi-gl: line stipple repeat {}, pattern {:#x}",
            c.line_stipple_repeat, c.line_stipple_pattern
        ));
    }
    for i in 0..2 {
        if c.polygon_mode[i].wrapping_sub(SGI_GL_POINT) > SGI_GL_FILL - SGI_GL_POINT {
            return Err(format!("sgi-gl: polygon mode {:#x}", c.polygon_mode[i]));
        }
    }
    if c.raster_enables
        & !(SGI_GL_RASTER_POINT_SMOOTH
            | SGI_GL_RASTER_LINE_SMOOTH
            | SGI_GL_RASTER_LINE_STIPPLE
            | SGI_GL_RASTER_POLYGON_SMOOTH
            | SGI_GL_RASTER_POLYGON_STIPPLE)
        != 0
    {
        return Err(format!("sgi-gl: rasterization enables {:#x}", c.raster_enables));
    }
    if c.polygon.enabled > 1
        || c.polygon.failed > 1
        || c.polygon.count > c.polygon.capacity
        || (c.polygon.count != 0 && c.polygon.vertices.is_empty())
        || (c.polygon.failed != 0 && c.polygon.count != 0)
        || (c.polygon.error != 0 && c.polygon.error != SGI_GL_OUT_OF_MEMORY)
        || ((c.polygon.count != 0 || c.polygon.failed != 0)
            && (c.polygon.enabled == 0 || c.in_begin == 0 || c.begin_mode != SGI_GL_POLYGON))
        || (c.polygon.enabled != 0
            && c.in_begin != 0
            && c.begin_mode == SGI_GL_POLYGON
            && c.nheld != 0)
    {
        return Err("sgi-gl: invalid owned polygon assembly".to_string());
    }
    if c.begin_mode > SGI_GL_POLYGON {
        return Err(format!("sgi-gl: primitive mode {}", c.begin_mode));
    }
    if c.nheld > (if c.in_begin != 0 { sgi_gl_held_max(c.begin_mode) } else { 0 }) {
        return Err(format!(
            "sgi-gl: {} vertices held in mode {}{}",
            c.nheld,
            c.begin_mode,
            if c.in_begin != 0 { "" } else { " outside glBegin" }
        ));
    }
    Ok(())
}
