//! Odyssey scalar geometry arithmetic, kept separate from Iris's f32
//! geometry helpers to preserve the QEMU model's arithmetic.
//!
//! Ported from hw/display/sgi/gl-context.c and hw/display/sgi/rasterizer.c
//! in jamesbraid/qemu-sgi-origami.

pub type Matrix = [f32; 16];
pub fn multiply(m: &mut Matrix, b: &[f64; 16]) {
    let mut result = [0f32; 16];
    for col in 0..4 {
        for row in 0..4 {
            let mut sum = 0f64;
            for k in 0..4 {
                sum += (m[k * 4 + row] as f64) * b[col * 4 + k];
            }
            result[col * 4 + row] = sum as f32;
        }
    }
    *m = result;
}
pub fn invert(m: &Matrix) -> Option<[f64; 16]> {
    let a = m.map(f64::from);
    let mut inv = [0f64; 16];
    for col in 0..4 {
        for row in 0..4 {
            let mut minor = [0f64; 9];
            let mut n = 0;
            for c in 0..4 {
                for r in 0..4 {
                    if c != col && r != row {
                        minor[n] = a[c * 4 + r];
                        n += 1;
                    }
                }
            }
            inv[row * 4 + col] = if (row + col) & 1 != 0 { -1.0 } else { 1.0 }
                * (minor[0] * (minor[4] * minor[8] - minor[5] * minor[7])
                    - minor[3] * (minor[1] * minor[8] - minor[2] * minor[7])
                    + minor[6] * (minor[1] * minor[5] - minor[2] * minor[4]));
        }
    }
    let mut det = 0f64;
    for k in 0..4 {
        det += a[k * 4] * inv[k];
    }
    if det == 0.0 || !det.is_finite() {
        return None;
    }
    for v in &mut inv {
        *v /= det;
    }
    Some(inv)
}
pub fn index_value(bits: u32, index: f64) -> u32 {
    let mask = if bits >= 32 {
        u32::MAX
    } else {
        (1u32 << bits) - 1
    };
    let f = index.floor();
    if !(f >= -2147483648.0 && f < 4294967296.0) {
        return 0;
    }
    (if f < 0.0 { f as i32 as u32 } else { f as u32 }) & mask
}
/// No near-eye cutoff or snapping is introduced at projection.
pub fn clip_to_window(clip: [f64; 4], viewport: [i32; 4], window_matrix: bool) -> [f64; 3] {
    let x = if window_matrix {
        clip[0] * 16384.0 / clip[3]
    } else {
        (clip[0] / clip[3] + 1.0) * viewport[2] as f64 / 2.0 + viewport[0] as f64
    };
    let y = if window_matrix {
        clip[1] * 16384.0 / clip[3]
    } else {
        (clip[1] / clip[3] + 1.0) * viewport[3] as f64 / 2.0 + viewport[1] as f64
    };
    [x, y, (clip[2] / clip[3] + 1.0) / 2.0]
}
#[derive(Clone, Copy, Debug)]
pub struct Vertex {
    pub clip: [f64; 4],
    pub eye: [f64; 4],
    pub attr: [f32; 24],
}
impl Vertex {
    fn plane(&self, plane: usize) -> f64 {
        let c = self.clip[plane / 2];
        if plane & 1 != 0 {
            self.clip[3] - c
        } else {
            self.clip[3] + c
        }
    }
    pub fn inside(&self) -> bool {
        (0..6).all(|p| self.plane(p) >= 0.0)
    }
    fn lerp(inside: &Self, outside: &Self, t: f64) -> Self {
        let mut result = *inside;
        for k in 0..4 {
            result.clip[k] = inside.clip[k] + t * (outside.clip[k] - inside.clip[k]);
            result.eye[k] = inside.eye[k] + t * (outside.eye[k] - inside.eye[k]);
        }
        for k in 0..24 {
            // Material and edge are bit patterns, not float attributes.
            if k != 11 && k != 17 {
                result.attr[k] =
                    (inside.attr[k] as f64 + t * (outside.attr[k] - inside.attr[k]) as f64) as f32;
            }
        }
        result
    }
    fn cut(inside: &Self, din: f64, outside: &Self, dout: f64) -> Self {
        Self::lerp(inside, outside, din / (din - dout))
    }
}
pub fn clip_triangle(input: [Vertex; 3]) -> Vec<Vertex> {
    let mut src = input.to_vec();
    if input.iter().all(Vertex::inside) {
        return src;
    }
    for plane in 0..6 {
        let mut dst = Vec::with_capacity(src.len() + 1);
        for i in 0..src.len() {
            let a = &src[i];
            let b = &src[(i + 1) % src.len()];
            let da = a.plane(plane);
            let db = b.plane(plane);
            if da >= 0.0 {
                dst.push(*a);
                if !(db >= 0.0) {
                    dst.push(Vertex::cut(a, da, b, db));
                }
            } else if db >= 0.0 {
                dst.push(Vertex::cut(b, db, a, da));
            }
        }
        if dst.len() < 3 {
            return Vec::new();
        }
        src = dst;
    }
    src
}
pub fn clip_segment(a: &mut Vertex, b: &mut Vertex) -> bool {
    let mut sa = 1f64;
    let mut tb = 1f64;
    for p in 0..6 {
        let da = a.plane(p);
        let db = b.plane(p);
        if da >= 0.0 && db >= 0.0 {
            continue;
        }
        if !(da >= 0.0) && !(db >= 0.0) {
            return false;
        }
        if da >= 0.0 {
            tb = donor_min(tb, da / (da - db));
        } else {
            sa = donor_min(sa, db / (db - da));
        }
    }
    if !(sa + tb >= 1.0) {
        return false;
    }
    let na = if sa < 1.0 { Vertex::lerp(b, a, sa) } else { *a };
    let nb = if tb < 1.0 { Vertex::lerp(a, b, tb) } else { *b };
    *a = na;
    *b = nb;
    true
}

