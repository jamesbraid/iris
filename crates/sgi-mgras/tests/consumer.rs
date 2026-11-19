//! The IMPACT raster engine driven the way a front end other than iris's
//! board would drive it: register writes into `ImpactBack`, no host
//! interface, geometry engine or display.
//!
//! Each case in `lines.streams` is the stream of raster-register writes iris's
//! front end makes for one case of `real_board_lines`, a 64 x 48 GL window of
//! lines or points. The pixels are compared with the ones iris keeps from a
//! real High IMPACT (`real_high_impact_lines.txt`), at the tolerances
//! `real_board_lines` allows.

use sgi_mgras::dev::mgras::back::ImpactBack;
use sgi_mgras::dev::mgras::pixmem::{Buffer, Kind};
use sgi_mgras::dev::mgras::rss::reg;
use sgi_mgras::wgpu::{Gpu, GpuOptions};

const STREAMS: &str = include_str!("lines.streams");
const REAL: &str = include_str!("../../../src/dev/mgras/testdata/real_high_impact_lines.txt");

/// The window's size and the page the X server's layout draws into.
const W: usize = 64;
const H: usize = 48;
const MAIN_PAGE: u32 = 0x240;

const GPU_CASES: &[&str] = &["lloop_w1", "lloop_stipple_3_3333", "points_s3", "lloop_aa_w2"];

fn streams() -> Vec<(&'static str, Vec<&'static str>)> {
    let mut out: Vec<(&str, Vec<&str>)> = Vec::new();
    for l in STREAMS.lines().filter(|l| !l.starts_with('#')) {
        match l.strip_prefix("case ") {
            Some(name) => out.push((name, Vec::new())),
            None => out.last_mut().expect("a case first").1.push(l),
        }
    }
    out
}

/// The board's pixels for a case, 0xBBGGRR, GL rows bottom up.
fn real(case: &str) -> Vec<u32> {
    let mut px = vec![0; W * H];
    let mut on = false;
    for l in REAL.lines().filter(|l| !l.starts_with('#')) {
        if let Some(name) = l.strip_prefix("case ") {
            on = name == case;
        } else if on {
            let v: Vec<&str> = l.split_whitespace().collect();
            let (x, y): (usize, usize) = (v[0].parse().unwrap(), v[1].parse().unwrap());
            let rgb = u32::from_str_radix(v[2], 16).unwrap();
            px[y * W + x] = (rgb >> 16) | (rgb & 0xFF00) | (rgb & 0xFF) << 16;
        }
    }
    px
}

fn number(s: &str) -> u32 {
    u32::from_str_radix(s.trim_start_matches("0x"), 16).unwrap()
}

fn replay(back: &mut ImpactBack, stream: &[&str]) {
    for l in stream {
        let f: Vec<&str> = l.split_whitespace().collect();
        match f[0] {
            "w" => {
                let (r, mut v) = (number(f[1]), number(f[2]));
                // The streams are a one-engine board's: say two engines in
                // drb_size when two draw.
                if r == reg::MGRAS_PP1_DRB_SIZE && back.engines() == 2 {
                    v |= 0x40;
                }
                back.write(r, v, f[3] == "1");
            }
            "enter" => back.gl_enter(),
            "leave" => back.gl_leave(),
            other => panic!("{other}"),
        }
    }
    back.sync().expect("sync");
}

fn pixels(back: &ImpactBack) -> Vec<u32> {
    let buffer = Buffer::new(MAIN_PAGE, Kind::Wide, back.reg(reg::MGRAS_PP1_DRB_SIZE));
    (0..H).flat_map(|y| (0..W).map(move |x| (x, y))).map(|(x, y)| back.pixel(&buffer, x as u32, y as u32) as u32 & 0xFF_FFFF).collect()
}

fn tolerance(case: &str) -> u32 {
    if case.contains("smooth") {
        1
    } else if case.contains("_aa") {
        5
    } else {
        0
    }
}

fn differing(case: &str, got: &[u32]) -> usize {
    let (want, tol) = (real(case), tolerance(case));
    let near = |a: u32, b: u32| (0..3).all(|k| ((a >> (8 * k)) & 0xFF).abs_diff((b >> (8 * k)) & 0xFF) <= tol);
    (0..W * H).filter(|&i| !near(got[i], want[i])).count()
}

#[test]
fn cpu_matches_the_real_board() {
    for (case, stream) in streams() {
        let mut back = ImpactBack::new(None).expect("back end");
        replay(&mut back, &stream);
        let got = pixels(&back);

        assert!(got.iter().any(|&p| p != 0), "{case} drew nothing");
        assert_eq!(differing(case, &got), 0, "{case}: pixels differ from the board");
    }
}

#[test]
fn the_comparison_sees_a_wrong_frame() {
    let streams = streams();
    let mut back = ImpactBack::new(None).expect("back end");
    replay(&mut back, &streams.iter().find(|s| s.0 == "lloop_w1").unwrap().1);

    assert!(differing("lloop_w3", &pixels(&back)) > 0);
}

#[test]
fn gpu_stores_what_the_cpu_stores() {
    let gpu = match Gpu::new(&GpuOptions::default(), Box::new(|m| panic!("GPU failed: {m}"))) {
        Ok(g) => g,
        Err(e) => {
            eprintln!("skipped: {e}");
            return;
        }
    };
    // Building a GPU unit compiles its kernels, so a few cases will do.
    for (case, stream) in streams().into_iter().filter(|(c, _)| GPU_CASES.contains(c)) {
        let mut cpu = ImpactBack::new(None).expect("cpu");
        let mut back = ImpactBack::new(Some(&gpu)).expect("gpu");
        replay(&mut cpu, &stream);
        replay(&mut back, &stream);

        assert_eq!(back.fault(), None, "{case}");
        let (_, records) = back.gpu_counts().expect("the GPU unit");
        assert!(records > 0, "{case}: nothing reached the GPU");
        assert_eq!(pixels(&back), pixels(&cpu), "{case}: GPU and CPU words differ");
        assert_eq!(differing(case, &pixels(&back)), 0, "{case}: pixels differ from the board");
    }
}

/// Two engines stepped on one thread, each drawing its own scanlines, store
/// what one drawing them all does.
#[test]
fn two_stepped_engines_draw_what_one_does() {
    for (case, stream) in streams() {
        let mut one = ImpactBack::new(None).expect("one engine");
        let mut two = ImpactBack::with_engines(None, 2).expect("two engines");
        replay(&mut one, &stream);
        replay(&mut two, &stream);

        assert!(pixels(&two).iter().any(|&p| p != 0), "{case} drew nothing");
        assert_eq!(pixels(&two), pixels(&one), "{case}");
    }
}
