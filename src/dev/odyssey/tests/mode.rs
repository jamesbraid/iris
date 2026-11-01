//! The 0x0404 mode register's cull fields, as the model reads them.

use crate::dev::odyssey::consts::mode;
use crate::dev::odyssey::gl_context::{GL_BACK, GL_FRONT, GL_FRONT_AND_BACK};
use crate::dev::odyssey::tests::board;

/// Bit 8 with a face in bits 10:9 culls that face. Bit 8 with 00 is never
/// sent; the model culls nothing for it and does not keep an earlier face.
/// Bit 8 clear culls nothing whatever the face bits hold.
#[test]
fn cull_follows_the_word_alone() {
    let (mut s, _) = board("v12");
    let word = |enable: bool, faces: u32| (if enable { mode::CULL_FACE } else { 0 }) | (faces << mode::CULL_MODE_SHIFT);
    // (enable, faces, culls, face when it culls)
    let table = [
        (true, 1, true, GL_BACK),
        (true, 2, true, GL_FRONT),
        (true, 3, true, GL_FRONT_AND_BACK),
        (true, 0, false, 0),
        (false, 0, false, 0),
        (false, 3, false, 0),
        (true, 3, true, GL_FRONT_AND_BACK),
        (true, 0, false, 0),
    ];

    for (enable, faces, culls, face) in table {
        s.stream.pattern_mode = word(enable, faces);
        s.gl_apply_mode();
        let cur = &s.gl_ref().cur;
        assert_eq!(cur.cull_face, culls, "enable {enable}, faces {faces:02b}");
        if culls {
            assert_eq!(cur.cull_face_mode, face, "enable {enable}, faces {faces:02b}");
        }
    }
}
