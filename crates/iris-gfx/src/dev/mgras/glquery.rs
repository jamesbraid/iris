//! GL state queries: what the GE answers when libGLcore reads its state back
//! (`__MGR_RETURN_MODE` 0x0A2 and `SPIN_AND_RETURN` 0x0A1 at a GE address;
//! `GET_LIGHTFV` 0x0EF and `GET_MATERIALFV` 0x0EE, which copy a parameter
//! into the scratch words at 0x32E..0x331 first).
//!
//! The map of GE addresses and their formats comes from
//! `test/gltest/glstate.c` (every queryable GL state, set and read back)
//! recorded and split by `tools/mgras-statemap.py`, and from `mgras probe`
//! runs (`tools/mgras-probe-states.py`): the GE answers chosen words and
//! glstate shows how libGLcore decodes them (IRIX 6.5.22, 2026-10-13).
//! ignore/gr4/GE11.h "STATE READBACK" has the table.
//!
//! The render state keeps derived values (lights in eye space and folded,
//! blend factors as PP1 codes); `Query` keeps what the program set, in the
//! form a query returns it.

use super::Gl;
use crate::dev::gl::math;
use crate::dev::gl::light::MAX_LIGHTS;

/// One light's parameters as glGetLight returns them (position and spot
/// direction in eye coordinates).
#[derive(Clone, Copy, Default)]
#[repr(C)]
pub(super) struct LightQ {
    pub ambient: [f32; 4],
    pub diffuse: [f32; 4],
    pub specular: [f32; 4],
    pub position: [f32; 4],
    pub spot_dir: [f32; 3],
    pub spot_exp: f32,
    pub spot_cutoff: f32,
    /// Constant, linear, quadratic.
    pub atten: [f32; 3],
}

/// One face's material as glGetMaterial returns it.
#[derive(Clone, Copy, Default)]
#[repr(C)]
pub(super) struct MatQ {
    pub ambient: [f32; 4],
    pub diffuse: [f32; 4],
    pub specular: [f32; 4],
    pub emission: [f32; 4],
    pub shininess: f32,
    pub indexes: [f32; 3],
}

/// The query shadow (plain numbers: it lives in the per-context record).
#[derive(Clone, Copy, Default)]
#[repr(C)]
pub(super) struct Query {
    pub lights: [LightQ; MAX_LIGHTS],
    pub mats: [MatQ; 2],
    pub lmodel_ambient: [f32; 4],
    /// glColorMaterial face and parameter, GL enums.
    pub cmat: [u32; 2],
    /// glFog: mode (GL enum), colour, index.
    pub fog_mode: u32,
    pub fog_color: [f32; 4],
    /// The fog index as FOG_INDEX sends it, an integer.
    pub fog_index: u32,
    /// glPolygonOffset factor, units.
    pub poly_offset: [f32; 2],
    pub line_smooth: u32,
    /// glBlendFunc as GL enums.
    pub blend_func: [u32; 2],
    /// The raster position's colour as latched by glRasterPos (drawing
    /// lets later colours through, see `Gl::raster_color`), texture
    /// coordinates and eye distance.
    pub raster_color: [f32; 4],
    pub raster_tex: [f32; 4],
    pub raster_dist: f32,
    /// GET_LIGHTFV / GET_MATERIALFV results, read at 0x32E..0x331.
    pub scratch: [u32; 4],
}

// GL enums.
const GL_FRONT: u32 = 0x404;
const GL_BACK: u32 = 0x405;
const GL_FRONT_AND_BACK: u32 = 0x408;
const GL_AMBIENT: u32 = 0x1200;
const GL_DIFFUSE: u32 = 0x1201;
const GL_SPECULAR: u32 = 0x1202;
const GL_POSITION: u32 = 0x1203;
const GL_SPOT_DIRECTION: u32 = 0x1204;
const GL_SPOT_EXPONENT: u32 = 0x1205;
const GL_SPOT_CUTOFF: u32 = 0x1206;
const GL_CONSTANT_ATTENUATION: u32 = 0x1207;
const GL_LINEAR_ATTENUATION: u32 = 0x1208;
const GL_QUADRATIC_ATTENUATION: u32 = 0x1209;
const GL_EMISSION: u32 = 0x1600;
const GL_SHININESS: u32 = 0x1601;
const GL_AMBIENT_AND_DIFFUSE: u32 = 0x1602;
const GL_COLOR_INDEXES: u32 = 0x1603;
const GL_EXP: u32 = 0x800;
const GL_FUNC_ADD: u32 = 0x8006;