fn donor_min(a: f64, b: f64) -> f64 {
    if a < b {
        a
    } else {
        b
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MatrixMode {
    Modelview,
    Projection,
    Texture,
}
struct Stack {
    current: Matrix,
    saved: Vec<Matrix>,
}
impl Default for Stack {
    fn default() -> Self {
        let mut current = [0f32; 16];
        for i in 0..4 {
            current[i * 5] = 1.0;
        }
        Self {
            current,
            saved: Vec::with_capacity(31),
        }
    }
}
/// The donor gives all three stacks a capacity of 32 including current.
/// Overflow and underflow leave the current matrix and depth unchanged.
pub struct Matrices {
    pub mode: MatrixMode,
    stack: [Stack; 3],
}
impl Default for Matrices {
    fn default() -> Self {
        Self {
            mode: MatrixMode::Modelview,
            stack: std::array::from_fn(|_| Stack::default()),
        }
    }
}
impl Matrices {
    fn slot(mode: MatrixMode) -> usize {
        match mode {
            MatrixMode::Modelview => 0,
            MatrixMode::Projection => 1,
            MatrixMode::Texture => 2,
        }
    }
    pub fn get(&self, mode: MatrixMode) -> &Matrix {
        &self.stack[Self::slot(mode)].current
    }
    pub fn set(&mut self, mode: MatrixMode, matrix: Matrix) {
        self.stack[Self::slot(mode)].current = matrix;
    }
    pub fn depth(&self, mode: MatrixMode) -> usize {
        1 + self.stack[Self::slot(mode)].saved.len()
    }
    pub fn multiply(&mut self, b: &[f64; 16]) {
        multiply(&mut self.stack[Self::slot(self.mode)].current, b);
    }
    pub fn push(&mut self) -> bool {
        let s = &mut self.stack[Self::slot(self.mode)];
        if s.saved.len() == 31 {
            return false;
        }
        s.saved.push(s.current);
        true
    }
    pub fn pop(&mut self) -> bool {
        let s = &mut self.stack[Self::slot(self.mode)];
        if let Some(current) = s.saved.pop() {
            s.current = current;
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn matrix_products_accumulate_in_double() {
        let mut m = [0f32; 16];
        m[0] = 16777216.0;
        m[4] = 1.0;
        m[8] = -16777216.0;
        let mut b = [0f64; 16];
        b[0] = 1.0;
        b[1] = 1.0;
        b[2] = 1.0;
        multiply(&mut m, &b);
        assert_eq!(m[0], 1.0);
    }

    #[test]
    fn inversion_rejects_singular_and_nonfinite() {
        assert!(invert(&[0.0; 16]).is_none());
        let mut m = [0f32; 16];
        for i in 0..4 {
            m[i * 5] = 1.0;
        }
        let inv = invert(&m).unwrap();
        assert_eq!(inv, m.map(f64::from));
        m[0] = f32::INFINITY;
        assert!(invert(&m).is_none());
    }
    /// The donor's scalar index-value cases, without its QEMU fixture.

    #[test]
    fn colour_index_wraps_after_floor() {
        for (bits, value, expected) in [
            (8, 2.0, 2),
            (8, 2.99, 2),
            (8, 300.0, 44),
            (8, -1.0, 255),
            (12, 4095.5, 4095),
            (32, 4294967295.0, u32::MAX),
            (8, f64::NAN, 0),
        ] {
            assert_eq!(index_value(bits, value), expected);
        }
    }
}