impl Query {
    /// OpenGL's initial values.
    pub fn init(&mut self) {
        *self = Query::default();
        for (i, l) in self.lights.iter_mut().enumerate() {
            let on = if i == 0 { 1.0 } else { 0.0 };
            l.ambient = [0.0, 0.0, 0.0, 1.0];
            l.diffuse = [on, on, on, 1.0];
            l.specular = [on, on, on, 1.0];
            l.position = [0.0, 0.0, 1.0, 0.0];
            l.spot_dir = [0.0, 0.0, -1.0];
            l.spot_cutoff = 180.0;
            l.atten = [1.0, 0.0, 0.0];
        }
        for m in self.mats.iter_mut() {
            m.ambient = [0.2, 0.2, 0.2, 1.0];
            m.diffuse = [0.8, 0.8, 0.8, 1.0];
            m.specular = [0.0, 0.0, 0.0, 1.0];
            m.emission = [0.0, 0.0, 0.0, 1.0];
            m.indexes = [0.0, 1.0, 1.0];
        }
        self.lmodel_ambient = [0.2, 0.2, 0.2, 1.0];
        self.cmat = [GL_FRONT_AND_BACK, GL_AMBIENT_AND_DIFFUSE];
        self.fog_mode = GL_EXP;
        self.blend_func = [1, 0];
        self.raster_tex = [0.0, 0.0, 0.0, 1.0];
        self.raster_color = [1.0; 4];
    }

    /// A light parameter token (`light`, GL pname, values), `mv` the
    /// modelview it is given under.
    pub fn light(&mut self, light: u32, pname: u32, v: [f32; 4], mv: &math::Mat4) {
        let Some(l) = self.lights.get_mut(light as usize) else { return };
        match pname {
            GL_AMBIENT => l.ambient = v,
            GL_DIFFUSE => l.diffuse = v,
            GL_SPECULAR => l.specular = v,
            GL_POSITION => l.position = math::xform(mv, v),
            GL_SPOT_DIRECTION => {
                let e = math::xform(mv, [v[0], v[1], v[2], 0.0]);
                l.spot_dir = [e[0], e[1], e[2]];
            }
            GL_SPOT_EXPONENT => l.spot_exp = v[0],
            GL_SPOT_CUTOFF => l.spot_cutoff = v[0],
            GL_CONSTANT_ATTENUATION => l.atten[0] = v[0],
            GL_LINEAR_ATTENUATION => l.atten[1] = v[0],
            GL_QUADRATIC_ATTENUATION => l.atten[2] = v[0],
            _ => {}
        }
    }

    /// glMaterial: face, pname, values.
    pub fn material(&mut self, face: u32, pname: u32, v: [f32; 4]) {
        let faces: &[usize] = match face {
            GL_FRONT => &[0],
            GL_BACK => &[1],
            _ => &[0, 1],
        };
        for &f in faces {
            let m = &mut self.mats[f];
            match pname {
                GL_AMBIENT => m.ambient = v,
                GL_DIFFUSE => m.diffuse = v,
                GL_SPECULAR => m.specular = v,
                GL_EMISSION => m.emission = v,
                GL_SHININESS => m.shininess = v[0],
                GL_AMBIENT_AND_DIFFUSE => (m.ambient, m.diffuse) = (v, v),
                GL_COLOR_INDEXES => m.indexes = [v[0], v[1], v[2]],
                _ => {}
            }
        }
    }

    /// GET_LIGHTFV (light, pname): the parameter into the scratch words.
    pub fn get_light(&mut self, light: u32, pname: u32) {
        let Some(l) = self.lights.get(light as usize) else { return };
        let v: [f32; 4] = match pname {
            GL_AMBIENT => l.ambient,
            GL_DIFFUSE => l.diffuse,
            GL_SPECULAR => l.specular,
            GL_POSITION => l.position,
            GL_SPOT_DIRECTION => [l.spot_dir[0], l.spot_dir[1], l.spot_dir[2], 0.0],
            GL_SPOT_EXPONENT => [l.spot_exp, 0.0, 0.0, 0.0],
            GL_SPOT_CUTOFF => [l.spot_cutoff, 0.0, 0.0, 0.0],
            GL_CONSTANT_ATTENUATION => [l.atten[0], 0.0, 0.0, 0.0],
            GL_LINEAR_ATTENUATION => [l.atten[1], 0.0, 0.0, 0.0],
            GL_QUADRATIC_ATTENUATION => [l.atten[2], 0.0, 0.0, 0.0],
            _ => [0.0; 4],
        };
        self.scratch = v.map(f32::to_bits);
    }

    /// GET_MATERIALFV (face, pname): the parameter into the scratch words.
    pub fn get_material(&mut self, face: u32, pname: u32) {
        let m = &self.mats[(face == GL_BACK) as usize];
        let v: [f32; 4] = match pname {
            GL_AMBIENT => m.ambient,
            GL_DIFFUSE => m.diffuse,
            GL_SPECULAR => m.specular,
            GL_EMISSION => m.emission,
            GL_SHININESS => [m.shininess, 0.0, 0.0, 0.0],
            GL_COLOR_INDEXES => [m.indexes[0], m.indexes[1], m.indexes[2], 0.0],
            _ => [0.0; 4],
        };
        self.scratch = v.map(f32::to_bits);
    }
}

impl Gl {
    /// The GE word at state address `addr`, for the addresses glstate's map
    /// names (see the module comment); None for the rest.
    pub(super) fn query_word(&self, addr: u32) -> Option<u32> {
        let q = &self.q;
        let f = |v: f32| Some(v.to_bits());
        let on = |b: bool| b as u32;
        match addr {
            // Current normal, texture coordinates (SPIN_AND_RETURN).
            8..=10 => f(self.normal[(addr - 8) as usize]),
            11..=14 => f(self.texcoord[(addr - 11) as usize]),
            // Enables, PP1 fill-mode style (probed): polygon stipple bit 0,
            // line stipple bit 5, fog bit 17.
            0x1FA => Some(on(self.poly_stipple != 0) | on(self.line_stipple != 0) << 5 | on(self.lt.fog_on != 0) << 17),
            // Dither bit 0, stencil test bit 4, alpha test bit 6, the logic
            // op in 29:26 (PP1 fill mode's field).
            0x1FC => Some(
                on(self.dither != 0) | on(self.stencil != 0) << 4 | on(self.alpha_test != 0) << 6 | (self.logic_op & 0xF) << 26,
            ),
            // Stencil: func 2:0, fail 5:3, zfail 8:6, zpass 11:9 (KEEP ZERO
            // REPLACE INCR DECR INVERT), ref 19:12.
            0x23A => Some(
                self.st_func & 7
                    | (self.st_ops[0] & 7) << 3
                    | (self.st_ops[1] & 7) << 6
                    | (self.st_ops[2] & 7) << 9
                    | (self.st_ref & 0xFF) << 12,
            ),
            // Stencil value mask 7:0, write mask 15:8.
            0x23C => Some(self.st_cmask & 0xFF | (self.st_wmask & 0xFF) << 8),
            // Depth func 2:0, write mask 27:4 (true when all set).
            0x23E => Some(self.depth_func & 7 | if self.depth_mask != 0 { 0xFF_FFFF << 4 } else { 0 }),
            // Alpha func 2:0, reference 14:3 (12 bits).
            0x240 => Some(self.alpha_func & 7 | (self.alpha_ref & 0xFFF) << 3),
            // The GE's error word (read by glGetError).
            0x2A0 => Some(0),
            0x2BF => f(q.raster_dist),
            0x2C5..=0x2C8 => f(q.raster_color[(addr - 0x2C5) as usize]),
            0x2C9..=0x2CC => f(q.raster_tex[(addr - 0x2C9) as usize]),
            0x32E..=0x331 => Some(q.scratch[(addr - 0x32E) as usize]),
            0x1B9..=0x1C8 => f(self.cm.get()[(addr - 0x1B9) as usize]),
            0x4D2..=0x4D5 => f(q.lmodel_ambient[(addr - 0x4D2) as usize]),
            // glColorMaterial: parameter, then face.
            0x4DF => Some(q.cmat[1]),
            0x4E0 => Some(q.cmat[0]),
            // Clip plane enables: bit 31; the planes, eye space.
            0xAFB..=0xB00 => Some(on(self.clip.user_on >> (addr - 0xAFB) & 1 != 0) << 31),
            0xB01..=0xB18 => {
                let i = (addr - 0xB01) as usize;
                f(self.clip.user[i / 4][i % 4])
            }
            0xB42 => Some(q.fog_mode),
            0xB43 => f(self.fog_density),
            0xB44 => f(self.fog_start),
            0xB45 => f(self.fog_end),
            0xB46..=0xB49 => f(q.fog_color[(addr - 0xB46) as usize]),
            0xB4A => Some(q.fog_index),
            // The depth clear value, 24-bit integer (libGLcore divides by
            // 2^24 - 1).
            0xB86 => Some(self.clear_depth & 0xFF_FFFF),
            0xB8B..=0xB8C => Some(self.polymode[(addr - 0xB8B) as usize]),
            0xB8D => Some(if self.smooth != 0 { 0x1D01 } else { 0x1D00 }),
            0xB8E => Some(self.lt.local_viewer),
            0xB8F => Some(self.lt.two_sided),
            0xBA4 => Some(q.line_smooth),
            0xBB3 => Some(self.cull_mode),
            0xBB4 => Some(if self.front_ccw != 0 { 0x901 } else { 0x900 }),
            // Line stipple as LINE_STIPPLE sends it: the pattern bit-reversed,
            // repeat - 1 (libGLcore undoes both).
            0xBBA => Some(self.line_pattern),
            0xBBB => Some(self.line_repeat),
            0xBBE => Some(self.blend),
            0xBBF => Some(q.blend_func[0]),
            0xBC0 => Some(q.blend_func[1]),
            0xBC1 => Some(if self.blend_eq == 0 { GL_FUNC_ADD } else { self.blend_eq }),
            0xC16 => f(q.poly_offset[0]),
            0xC18 => f(q.poly_offset[1]),
            _ => None,
        }
    }
}
