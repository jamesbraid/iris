use std::sync::Arc;
use std::time::{Duration, Instant};
use parking_lot::Mutex;
use winit::{
    application::ApplicationHandler,
    event::{DeviceEvent, DeviceId, ElementState, KeyEvent, WindowEvent, MouseButton},
    event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy},
    keyboard::{KeyCode, PhysicalKey},
    window::{Window, WindowAttributes, WindowId},
};
use glow::HasContext;
use crate::dev::ps2::Ps2Controller;
use crate::config::ScreenWindows;
use crate::gfx_display::{Cycle, Frame, Renderer, Screen};
use crate::side_by_side::SideBySide;
use crate::disp::{StatusBar, StatusBarTexture, BarStats, STATUS_BAR_HEIGHT};
use crate::gl_compositor::GlCompositor;
use crate::hptimer::{TimerManager, TimerReturn};
use crate::workstation::DiscChanger;
use glutin::config::ConfigTemplateBuilder;
use glutin::context::{ContextApi, ContextAttributesBuilder, GlProfile, NotCurrentContext, PossiblyCurrentContext, Version};
use glutin::display::GetGlDisplay;
use glutin::prelude::*;
use glutin::surface::{GlSurface, SurfaceAttributesBuilder, SwapInterval, WindowSurface, Surface};
use glutin_winit::{ApiPreference, DisplayBuilder};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use std::num::NonZeroU32;
use std::ffi::CString;

// Vertex layout: [pos_x, pos_y, tex_u, tex_v] × 4 vertices = 64 bytes
const VBO_SIZE: i32 = 64;

struct GlState {
    gl: glow::Context,
    context: PossiblyCurrentContext,
    surface: Surface<WindowSurface>,
    // Integer texelFetch shader (used at 1x and 2x exact scales)
    integer_program:      glow::Program,
    viewport_info_loc:    Option<glow::UniformLocation>,
    scale_factor_loc:     Option<glow::UniformLocation>,
    // Fallback UV sampler shader with mipmap/trilinear (used at non-integer scales)
    fallback_program:     glow::Program,
    fallback_tex_loc:     Option<glow::UniformLocation>,
    fallback_ortho_loc:   Option<glow::UniformLocation>,
    // Shared VAO + two VBOs: emulator quad and status-bar quad
    vao:        glow::VertexArray,
    main_vbo:   glow::Buffer,
    status_vbo: glow::Buffer,
}

// Pending window-resize request, applied by the event-loop thread only.
//
// Every `Window` method that changes the window's size must run on the thread
// that owns the window (the one running the event loop). On Windows this is not
// merely a convention: `request_inner_size` reaches `SetWindowPos`, and
// `SetWindowPos` on a window owned by *another* thread makes Win32 dispatch
// `WM_WINDOWPOSCHANGING`/`WM_SIZE` **synchronously into the owning thread's
// wndproc** (`SWP_ASYNCWINDOWPOS` only affects the caller's return, not the
// callback delivery). winit's Windows `EventLoopRunner` holds its event handler
// and buffer in `Cell`/`RefCell` with no synchronisation whatsoever, so that
// re-entrant dispatch races the event thread's own dispatch and panics inside a
// Win32 callback — which the OS turns into a process kill with
// STATUS_FATAL_USER_CALLBACK_EXCEPTION (0xC000041D), no Rust backtrace.
// Symptom seen in the wild: iris dying ~90% of launches right after REX3
// reports its first resolution change, because that change is exactly what used
// to call `request_inner_size` from the REX3 refresh thread.
//
// So both resize sources — the REX3 refresh thread's mode change and the
// RCtrl+1/2 hotkey — only *record* what they want here; `UiApp` applies it.
#[derive(Clone, Copy, PartialEq)]
enum ResizeRequest {
    /// Aspect-lock correction: set the window to exactly this size.
    Fit { w: u32, h: u32 },
    /// Snap to an integer multiple of the current display. Sent by RCtrl+1/2,
    /// and by the renderer on every guest mode change with the configured
    /// scale (`--2x` / `scale =`), so a 2x window stays 2x across boot.
    Scale(u32),
    /// RCtrl+F11: toggle borderless fullscreen. Same deferral rationale as the
    /// others — `set_fullscreen` inside a callback re-enters the wndproc with a
    /// whole style/geometry change (WM_WINDOWPOSCHANGING, WM_SIZE,
    /// possibly WM_DPICHANGED when the window lands on a different monitor).
    ToggleFullscreen,
}

// The context we got: Core32 runs GlCompositor and the texelFetch shaders;
// Legacy (GL 2.1) draws words with a copy shader and Newport composes on the CPU.
#[derive(Clone, Copy, PartialEq, Debug)]
enum GlTier { Core32, Legacy }

struct GlRenderer {
    window:      Arc<Window>,
    gl_config:   glutin::config::Config,
    // Created on the main thread (where the window/display were set up) and
    // handed across to the refresh thread, which only calls make_current() on
    // it. Some drivers (notably proprietary NVIDIA) are picky about GLX calls
    // that validate against the drawable/FBConfig being issued from a thread
    // other than the one that created the X11 connection/window.
    not_current_context: Option<NotCurrentContext>,
    // What made that context, to make another when stop() could not give it back.
    context_attributes: glutin::context::ContextAttributes,
    // The window surface, also built on the main thread in Ui::new() and bound
    // once there (make_current → make_not_current) before the event loop runs.
    // On Windows that first bind is what installs the GL ICD's window subclass
    // and populates its per-HWND state; doing it lazily on the refresh thread
    // while the event loop is already dispatching a startup resize raced the
    // half-initialised driver (AMD atio6axx.dll null read — issue #94).
    // `ensure_init()` consumes this on the true first frame; on a stop()/start()
    // respawn it is already `None` and a fresh surface is made on the refresh
    // thread (safe by then — the driver's window state exists).
    // See clanker/gui/windows-silent-exit-0xc000041d.md.
    initial_surface: Option<Surface<WindowSurface>>,
    // Captured on the main thread in Ui::new(): winit 0.30 macOS returns
    // HandleError::Unavailable from window_handle() on any other thread, and
    // init_gl() runs on the refresh thread.
    raw_window_handle: RawWindowHandle,
    gl_tier: GlTier,
    window_size: Arc<Mutex<Option<(u32, u32)>>>,
    // Resize wanted by this (render) thread, applied by the event thread.
    // Never call a window-mutating method from here — see `ResizeRequest`.
    resize_request: Arc<Mutex<Option<ResizeRequest>>>,
    // Waker for the event loop: the resize request above is useless until the
    // event thread wakes up to notice it, and with ControlFlow::Wait an idle
    // guest may produce no OS events at all for a long time.
    event_proxy: EventLoopProxy<()>,
    // Configured window scale; each guest mode change snaps to this multiple.
    scale: u32,
    // Current emulated display resolution (width, height), published for the
    // event thread so it can lock the window to the right aspect ratio.
    display_res: Arc<Mutex<(u32, u32)>>,
    state:       Option<GlState>,
    // Newport's planes composed in a fragment shader: the frame's planes kind.
    // None on a legacy context, where Newport composes on the CPU instead.
    compositor:  Option<GlCompositor>,
    // The status bar, drawn here under every board's frame.
    status:      StatusBar,
    sbtex:       StatusBarTexture,
    // Textures for the frame's words and its debug overlay, made on first use.
    words_tex:   Option<glow::Texture>,
    overlay_tex: Option<glow::Texture>,
    // The texture the last new frame left, and that frame's sequence number:
    // a repeat of it is drawn again without composing or uploading.
    main_tex:    Option<glow::Texture>,
    seen_seq:    u64,
    current_w:     usize,
    current_h:     usize,
    current_win_w: usize,
    current_win_h: usize,
    // Width of the words texture: a frame wider than the usual stride (the
    // side-by-side window) needs a wider one.
    tex_w:         usize,
}

// Safety: GlRenderer is sent to the refresh thread where it owns and uses the GL context.
// No other thread touches these fields.
unsafe impl Send for GlRenderer {}

impl GlRenderer {
    /// GL setup on the first call, and `make_current` on this thread on every
    /// call (a respawned refresh thread is a new thread). `false` skips the frame.
    fn ensure_init(&mut self) -> bool {
        if let Some(state) = &self.state {
            return state.context.make_current(&state.surface).is_ok();
        }

        let gl_display = self.gl_config.display();

        // The context itself was already created on the main thread in
        // Ui::new() (see comment on not_current_context) — this is that
        // context's one and only real consumption, guarded by the
        // `self.state.is_none()` branch above never running a second time.
        let not_current_gl_context = match self.not_current_context.take() {
            Some(c) => c,
            // A stop() that could not release the context lost it.
            None => match unsafe { gl_display.create_context(&self.gl_config, &self.context_attributes) } {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("iris: the GL context was lost and a new one failed: {e}");
                    return false;
                }
            },
        };

        // Normal boot: the surface was created and bound once on the main
        // thread in Ui::new() — reuse it. Only after a stop()/start() cycle
        // (jitcheck checkpoint restore, `reset`, snapshot load) is it already
        // gone, and then a fresh one on this thread is fine: the driver's
        // per-HWND GL state was established at startup.
        let gl_surface = self.initial_surface.take().unwrap_or_else(|| {
            let size = self.window.inner_size();
            let attrs = SurfaceAttributesBuilder::<WindowSurface>::new().build(
                self.raw_window_handle,
                NonZeroU32::new(size.width.max(1)).unwrap(),
                NonZeroU32::new(size.height.max(1)).unwrap(),
            );
            unsafe {
                gl_display
                    .create_window_surface(&self.gl_config, &attrs)
                    .unwrap()
            }
        });

        let gl_context = not_current_gl_context.make_current(&gl_surface).unwrap();

        let gl = unsafe {
            glow::Context::from_loader_function(|s| {
                gl_display.get_proc_address(&CString::new(s).unwrap())
            })
        };

        let _ = gl_surface.set_swap_interval(&gl_context, SwapInterval::Wait(NonZeroU32::new(1).unwrap()));

        // In the Legacy tier we don't have GL 3.2 core (no texelFetch, no usampler2D),
        // so both the vertex and fragment shaders are written as plain GLSL 120 —
        // varying/gl_FragColor instead of in/out — and there is no integer shader at all.
        let legacy = self.gl_tier == GlTier::Legacy;

        let (integer_program, viewport_info_loc, scale_factor_loc,
             fallback_program, fallback_tex_loc, fallback_ortho_loc,
             vao, main_vbo, status_vbo) = unsafe {

            // Shared vertex shader: pixel-coordinate ortho projection.
            // ortho = (win_w, win_h); (0,0)=top-left, y increases downward.
            let vs_src_150 = "
                #version 150
                in vec2 position;
                in vec2 tex_coord;
                out vec2 v_tex_coord;
                uniform vec2 ortho;
                void main() {
                    vec2 ndc = (position / ortho) * 2.0 - 1.0;
                    gl_Position = vec4(ndc.x, -ndc.y, 0.0, 1.0);
                    v_tex_coord = tex_coord;
                }
            ";
            let vs_src_120 = "
                #version 120
                attribute vec2 position;
                attribute vec2 tex_coord;
                varying vec2 v_tex_coord;
                uniform vec2 ortho;
                void main() {
                    vec2 ndc = (position / ortho) * 2.0 - 1.0;
                    gl_Position = vec4(ndc.x, -ndc.y, 0.0, 1.0);
                    v_tex_coord = tex_coord;
                }
            ";

            // ── Integer shader: texelFetch, no filtering. Used at 1x and 2x. ──
            // scale_factor must be an exact integer (1 or 2). GL 3.2 core only —
            // there is no legacy equivalent, Legacy tier always uses the fallback shader.
            let integer_fs_src = "
                #version 150
                in vec2 v_tex_coord;
                out vec4 color;
                uniform sampler2D tex;
                uniform ivec2 viewport_info[2];
                uniform int scale_factor;
                void main() {
                    int tex_h  = viewport_info[0].y;
                    int quad_y = viewport_info[1].y;
                    int scale  = max(scale_factor, 1);
                    int x = int(gl_FragCoord.x) / scale;
                    int y = (int(gl_FragCoord.y) - quad_y) / scale;
                    y = (tex_h - 1) - y;
                    color = texelFetch(tex, ivec2(x, y), 0);
                }
            ";

            // ── Fallback shader: UV sampler with trilinear filtering. ──
            // Caller sets TEXTURE_MIN_FILTER = LINEAR_MIPMAP_LINEAR and generates mipmaps.
            let fallback_fs_src_150 = "
                #version 150
                in vec2 v_tex_coord;
                out vec4 color;
                uniform sampler2D tex;
                void main() {
                    color = texture(tex, v_tex_coord);
                }
            ";
            let fallback_fs_src_120 = "
                #version 120
                varying vec2 v_tex_coord;
                uniform sampler2D tex;
                void main() {
                    gl_FragColor = texture2D(tex, v_tex_coord);
                }
            ";

            let vs_src = if legacy { vs_src_120 } else { vs_src_150 };
            let fallback_fs_src = if legacy { fallback_fs_src_120 } else { fallback_fs_src_150 };

            let compile_shader = |kind: u32, src: &str| -> Option<glow::Shader> {
                let s = gl.create_shader(kind).unwrap();
                gl.shader_source(s, src);
                gl.compile_shader(s);
                if gl.get_shader_compile_status(s) { Some(s) } else {
                    eprintln!("Shader compile error: {}", gl.get_shader_info_log(s));
                    gl.delete_shader(s);
                    None
                }
            };

            let link_program = |vs: glow::Shader, fs: glow::Shader| -> Option<glow::Program> {
                let p = gl.create_program().unwrap();
                gl.attach_shader(p, vs);
                gl.attach_shader(p, fs);
                gl.link_program(p);
                if gl.get_program_link_status(p) { Some(p) } else {
                    eprintln!("Program link error: {}", gl.get_program_info_log(p));
                    gl.delete_program(p);
                    None
                }
            };

            let vs = compile_shader(glow::VERTEX_SHADER, vs_src)
                .expect("vertex shader must compile");

            // Integer program is GL 3.2 core only (#version 150, texelFetch) and has no
            // legacy equivalent — linking it against the 120 vertex shader would be an
            // invalid mixed-version program, so skip it outright in Legacy tier and always
            // draw through fallback_program there. In Core32 tier, try to compile it;
            // fall back to fallback_program if it fails.
            let int_fs    = if legacy { None } else { compile_shader(glow::FRAGMENT_SHADER, integer_fs_src) };
            let int_prog  = int_fs.and_then(|fs| link_program(vs, fs));

            // Fallback program — must always compile.
            let fb_fs   = compile_shader(glow::FRAGMENT_SHADER, fallback_fs_src)
                .expect("fallback fragment shader must compile");
            let fb_prog = link_program(vs, fb_fs)
                .expect("fallback program must link");

            // If integer program failed, reuse fallback as integer_program too (same draw path,
            // just won't do texelFetch).
            let integer_program = int_prog.unwrap_or(fb_prog);

            let viewport_info_loc = gl.get_uniform_location(integer_program, "viewport_info");
            let scale_factor_loc  = gl.get_uniform_location(integer_program, "scale_factor");
            let fallback_tex_loc  = gl.get_uniform_location(fb_prog, "tex");
            let fallback_ortho_loc = gl.get_uniform_location(fb_prog, "ortho");

            let vao = gl.create_vertex_array().unwrap();
            gl.bind_vertex_array(Some(vao));

            let main_vbo = gl.create_buffer().unwrap();
            gl.bind_buffer(glow::ARRAY_BUFFER, Some(main_vbo));
            gl.buffer_data_size(glow::ARRAY_BUFFER, VBO_SIZE, glow::DYNAMIC_DRAW);
            Self::bind_vbo_attribs(&gl, integer_program, main_vbo);

            let status_vbo = gl.create_buffer().unwrap();
            gl.bind_buffer(glow::ARRAY_BUFFER, Some(status_vbo));
            gl.buffer_data_size(glow::ARRAY_BUFFER, VBO_SIZE, glow::DYNAMIC_DRAW);
            Self::bind_vbo_attribs(&gl, integer_program, status_vbo);

            (integer_program, viewport_info_loc, scale_factor_loc,
             fb_prog, fallback_tex_loc, fallback_ortho_loc,
             vao, main_vbo, status_vbo)
        };

        self.state = Some(GlState {
            gl,
            context: gl_context,
            surface: gl_surface,
            integer_program,
            viewport_info_loc,
            scale_factor_loc,
            fallback_program,
            fallback_tex_loc,
            fallback_ortho_loc,
            vao,
            main_vbo,
            status_vbo,
        });
        true
    }

    // Upload a quad covering pixel rect [x0..x1] × [y0..y1] (top-left origin, y down).
    unsafe fn upload_quad(gl: &glow::Context, vbo: glow::Buffer,
        x0: f32, y0: f32, x1: f32, y1: f32,
        u0: f32, v0: f32, u1: f32, v1: f32)
    {
        let vertices: [f32; 16] = [
            x0, y0,  u0, v0,
            x1, y0,  u1, v0,
            x0, y1,  u0, v1,
            x1, y1,  u1, v1,
        ];
        gl.bind_buffer(glow::ARRAY_BUFFER, Some(vbo));
        let u8_slice = std::slice::from_raw_parts(vertices.as_ptr() as *const u8, vertices.len() * 4);
        gl.buffer_sub_data_u8_slice(glow::ARRAY_BUFFER, 0, u8_slice);
    }

    unsafe fn bind_vbo_attribs(gl: &glow::Context, program: glow::Program, vbo: glow::Buffer) {
        gl.bind_buffer(glow::ARRAY_BUFFER, Some(vbo));
        let pos_loc = gl.get_attrib_location(program, "position").unwrap();
        gl.enable_vertex_attrib_array(pos_loc);
        gl.vertex_attrib_pointer_f32(pos_loc, 2, glow::FLOAT, false, 16, 0);
        if let Some(tex_loc) = gl.get_attrib_location(program, "tex_coord") {
            gl.enable_vertex_attrib_array(tex_loc);
            gl.vertex_attrib_pointer_f32(tex_loc, 2, glow::FLOAT, false, 16, 8);
        }
    }

    // Set trilinear filtering and regenerate mipmaps for a bound TEXTURE_2D.
    // Call after binding the texture, before drawing.
    unsafe fn setup_trilinear(gl: &glow::Context) {
        gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MIN_FILTER, glow::LINEAR_MIPMAP_LINEAR as i32);
        gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MAG_FILTER, glow::LINEAR as i32);
        gl.generate_mipmap(glow::TEXTURE_2D);
    }

    // Draw a texture using the integer texelFetch shader.
    // quad_y_bottom: bottom edge of the quad in GL pixels from bottom of window.
    // scale_i: exact integer scale (1 or 2).
    unsafe fn draw_tex_integer(
        gl: &glow::Context,
        state: &GlState,
        vbo: glow::Buffer,
        tex: glow::Texture,
        tex_w: i32, tex_h: i32,
        quad_y_bottom: i32,
        scale_i: i32,
        win_w: f32, win_h: f32,
    ) {
        gl.use_program(Some(state.integer_program));
        if let Some(loc) = gl.get_uniform_location(state.integer_program, "ortho") {
            gl.uniform_2_f32(Some(&loc), win_w, win_h);
        }
        gl.bind_vertex_array(Some(state.vao));
        gl.bind_texture(glow::TEXTURE_2D, Some(tex));
        Self::setup_trilinear(gl);
        Self::bind_vbo_attribs(gl, state.integer_program, vbo);
        // viewport_info[0] = (tex_w, tex_h), viewport_info[1] = (0, quad_y_bottom)
        let info = [tex_w, tex_h, 0, quad_y_bottom];
        gl.uniform_2_i32_slice(state.viewport_info_loc.as_ref(), &info);
        if let Some(loc) = &state.scale_factor_loc {
            gl.uniform_1_i32(Some(loc), scale_i);
        }
        gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
    }

    // Draw a texture using the fallback UV sampler shader with trilinear filtering.
    unsafe fn draw_tex_fallback(
        gl: &glow::Context,
        state: &GlState,
        vbo: glow::Buffer,
        tex: glow::Texture,
        win_w: f32, win_h: f32,
    ) {
        gl.use_program(Some(state.fallback_program));
        if let Some(loc) = &state.fallback_ortho_loc {
            gl.uniform_2_f32(Some(loc), win_w, win_h);
        }
        if let Some(loc) = &state.fallback_tex_loc {
            gl.uniform_1_i32(Some(loc), 0);
        }
        gl.bind_vertex_array(Some(state.vao));
        gl.active_texture(glow::TEXTURE0);
        gl.bind_texture(glow::TEXTURE_2D, Some(tex));
        Self::setup_trilinear(gl);
        Self::bind_vbo_attribs(gl, state.fallback_program, vbo);
        gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
    }

    // Compute letterboxed quad rect and float scale for a given display and window size.
    // Returns (x0, y0, x1, y1, scale_f) in window pixel coords (top-left origin).
    // The status bar scales with the display: total content height = indy_h + STATUS_BAR_HEIGHT.
    fn letterbox(indy_w: f32, indy_h: f32, win_w: f32, win_h: f32)
        -> (f32, f32, f32, f32, f32)
    {
        let total_h = indy_h + STATUS_BAR_HEIGHT as f32;
        let scale = (win_w / indy_w).min(win_h / total_h);
        let disp_w = indy_w * scale;
        let disp_h = indy_h * scale;
        let sb_h   = STATUS_BAR_HEIGHT as f32 * scale;
        let x0 = ((win_w - disp_w) * 0.5).floor();
        let y0 = ((win_h - disp_h - sb_h) * 0.5).floor();
        (x0, y0, x0 + disp_w, y0 + disp_h, scale)
    }
}

impl Renderer for GlRenderer {
    fn present(&mut self, frame: &Frame, stats: &BarStats) {
        // ensure_init(): runs the one-time GL setup (shader compile, VAO/
        // VBO creation, etc.) on whichever thread's first `present()` call
        // reaches it — the context genuinely does need to be created here
        // rather than back in Ui::new(), since some drivers require the
        // context to stay tied to the thread that will actually issue GL
        // calls against it (see `not_current_context`'s field comment).
        // Idempotent: safe to call on every `present()`, every frame — it
        // does real setup work only the very first time (`self.state` still
        // `None`), and on every other call just makes sure the context is
        // current *on this OS thread* before any GL call below runs. REX3's
        // refresh thread is respawned on every stop()/start() cycle
        // (jitcheck's checkpoint capture/restore among the real callers) —
        // a *different* OS thread each time — and a GL context's "current"
        // binding is per-thread, so `state` surviving the respawn is not by
        // itself enough; the context must be re-made-current on whichever
        // new thread picks up rendering duty even though `ensure_init`'s
        // own one-time setup must never run again.
        if !self.ensure_init() {
            return;
        }

        let width  = frame.width as usize;
        let height = frame.height as usize;
        if width == 0 || height == 0 { return; }

        let state = self.state.as_mut().unwrap();
        let gl    = &state.gl;
        let bar_w = width.min(crate::disp::FB_STRIDE);

        // Handle window resize — take the latest queued size. Taken out of the
        // lock first: written as `if let Some(..) = self.window_size.lock().take()`
        // the guard lives through the else branches (edition 2021), and
        // `inner_size()` there waits on the main thread, which takes this same
        // lock for every Resized event -- a deadlock at start-up.
        let queued = self.window_size.lock().take();
        let (win_w, win_h) = if let Some((w, h)) = queued {
            state.surface.resize(
                &state.context,
                NonZeroU32::new(w).unwrap(),
                NonZeroU32::new(h).unwrap(),
            );
            (w as usize, h as usize)
        } else if self.current_win_w > 0 {
            (self.current_win_w, self.current_win_h)
        } else {
            // First frame before any resize event: query actual window size.
            let s = self.window.inner_size();
            (s.width as usize, s.height as usize)
        };

        let win_w_f = win_w as f32;
        let win_h_f = win_h as f32;
        let win_w_i = win_w as i32;
        let win_h_i = win_h as i32;

        // Letterbox: status bar scales with display.
        let (qx0, qy0, qx1, qy1, scale_f) = Self::letterbox(
            width as f32, height as f32, win_w_f, win_h_f);
        let sb_h_px = STATUS_BAR_HEIGHT as f32 * scale_f;
        let sb_h_i  = sb_h_px as i32;

        // Use integer texelFetch shader only at exactly 1x or 2x, and only when we
        // actually have a Core32 context — Legacy tier has no texelFetch shader at all.
        let use_integer = self.gl_tier == GlTier::Core32 && (scale_f == 1.0 || scale_f == 2.0);
        let scale_i = scale_f.round() as i32;

        // Recompute quads when anything changes.
        if width != self.current_w || height != self.current_h
            || win_w != self.current_win_w || win_h != self.current_win_h
        {
            self.current_w     = width;
            self.current_h     = height;
            self.current_win_w = win_w;
            self.current_win_h = win_h;
            // Publish the display resolution for the event thread's aspect lock.
            *self.display_res.lock() = (width as u32, height as u32);
            // UV coords into the words texture (2048×FB_MAX_H unless wider)
            let max_u      = width  as f32 / self.tex_w as f32;
            let max_v_main = height as f32 / crate::disp::FB_MAX_H as f32;
            unsafe {
                // Main display quad (letterboxed).
                Self::upload_quad(gl, state.main_vbo,
                    qx0, qy0, qx1, qy1,
                    0.0, 0.0, max_u, max_v_main);
                // Status bar under the display, same x extent; a frame wider than
                // the 2048-wide bar texture gets it over its left 2048 pixels.
                let (qxb, max_ub) = if bar_w == width {
                    (qx1, width as f32 / 2048.0)
                } else {
                    (qx0 + (qx1 - qx0) * bar_w as f32 / width as f32, bar_w as f32 / 2048.0)
                };
                Self::upload_quad(gl, state.status_vbo,
                    qx0, qy1, qxb, qy1 + sb_h_px,
                    0.0, 0.0, max_ub, 1.0);
            }
        }

        // quad_y_bottom for integer shader: how many GL pixels from bottom to bottom of main quad.
        let main_quad_y_bottom = (win_h_f - qy1) as i32;

        unsafe {
            // ── The picture first: composing sets its own FBO/viewport ───────
            let new_frame = frame.seq != self.seen_seq || self.main_tex.is_none();
            if new_frame {
                let planes = frame.planes.as_ref().and_then(|p| p.downcast_ref::<crate::dev::ng1::screen::Ng1Screen>());
                self.main_tex = match (planes, &mut self.compositor) {
                    // Newport's planes, composed here in the shader.
                    (Some(planes), Some(compositor)) => {
                        let planes = planes.0.lock();
                        Some(compositor.compose(&planes.compositor_source(), gl))
                    }
                    _ if !frame.words.is_empty() => {
                        Some(upload_words(gl, &mut self.words_tex, &frame.words, width, height, self.tex_w))
                    }
                    _ => None,
                };
                self.seen_seq = frame.seq;
            }
            let Some(main_tex) = self.main_tex else { return };

            // Restore full-window viewport/scissor — compositor FBO left them dirty.
            gl.viewport(0, 0, win_w_i, win_h_i);
            gl.enable(glow::SCISSOR_TEST);
            gl.scissor(0, 0, win_w_i, win_h_i);
            gl.clear_color(0.0, 0.0, 0.0, 1.0);
            gl.clear(glow::COLOR_BUFFER_BIT);

            // ── Pass 1: main display ─────────────────────────────────────────
            gl.scissor(0, 0, win_w_i, win_h_i);
            if use_integer {
                Self::draw_tex_integer(gl, state, state.main_vbo, main_tex,
                    width as i32, height as i32, main_quad_y_bottom, scale_i,
                    win_w_f, win_h_f);
            } else {
                Self::draw_tex_fallback(gl, state, state.main_vbo, main_tex,
                    win_w_f, win_h_f);
            }

            // ── Pass 2: debug overlay (alpha-blended) ────────────────────────
            if let Some(overlay) = frame.overlay.as_deref() {
                gl.enable(glow::BLEND);
                gl.blend_func(glow::SRC_ALPHA, glow::ONE_MINUS_SRC_ALPHA);
                let ov_tex = upload_words(gl, &mut self.overlay_tex, overlay, width, height, self.tex_w);
                if use_integer {
                    Self::draw_tex_integer(gl, state, state.main_vbo, ov_tex,
                        width as i32, height as i32, main_quad_y_bottom, scale_i,
                        win_w_f, win_h_f);
                } else {
                    Self::draw_tex_fallback(gl, state, state.main_vbo, ov_tex,
                        win_w_f, win_h_f);
                }
                gl.disable(glow::BLEND);
            }

            // ── Pass 3: status bar ───────────────────────────────────────────
            gl.scissor(0, 0, win_w_i, win_h_i);
            let sb_tex = self.sbtex.render_and_upload(&mut self.status, stats, bar_w, gl);
            Self::draw_tex_fallback(gl, state, state.status_vbo, sb_tex, win_w_f, win_h_f);

            gl.disable(glow::SCISSOR_TEST);

            state.surface.swap_buffers(&state.context).unwrap();
        }
    }

    fn resize(&mut self, width: usize, height: usize) {
        // Publish the new resolution *before* the snap below, so the event
        // thread's aspect lock sees it when it handles the resulting Resized
        // event (otherwise it would re-fit the window to the stale aspect).
        *self.display_res.lock() = (width as u32, height as u32);
        // On display resolution change, snap the window to the configured
        // scale of the new resolution — but only ever *ask*. This runs on
        // REX3's refresh thread, and touching the window from here is what
        // used to kill iris on Windows; see `ResizeRequest` for the full
        // mechanism. `Scale` sizes from `display_res`, published just above.
        *self.resize_request.lock() = Some(ResizeRequest::Scale(self.scale));
        let _ = self.event_proxy.send_event(());
    }

    fn stop(&mut self) {
        if let Some(state) = self.state.take() {
            if let Some(compositor) = &mut self.compositor {
                compositor.destroy(&state.gl);
            }
            self.sbtex.destroy(&state.gl);
            for tex in [self.words_tex.take(), self.overlay_tex.take()].into_iter().flatten() {
                unsafe { state.gl.delete_texture(tex) };
            }
            self.main_tex = None;
            self.seen_seq = 0;
            // Give the context back so the next ensure_init (after a stop/start,
            // as jitcheck does repeatedly) sets up again.
            match state.context.make_not_current() {
                Ok(not_current) => self.not_current_context = Some(not_current),
                Err(e) => eprintln!("iris: failed to release GL context on stop(), the next start makes a new one: {e}"),
            }
        }
        self.current_w     = 0;
        self.current_h     = 0;
        self.current_win_w = 0;
        self.current_win_h = 0;
    }

    fn takes_planes(&self) -> bool {
        self.compositor.is_some()
    }
}

/// Upload `words` (rows `width` apart) to the top left of a `tex_w` by
/// `FB_MAX_H` texture made on first use.
fn upload_words(gl: &glow::Context, tex: &mut Option<glow::Texture>, words: &[u32], width: usize, height: usize, tex_w: usize) -> glow::Texture {
    let (w, h) = (width.min(tex_w), height.min(crate::disp::FB_MAX_H));
    unsafe {
        let t = *tex.get_or_insert_with(|| {
            let t = gl.create_texture().unwrap();
            gl.bind_texture(glow::TEXTURE_2D, Some(t));
            gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MIN_FILTER, glow::NEAREST as i32);
            gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MAG_FILTER, glow::NEAREST as i32);
            gl.tex_image_2d(glow::TEXTURE_2D, 0, glow::RGBA as i32,
                tex_w as i32, crate::disp::FB_MAX_H as i32, 0, glow::RGBA, glow::UNSIGNED_BYTE, glow::PixelUnpackData::Slice(None));
            t
        });
        gl.bind_texture(glow::TEXTURE_2D, Some(t));
        gl.pixel_store_i32(glow::UNPACK_ROW_LENGTH, width as i32);
        let bytes = std::slice::from_raw_parts(words.as_ptr() as *const u8, words.len() * 4);
        gl.tex_sub_image_2d(
            glow::TEXTURE_2D, 0, 0, 0, w as i32, h as i32,
            glow::RGBA, glow::UNSIGNED_BYTE,
            glow::PixelUnpackData::Slice(Some(bytes)),
        );
        gl.pixel_store_i32(glow::UNPACK_ROW_LENGTH, 0);
        t
    }
}

/// The window drawn through wgpu (`renderer = "wgpu"`), on the board's device
/// when it has one, else its own.
#[cfg(feature = "wgpu")]
struct WgpuRenderer {
    presenter: crate::wgpu::Presenter,
    window_size: Arc<Mutex<Option<(u32, u32)>>>,
    resize_request: Arc<Mutex<Option<ResizeRequest>>>,
    event_proxy: EventLoopProxy<()>,
    scale: u32,
    display_res: Arc<Mutex<(u32, u32)>>,
    status: StatusBar,
    sbtex: StatusBarTexture,
    /// The status bar's rows cut to the frame's width.
    bar: Vec<u32>,
}

#[cfg(feature = "wgpu")]
/// A failed present is a GPU failure: it stops the machine, like the boards'.
#[cfg(feature = "wgpu")]
fn report_present(gpu: &crate::wgpu::Gpu, r: Result<(), String>) {
    if let Err(e) = r {
        gpu.fail(&format!("present: {e}"));
    }
}

#[cfg(feature = "wgpu")]
impl WgpuRenderer {
    /// The window's size and the frame's, and the status bar for `width`:
    /// what every presentation needs first.
    fn prepare(&mut self, width: usize, height: usize, stats: &BarStats) {
        if let Some((w, h)) = self.window_size.lock().take() {
            self.presenter.resize(w, h);
        }
        *self.display_res.lock() = (width as u32, height as u32);
        // The bar is at most FB_STRIDE wide; a wider frame repeats its last column.
        let bar_w = width.min(crate::disp::FB_STRIDE);
        let rows = self.sbtex.render_pixels(&mut self.status, stats, bar_w);
        self.bar.clear();
        for y in 0..STATUS_BAR_HEIGHT {
            let row = &rows[y * crate::disp::FB_STRIDE..][..bar_w];
            self.bar.extend_from_slice(row);
            self.bar.resize((y + 1) * width, row[bar_w - 1]);
        }
    }
}

#[cfg(feature = "wgpu")]
impl Renderer for WgpuRenderer {
    fn present(&mut self, frame: &Frame, stats: &BarStats) {
        let (width, height) = (frame.width as usize, frame.height as usize);
        if width == 0 || height == 0 {
            return;
        }
        self.prepare(width, height, stats);
        let shown = match &frame.gpu {
            Some(buffer) => crate::wgpu::Frame::Gpu { buffer, width: frame.width, height: frame.height },
            None if !frame.words.is_empty() => {
                crate::wgpu::Frame::Cpu { rgba: &frame.words, stride: width, width: frame.width, height: frame.height }
            }
            None => return,
        };
        let r = self.presenter.present(shown, frame.overlay.as_deref(), &self.bar, width);
        report_present(self.presenter.gpu(), r);
    }

    fn resize(&mut self, width: usize, height: usize) {
        // As the GL renderer: publish the resolution, ask the event thread
        // to snap the window (never touch the window from here).
        *self.display_res.lock() = (width as u32, height as u32);
        *self.resize_request.lock() = Some(ResizeRequest::Scale(self.scale));
        let _ = self.event_proxy.send_event(());
    }

    // No stop(): a wgpu device is not bound to a thread, so a hand-over moves
    // the presenter as it is.
    fn gpu(&self) -> Option<&Arc<crate::wgpu::Gpu>> {
        Some(self.presenter.gpu())
    }

    #[cfg(test)]
    fn present_resident_gpu_frame(&mut self, frame: &Frame, stats: &BarStats) -> Result<(), String> {
        let Some(buffer) = &frame.gpu else {
            return Err("the frame is not on the device".into());
        };
        // The debug overlay's pixel inspector expects CPU plane snapshots,
        // which are intentionally stale in resident mode.
        if frame.overlay.is_some() {
            return Err("resident debug overlays require a raw-plane observation".into());
        }
        let width = frame.width as usize;
        self.prepare(width, frame.height as usize, stats);
        self.presenter.present(
            crate::wgpu::Frame::Gpu { buffer, width: frame.width, height: frame.height },
            None,
            &self.bar,
            width,
        )
    }
}

struct MouseDelta {
    accum: (f64, f64),
    wheel: f64,
    buttons: u8,
}

/// One host window: the winit window and what passes between its event thread
/// and its renderer.
struct Pane {
    window: Arc<Window>,
    window_size: Arc<Mutex<Option<(u32, u32)>>>,
    resize_request: Arc<Mutex<Option<ResizeRequest>>>,
    display_res: Arc<Mutex<(u32, u32)>>,
    /// Last accepted window size: tells which edge is being dragged when locking the aspect ratio.
    last_win_size: (u32, u32),
    /// The machine screen this window shows; with `cycle` or side by side
    /// the pane is not tied to one, and this is the first.
    screen: usize,
}

impl Pane {
    fn new(window: Arc<Window>, window_size: Arc<Mutex<Option<(u32, u32)>>>, resize_request: Arc<Mutex<Option<ResizeRequest>>>, display_res: Arc<Mutex<(u32, u32)>>, screen: usize) -> Self {
        let s = window.inner_size();
        Self { window, window_size, resize_request, display_res, last_win_size: (s.width, s.height), screen }
    }
}

/// UI Manager handling Window, OpenGL context, and Input
pub struct Ui {
    ps2: Arc<Ps2Controller>,
    /// Every screen of the machine, in `screen N` order.
    screens: Vec<Arc<Screen>>,
    discs: Option<Arc<dyn DiscChanger>>,
    /// One window, except per-screen windows: one each.
    panes: Vec<Pane>,
    /// The hotkey's walk through the screens, for `cycle` on several.
    cycle: Option<Cycle>,
    timer_manager: Arc<TimerManager>,
    initial_scale: u32,
    scroll_pixels_per_line: f64,
    lock_aspect_ratio: bool,
}

impl Ui {
    /// The window(s) for `screens`, shown as `windows` says; one screen always
    /// gets one window. `machine` receives the window GPU's failures.
    pub fn new(ps2: Arc<Ps2Controller>, screens: Vec<Arc<Screen>>, discs: Option<Arc<dyn DiscChanger>>, timer_manager: Arc<TimerManager>, event_loop: &EventLoop<()>, scale: u32, scroll_pixels_per_line: f64, lock_aspect_ratio: bool, renderer: crate::config::UiRenderer, windows: ScreenWindows, machine: crate::workstation::MachineSlot) -> Self {
        assert!(!screens.is_empty(), "a window needs a screen");
        let fatal = Self::window_fatal(machine);
        let n = screens.len();
        let mode = if n > 1 { windows } else { ScreenWindows::Cycle };
        let named = n > 1 && mode != ScreenWindows::SideBySide;
        for (i, s) in screens.iter().enumerate() {
            s.window.shown.store(i, std::sync::atomic::Ordering::Relaxed);
            s.window.of.store(if named { n } else { 0 }, std::sync::atomic::Ordering::Relaxed);
        }
        let windows = if mode == ScreenWindows::PerScreen { n } else { 1 };
        let mut panes = Vec::with_capacity(windows);
        let mut renderers = Vec::with_capacity(windows);
        for i in 0..windows {
            let title = if named { format!("{} - screen {}/{n}", crate::machine::emulator_name(), i + 1) } else { crate::machine::emulator_name() };
            let (pane, r) = Self::open_window(&screens[i], i, &title, event_loop, scale, renderer, mode == ScreenWindows::SideBySide, &fatal);
            panes.push(pane);
            renderers.push(r);
        }
        match mode {
            ScreenWindows::Cycle => screens[0].window.install(renderers.pop().expect("one window")),
            ScreenWindows::PerScreen => {
                for (s, r) in screens.iter().zip(renderers) {
                    s.window.install(r);
                }
            }
            ScreenWindows::SideBySide => {
                let first = renderers.pop().expect("one window");
                for (s, r) in screens.iter().zip(SideBySide::for_screens(&screens, first)) {
                    s.window.install(r);
                }
            }
        }
        let cycle = if mode == ScreenWindows::Cycle { Cycle::new(&screens) } else { None };
        Self { ps2, screens, discs, panes, cycle, timer_manager, initial_scale: scale, scroll_pixels_per_line, lock_aspect_ratio }
    }

    /// What the window's own GPU does when it fails: stop the machine, as a
    /// board's device does.
    #[cfg(feature = "wgpu")]
    fn window_fatal(machine: crate::workstation::MachineSlot) -> Arc<dyn Fn(&str) + Send + Sync> {
        let (tx, rx) = std::sync::mpsc::channel();
        crate::workstation::register_fatal_controller(machine, Some(rx));
        Arc::new(move |why| {
            let _ = tx.send(crate::traits::MachineFatal::new("window GPU", why));
        })
    }

    #[cfg(not(feature = "wgpu"))]
    fn window_fatal(_machine: crate::workstation::MachineSlot) -> Arc<dyn Fn(&str) + Send + Sync> {
        Arc::new(|_| {})
    }

    /// A window for screen `index` and its renderer (not yet installed): wgpu
    /// when asked and the host can, else GL. `wide` fits side-by-side's picture.
    #[allow(clippy::too_many_arguments)]
    fn open_window(screen: &Screen, index: usize, title: &str, event_loop: &EventLoop<()>, scale: u32, renderer: crate::config::UiRenderer, wide: bool, fatal: &Arc<dyn Fn(&str) + Send + Sync>) -> (Pane, Box<dyn Renderer>) {
        let tex_w = if wide { 2 * crate::disp::FB_STRIDE } else { crate::disp::FB_STRIDE };
        // The Indy's default video mode is 1280×1024; open the window at that
        // size (plus the status bar). The renderer snaps to the real resolution
        // via resize() once the PROM/IRIX programs its actual mode, keeping `scale`.
        let w = 1280 * scale;
        let h = (1024 + STATUS_BAR_HEIGHT as u32) * scale;
        let window_builder = WindowAttributes::default()
            .with_title(title)
            .with_resizable(true)
            .with_inner_size(winit::dpi::PhysicalSize::new(w, h));

        #[cfg(feature = "wgpu")]
        if renderer == crate::config::UiRenderer::Wgpu {
            // Only a host that cannot run wgpu falls back, and says so; a device
            // or present failure after this point stops the machine.
            match Self::new_wgpu(screen, event_loop, window_builder.clone(), scale, tex_w, fatal) {
                Ok((window, window_size, resize_request, display_res, renderer)) => {
                    return (Pane::new(window, window_size, resize_request, display_res, index), renderer);
                }
                Err(e) => eprintln!("iris: renderer = \"wgpu\" is not possible on this host ({e}); drawing with GL"),
            }
        }
        #[cfg(not(feature = "wgpu"))]
        let _ = (renderer, fatal);

        let template = ConfigTemplateBuilder::new()
            .with_alpha_size(8)
            .with_transparency(false);

        // Only the wgpu build on Linux changes it below.
        #[cfg_attr(not(all(feature = "wgpu", target_os = "linux")), allow(unused_mut))]
        let mut display_builder = DisplayBuilder::new().with_window_attributes(Some(window_builder));
        // A board that draws on the host GPU (IMPACT with `raster = "wgpu"`)
        // shares its wgpu device with this window, and the thread that runs
        // this renderer is the board's refresh thread — the same one that
        // submits the board's batches to wgpu.
        //
        // libglvnd tags a thread's current GL context with the window system
        // that made it current, and its `eglMakeCurrent` refuses to make an
        // EGL context current while a GLX one is: EGL_BAD_ACCESS, "Another
        // window API already has a current context". glutin defaults to
        // GLX-first on X11, so the window's context went current as GLX and
        // wgpu-hal's `AdapterContext::lock()` — which unwraps that error —
        // panicked on the MGRAS-Refresh thread. Ask for EGL instead: it is the
        // window system wgpu-hal's GLES backend uses, so the two contexts
        // simply take turns being current on the one thread, as EGL allows.
        //
        // Linux only: this is a GLVND tag conflict, and elsewhere glutin's
        // preference is ignored (CGL on macOS) or there is no shared dispatch
        // layer (WGL on Windows).
        #[cfg(all(feature = "wgpu", target_os = "linux"))]
        if screen.gpu.get().is_some() {
            display_builder = display_builder.with_preference(ApiPreference::PreferEgl);
        }

        let (window, gl_config) = display_builder
            .build(event_loop, template, |configs| {
                configs
                    .reduce(|accum, config| {
                        if config.num_samples() > accum.num_samples() { config } else { accum }
                    })
                    .unwrap()
            })
            .unwrap();

        let window = Arc::new(window.unwrap());

        // Create the GL context here on the main thread, i.e. the same thread
        // that created the window/display above. The refresh thread only
        // makes it current later (in GlRenderer::ensure_init); it never calls
        // create_context itself. See the not_current_context field comment.
        let raw_window_handle = window.window_handle().expect("no raw window handle").as_raw();
        let gl_display = gl_config.display();

        // Try, in order: (1) explicit GL 3.2 core — what GlCompositor and the
        // texelFetch/integer shaders need; (2) explicit GL 2.1 compatibility —
        // enough for SwCompositor + the legacy (#version 120) copy shader;
        // (3) whatever the driver considers default, as a last resort. Some
        // drivers (seen on NVIDIA proprietary) reject an unversioned/ambiguous
        // context request with BadValue but accept an explicit version, so this
        // also guards against the original bug independent of the tier we land on.
        let attempts: &[(GlTier, ContextApi, Option<GlProfile>)] = &[
            (GlTier::Core32, ContextApi::OpenGl(Some(Version::new(3, 2))), Some(GlProfile::Core)),
            (GlTier::Legacy, ContextApi::OpenGl(Some(Version::new(2, 1))), Some(GlProfile::Compatibility)),
            (GlTier::Legacy, ContextApi::OpenGl(None), None),
        ];

        let mut result = None;
        for (tier, api, profile) in attempts {
            let mut builder = ContextAttributesBuilder::new().with_context_api(*api);
            if let Some(p) = profile {
                builder = builder.with_profile(*p);
            }
            let context_attributes = builder.build(Some(raw_window_handle));
            match unsafe { gl_display.create_context(&gl_config, &context_attributes) } {
                Ok(ctx) => { result = Some((*tier, ctx, context_attributes)); break; }
                Err(e) => eprintln!("iris: GL context attempt {:?} failed: {e}", tier),
            }
        }
        let (gl_tier, not_current_context, context_attributes) = result
            .expect("failed to create a GL context under any requested version/profile");
        eprintln!("iris: using GL tier {:?}", gl_tier);

        // Build the window surface and bind the context to it *once, here* — on
        // the main thread that owns the window, before `Ui::run` starts pumping
        // messages. This is the one GL step that must not happen lazily on the
        // refresh thread: `create_window_surface` sets the pixel format (which
        // makes the Windows GL ICD subclass the window) and the first
        // `make_current` populates the driver's per-HWND state. Doing that while
        // the event loop is already delivering a startup resize left the AMD ICD
        // dereferencing a null pointer inside its wndproc hook (issue #94).
        // Rendering stays entirely on the refresh thread — only this bind moves.
        let (not_current_context, initial_surface) = {
            let sz = window.inner_size();
            let attrs = SurfaceAttributesBuilder::<WindowSurface>::new().build(
                raw_window_handle,
                NonZeroU32::new(sz.width.max(1)).unwrap(),
                NonZeroU32::new(sz.height.max(1)).unwrap(),
            );
            let surface = unsafe {
                gl_display
                    .create_window_surface(&gl_config, &attrs)
                    .expect("failed to create the GL window surface on the main thread")
            };
            let current = not_current_context
                .make_current(&surface)
                .expect("failed to make the GL context current during init");
            // Hand the context straight back to `NotCurrentContext` so the
            // refresh thread can `make_current` it on its own thread (a GL
            // context is current on at most one thread at a time).
            let not_current = current
                .make_not_current()
                .expect("failed to release the GL context after init");
            (not_current, surface)
        };

        let window_size    = Arc::new(Mutex::new(None));
        let resize_request = Arc::new(Mutex::new(None));
        // Seed with the Indy's default 1280×1024; the render thread republishes
        // the real resolution on the first frame and on any mode change.
        let display_res = Arc::new(Mutex::new((1280u32, 1024u32)));

        // GlCompositor needs GL 3.2 core (usampler2D/texelFetch); a legacy
        // context takes words, which Newport then composes on the CPU.
        let compositor = (gl_tier == GlTier::Core32).then(GlCompositor::new);

        let renderer = GlRenderer {
            window:      window.clone(),
            gl_config,
            not_current_context: Some(not_current_context),
            context_attributes,
            initial_surface: Some(initial_surface),
            raw_window_handle,
            gl_tier,
            window_size: window_size.clone(),
            resize_request: resize_request.clone(),
            event_proxy: event_loop.create_proxy(),
            scale,
            display_res: display_res.clone(),
            state:       None,
            compositor,
            status: StatusBar::new(),
            sbtex: StatusBarTexture::new(),
            words_tex: None,
            overlay_tex: None,
            main_tex: None,
            seen_seq: 0,
            current_w:     0,
            current_h:     0,
            current_win_w: 0,
            current_win_h: 0,
            tex_w,
        };

        (Pane::new(window, window_size, resize_request, display_res, index), Box::new(renderer))
    }

    /// The window and its wgpu renderer, installed in the board's renderer
    /// slot. The window is a plain winit one (GL's needs a GL-capable visual
    /// from glutin, this one does not); the device is the board's when it
    /// draws on one, so a frame it composes there stays on it.
    #[cfg(feature = "wgpu")]
    #[allow(clippy::type_complexity)]
    fn new_wgpu(
        screen: &Screen,
        event_loop: &EventLoop<()>,
        attrs: WindowAttributes,
        scale: u32,
        tex_w: usize,
        fatal: &Arc<dyn Fn(&str) + Send + Sync>,
    ) -> Result<(Arc<Window>, Arc<Mutex<Option<(u32, u32)>>>, Arc<Mutex<Option<ResizeRequest>>>, Arc<Mutex<(u32, u32)>>, Box<dyn Renderer>), String> {
        let gpu = match screen.gpu.get().cloned() {
            Some(g) => g,
            None => {
                let fatal = fatal.clone();
                crate::wgpu::Gpu::new(&crate::wgpu::GpuOptions::default(), Box::new(move |e| fatal(e)))?
            }
        };
        #[allow(deprecated)]
        let window = Arc::new(event_loop.create_window(attrs).expect("create the window"));
        let size = window.inner_size();
        let presenter = crate::wgpu::Presenter::new(
            &gpu,
            window.clone(),
            (size.width, size.height),
            (tex_w as u32, crate::disp::FB_MAX_H as u32),
            (tex_w as u32, STATUS_BAR_HEIGHT as u32),
        )?;
        eprintln!("iris: drawing the window with wgpu on {}", gpu.caps.adapter);
        let window_size = Arc::new(Mutex::new(None));
        let resize_request = Arc::new(Mutex::new(None));
        let display_res = Arc::new(Mutex::new((1280u32, 1024u32)));
        let renderer = WgpuRenderer {
            presenter,
            window_size: window_size.clone(),
            resize_request: resize_request.clone(),
            event_proxy: event_loop.create_proxy(),
            scale,
            display_res: display_res.clone(),
            status: StatusBar::new(),
            sbtex: StatusBarTexture::new(),
            bar: Vec::new(),
        };
        Ok((window, window_size, resize_request, display_res, Box::new(renderer)))
    }

    /// Run the UI event loop (blocks the current thread)
    pub fn run(self, event_loop: EventLoop<()>) {
        let Ui { ps2, screens, discs, panes, cycle, timer_manager, initial_scale, scroll_pixels_per_line, lock_aspect_ratio } = self;
        let scale = initial_scale;
        let mouse_delta = Arc::new(Mutex::new(MouseDelta { accum: (0.0, 0.0), wheel: 0.0, buttons: 0 }));

        {
            let ps2   = ps2.clone();
            let delta = mouse_delta.clone();
            timer_manager.add_recurring(
                Instant::now() + Duration::from_millis(10),
                Duration::from_millis(10),
                (ps2, delta),
                |(ps2, delta)| {
                    Self::flush_mouse_delta(ps2, delta, true);
                    TimerReturn::Continue
                },
            );
        }

        event_loop.set_control_flow(ControlFlow::Wait);
        let mut app = UiApp {
            ps2, screens, discs, panes, cycle, mouse_delta,
            scale, scroll_pixels_per_line, lock_aspect_ratio,
            mouse_grabbed: false, rctrl_held: false, held_keys: Vec::new(),
        };
        event_loop.run_app(&mut app).unwrap();
    }

    fn flush_mouse_delta(ps2: &Ps2Controller, mouse_delta: &Mutex<MouseDelta>, require_motion: bool) {
        let mut md = mouse_delta.lock();
        let dx = md.accum.0 as i32;
        let dy = md.accum.1 as i32;
        let dz = md.wheel as i32;
        if require_motion && dx == 0 && dy == 0 && dz == 0 { return; }
        md.accum.0 -= dx as f64;
        md.accum.1 -= dy as f64;
        md.wheel   -= dz as f64;
        let buttons = md.buttons;
        drop(md);
        ps2.push_mouse_input(buttons, dx, dy, dz);
    }

    /// Adjust an incoming window size to match the emulated display's aspect
    /// ratio (display width : display height + status bar). Whichever axis the
    /// user is actively dragging — the one that moved most from `prev` — is
    /// kept, and the other is derived from it. Returns `None` when the size is
    /// already within 1 px of the target (no correction needed), which keeps
    /// the follow-up resize from oscillating.
    fn aspect_fit(win_w: u32, win_h: u32, prev: (u32, u32), disp_w: u32, disp_h: u32)
        -> Option<(u32, u32)>
    {
        if disp_w == 0 || disp_h == 0 { return None; }
        let content_h = disp_h + STATUS_BAR_HEIGHT as u32;
        // round(a * b / c) in u64 to avoid overflow/bias.
        let muldiv = |a: u32, b: u32, c: u32| -> u32 {
            ((a as u64 * b as u64 + c as u64 / 2) / c as u64) as u32
        };
        let (pw, ph) = prev;
        if win_w.abs_diff(pw) >= win_h.abs_diff(ph) {
            // Width is the driven axis: derive height from it.
            let target_h = muldiv(win_w, content_h, disp_w).max(1);
            if target_h.abs_diff(win_h) <= 1 { None } else { Some((win_w, target_h)) }
        } else {
            // Height is the driven axis: derive width from it.
            let target_w = muldiv(win_h, disp_w, content_h).max(1);
            if target_w.abs_diff(win_w) <= 1 { None } else { Some((target_w, win_h)) }
        }
    }

    /// `shot` is the screen for the screenshot hotkey; `cycle` the next-screen
    /// hotkey's state.
    #[allow(clippy::too_many_arguments)]
    fn handle_keyboard(ps2: &Ps2Controller, shot: &Arc<Screen>, discs: &Option<Arc<dyn DiscChanger>>,
        resize_request: &Mutex<Option<ResizeRequest>>,
        input: KeyEvent, grabbed: &mut bool, rctrl_held: &mut bool, held_keys: &mut Vec<KeyCode>,
        window: &Window, cycle: &mut Option<Cycle>)
    {
        use std::sync::atomic::Ordering;
        if let PhysicalKey::Code(keycode) = input.physical_key {
            let pressed = input.state == ElementState::Pressed;

            if keycode == KeyCode::ControlRight {
                *rctrl_held = pressed;
                if pressed && !input.repeat && *grabbed {
                    *grabbed = false;
                    let _ = window.set_cursor_grab(winit::window::CursorGrabMode::None);
                    window.set_cursor_visible(true);
                }
                return;
            }

            // Mac keyboards (every MacBook, the Magic Keyboard) have no Right
            // Ctrl, so on macOS Right Cmd releases the grab as well. It is not
            // passed to the guest.
            #[cfg(target_os = "macos")]
            if keycode == KeyCode::SuperRight {
                if pressed && !input.repeat && *grabbed {
                    *grabbed = false;
                    let _ = window.set_cursor_grab(winit::window::CursorGrabMode::None);
                    window.set_cursor_visible(true);
                }
                return;
            }

            if keycode == KeyCode::PrintScreen && pressed && !input.repeat && *rctrl_held {
                crate::gfx_display::save_screenshot(shot);
                return;
            }

            if keycode == KeyCode::F11 && pressed && !input.repeat && *rctrl_held {
                // Queued, not applied here: see `ResizeRequest`. Resolving
                // fullscreen-vs-windowed is deferred to apply time too, so the
                // toggle reads the state it will actually act on.
                *resize_request.lock() = Some(ResizeRequest::ToggleFullscreen);
                return;
            }

            // RCtrl+F12: hot-swap CD-ROM disc (open file picker and load into first CD-ROM device)
            if keycode == KeyCode::F12 && pressed && !input.repeat && *rctrl_held {
                // Find the first CD-ROM device (disc_status only lists CD-ROMs)
                let cdrom = discs.as_ref()
                    .and_then(|d| d.disc_status().first().map(|(id, ..)| (*id, d.clone())));

                if let Some((id, discs)) = cdrom {
                    // Run the picker on its own thread rather than inline.
                    //
                    // A native file dialog pumps its **own modal message loop**,
                    // and on Windows that re-enters our window procedure while
                    // winit is still inside this event callback — the same
                    // re-entrancy that makes off-thread resizing fatal (see
                    // `ResizeRequest`), since winit's Windows `EventLoopRunner`
                    // keeps its handler and buffer in unsynchronised
                    // `Cell`/`RefCell`. Meanwhile the REX3 thread keeps
                    // presenting frames and queueing resize requests into that
                    // same runner.
                    //
                    // Off-thread it can neither re-enter the event loop nor
                    // stall emulation while the dialog sits open.
                    // `DiscChanger::load_disc` takes `&self` and locks
                    // internally, so calling it from here is safe.
                    std::thread::Builder::new()
                        .name("cdrom-picker".to_string())
                        .spawn(move || {
                            let Some(path) = rfd::FileDialog::new()
                                .set_title("Load CD-ROM disc")
                                .add_filter("ISO images", &["iso", "chd"])
                                .add_filter("All", &["*"])
                                .pick_file()
                            else { return };

                            let path_str = path.to_string_lossy().into_owned();
                            match discs.load_disc(id, path_str.clone()) {
                                Ok(_) => {
                                    let filename = path.file_name()
                                        .map(|n| n.to_string_lossy().into_owned())
                                        .unwrap_or_else(|| path_str.clone());
                                    eprintln!("SCSI #{}: loaded {}", id, filename);
                                }
                                Err(e) => eprintln!("SCSI #{}: {}", id, e),
                            }
                        })
                        .expect("failed to spawn cdrom-picker thread");
                } else {
                    eprintln!("No CD-ROM drive attached");
                }
                return;
            }

            // RCtrl+Tab shows the next screen; with one screen it goes to the guest.
            if keycode == KeyCode::Tab && pressed && !input.repeat && *rctrl_held {
                if let Some(cycle) = cycle {
                    if cycle.step().is_some() {
                        if let Some((n, of)) = cycle.current().window.label() {
                            window.set_title(&format!("{} - screen {n}/{of}", crate::machine::emulator_name()));
                        }
                    }
                    return;
                }
            }

            // RCtrl+1 / RCtrl+2: snap window to 1x or 2x scale.
            if pressed && !input.repeat && *rctrl_held {
                let snap = match keycode {
                    KeyCode::Digit1 => Some(1u32),
                    KeyCode::Digit2 => Some(2u32),
                    _ => None,
                };
                if let Some(s) = snap {
                    // Recorded rather than applied inline: we are already
                    // inside a winit event callback, and on Windows resizing
                    // from in here re-enters the wndproc. `about_to_wait`
                    // applies it once we're back out of the callback.
                    *resize_request.lock() = Some(ResizeRequest::Scale(s));
                    return;
                }
            }

            // Remember what the guest holds, so a focus loss can let go of it.
            if pressed {
                if !held_keys.contains(&keycode) { held_keys.push(keycode); }
            } else {
                held_keys.retain(|k| *k != keycode);
            }
            ps2.push_kb(keycode, pressed);
        }
    }

    /// The window lost focus: lift every key and mouse button the guest still
    /// sees held. Their releases go to whichever window has focus now (Cmd-Tab
    /// or Alt-Tab is pressed here and released elsewhere), so without this the
    /// guest keeps them down — a stuck Shift, say, which IRIX then applies to
    /// every click and key.
    fn release_held_input(ps2: &Ps2Controller, mouse_delta: &Mutex<MouseDelta>, held_keys: &mut Vec<KeyCode>) {
        for k in held_keys.drain(..) {
            ps2.push_kb(k, false);
        }
        let had_buttons = {
            let mut md = mouse_delta.lock();
            let had = md.buttons != 0;
            md.buttons = 0;
            had
        };
        if had_buttons {
            Ui::flush_mouse_delta(ps2, mouse_delta, false);
        }
    }
}

/// `Ui::run`'s event-loop state: `run_app` takes the old closure's captures as a struct.
struct UiApp {
    ps2:         Arc<Ps2Controller>,
    screens:     Vec<Arc<Screen>>,
    discs:       Option<Arc<dyn DiscChanger>>,
    panes:       Vec<Pane>,
    cycle:       Option<Cycle>,
    mouse_delta: Arc<Mutex<MouseDelta>>,
    scale: u32,
    scroll_pixels_per_line: f64,
    lock_aspect_ratio: bool,
    mouse_grabbed: bool,
    rctrl_held: bool,
    // Keys sent to the guest as pressed and not yet released.
    held_keys: Vec<KeyCode>,
}

impl UiApp {
    /// The screen window `pane` shows now: the stepped-to one with `cycle`,
    /// the pane's own otherwise.
    fn shown(&self, pane: usize) -> &Arc<Screen> {
        match &self.cycle {
            Some(c) => c.current(),
            None => &self.screens[self.panes[pane].screen],
        }
    }
}

impl ApplicationHandler for UiApp {
    // Windows and GL contexts are created up front in Ui::new — nothing to (re)create here.
    fn resumed(&mut self, _event_loop: &ActiveEventLoop) {}

    fn window_event(&mut self, event_loop: &ActiveEventLoop, id: WindowId, event: WindowEvent) {
        let Some(p) = self.panes.iter().position(|p| p.window.id() == id) else { return };
        match event {
            WindowEvent::CloseRequested => { event_loop.exit() },
            WindowEvent::Resized(size) => {
                let pane = &mut self.panes[p];
                if size.width != 0 && size.height != 0 {
                    // Lock the window to the display's aspect ratio so the
                    // picture fills it without letterbox bars. Skipped when
                    // fullscreen or maximized (aspect can't be honoured there)
                    // and when disabled by config.
                    //
                    // The correction is queued, not applied here: we are inside
                    // an event callback, and resizing from within one re-enters
                    // the wndproc on Windows. `about_to_wait` applies it a
                    // moment later, which also coalesces a burst of Resized
                    // events (a mouse drag) into a single correction.
                    if self.lock_aspect_ratio
                        && pane.window.fullscreen().is_none()
                        && !pane.window.is_maximized()
                    {
                        let (dw, dh) = *pane.display_res.lock();
                        if let Some((w, h)) = Ui::aspect_fit(
                            size.width, size.height, pane.last_win_size, dw, dh)
                        {
                            *pane.resize_request.lock() =
                                Some(ResizeRequest::Fit { w, h });
                        }
                    }
                    let new_size = (size.width, size.height);
                    pane.last_win_size = new_size;
                    *pane.window_size.lock() = Some(new_size);
                }
            }
            WindowEvent::KeyboardInput { event, .. } => {
                let shot = self.shown(p).clone();
                let pane = &self.panes[p];
                Ui::handle_keyboard(&self.ps2, &shot, &self.discs, &pane.resize_request, event,
                    &mut self.mouse_grabbed, &mut self.rctrl_held, &mut self.held_keys, &pane.window, &mut self.cycle);
            }
            WindowEvent::MouseInput { state, button, .. } => {
                let window = &self.panes[p].window;
                if self.mouse_grabbed {
                    let pressed = state == ElementState::Pressed;
                    let mask = match button {
                        MouseButton::Left   => 1,
                        MouseButton::Right  => 2,
                        MouseButton::Middle => 4,
                        _ => 0,
                    };
                    if mask != 0 {
                        let mut md = self.mouse_delta.lock();
                        if pressed { md.buttons |= mask; } else { md.buttons &= !mask; }
                        drop(md);
                        Ui::flush_mouse_delta(&self.ps2, &self.mouse_delta, false);
                    }
                } else if state == ElementState::Pressed && button == MouseButton::Left {
                    self.mouse_grabbed = true;
                    if window.set_cursor_grab(winit::window::CursorGrabMode::Locked).is_err() {
                        let _ = window.set_cursor_grab(winit::window::CursorGrabMode::Confined);
                    }
                    window.set_cursor_visible(false);
                    self.mouse_delta.lock().accum = (0.0, 0.0);
                }
            }
            WindowEvent::MouseWheel { delta, .. } => {
                if self.mouse_grabbed {
                    let lines = match delta {
                        winit::event::MouseScrollDelta::LineDelta(_, y) => y as f64,
                        winit::event::MouseScrollDelta::PixelDelta(p)  => p.y / self.scroll_pixels_per_line,
                    };
                    self.mouse_delta.lock().wheel += lines;
                }
            }
            WindowEvent::Focused(false) => {
                Ui::release_held_input(&self.ps2, &self.mouse_delta, &mut self.held_keys);
                self.rctrl_held = false;
                if self.mouse_grabbed {
                    self.mouse_grabbed = false;
                    let window = &self.panes[p].window;
                    let _ = window.set_cursor_grab(winit::window::CursorGrabMode::None);
                    window.set_cursor_visible(true);
                }
            }
            WindowEvent::RedrawRequested => {
                // Rendering is driven by the board's display thread
            }
            _ => (),
        }
    }

    // Wakeup from the render thread's `event_proxy.send_event(())`. The
    // payload carries no information — the request itself is in
    // `resize_request`, and `about_to_wait` (which always runs after this)
    // is what applies it.
    fn user_event(&mut self, _event_loop: &ActiveEventLoop, _event: ()) {}

    // The one place window-resizing actually happens. Running it here rather
    // than inside a `window_event` arm matters: winit has finished dispatching
    // the current batch of OS messages, so `request_inner_size` — which on
    // Windows synchronously re-enters the wndproc with WM_SIZE — cannot land
    // in the middle of another event callback. Combined with never calling it
    // off-thread at all (see `ResizeRequest`), that removes both halves of the
    // re-entrancy hazard.
    fn about_to_wait(&mut self, _event_loop: &ActiveEventLoop) {
        for pane in &mut self.panes {
            Self::apply_resize(pane);
        }
    }

    fn device_event(&mut self, _event_loop: &ActiveEventLoop, _id: DeviceId, event: DeviceEvent) {
        if let DeviceEvent::MouseMotion { delta } = event {
            if self.mouse_grabbed {
                let mut md = self.mouse_delta.lock();
                md.accum.0 += delta.0 / self.scale as f64;
                md.accum.1 += delta.1 / self.scale as f64;
            }
        }
    }
}

impl UiApp {
    /// Apply the resize `pane`'s renderer or hotkeys asked for, if any.
    fn apply_resize(pane: &mut Pane) {
        let Some(req) = pane.resize_request.lock().take() else { return };

        // Fullscreen is a mode change, not a resize, so it is handled before
        // the "don't resize while fullscreen" guard below — that guard would
        // otherwise swallow every request to *leave* fullscreen. The window
        // manager sends a Resized event of its own afterwards, which
        // re-establishes window_size/last_win_size, so nothing to publish here.
        if req == ResizeRequest::ToggleFullscreen {
            let new_mode = if pane.window.fullscreen().is_some() {
                None
            } else {
                Some(winit::window::Fullscreen::Borderless(None))
            };
            pane.window.set_fullscreen(new_mode);
            return;
        }

        // A resize is meaningless while fullscreen or maximized — the window
        // manager owns the size — and applying one would fight it.
        if pane.window.fullscreen().is_some() || pane.window.is_maximized() {
            return;
        }

        let (w, h) = match req {
            ResizeRequest::Fit { w, h } => (w, h),
            ResizeRequest::Scale(s) => {
                let (dw, dh) = *pane.display_res.lock();
                if dw == 0 || dh == 0 { return; }
                (dw * s, (dh + STATUS_BAR_HEIGHT as u32) * s)
            }
            // Handled above; listed so a new variant is a compile error here.
            ResizeRequest::ToggleFullscreen => return,
        };
        if w == 0 || h == 0 { return; }

        let size = winit::dpi::PhysicalSize::new(w, h);
        if let Some(actual) = pane.window.request_inner_size(size) {
            // Applied synchronously: no Resized event will follow, so publish
            // the new size ourselves or the renderer keeps the stale one.
            if actual.width != 0 && actual.height != 0 {
                pane.last_win_size = (actual.width, actual.height);
                *pane.window_size.lock() = Some((actual.width, actual.height));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Indy default mode. Content aspect = 1280 : (1024 + 16) = 1280 : 1040.
    const DW: u32 = 1280;
    const DH: u32 = 1024;

    /// A present that fails is the device's failure: the machine is told.
    #[cfg(feature = "wgpu")]
    #[test]
    fn a_failed_present_is_fatal_not_a_log_line() {
        use std::sync::atomic::{AtomicU32, Ordering::SeqCst};
        let told = Arc::new(AtomicU32::new(0));
        let n = told.clone();
        let gpu = match crate::wgpu::Gpu::new(&crate::wgpu::GpuOptions::default(), Box::new(move |_| { n.fetch_add(1, SeqCst); })) {
            Ok(g) => g,
            Err(e) => {
                eprintln!("skipped: {e}");
                return;
            }
        };
        report_present(&gpu, Ok(()));
        assert_eq!(told.load(SeqCst), 0);
        report_present(&gpu, Err("surface lost".into()));
        report_present(&gpu, Err("lost again".into()));
        assert_eq!(told.load(SeqCst), 1, "the machine is told once, however many presents fail");
    }

    #[test]
    fn dragging_width_derives_height() {
        // Width grows from 1280 to 2560; height untouched. Expect height locked
        // to the content ratio: 2560 * 1040 / 1280 = 2080.
        let fixed = Ui::aspect_fit(2560, 1040, (1280, 1040), DW, DH);
        assert_eq!(fixed, Some((2560, 2080)));
    }

    #[test]
    fn dragging_height_derives_width() {
        // Height grows from 1040 to 2080; width untouched. Expect width locked:
        // 2080 * 1280 / 1040 = 2560.
        let fixed = Ui::aspect_fit(1280, 2080, (1280, 1040), DW, DH);
        assert_eq!(fixed, Some((2560, 2080)));
    }

    #[test]
    fn already_locked_is_noop() {
        // A size already on-ratio needs no correction (prevents oscillation on
        // the follow-up Resized event after we apply a fix).
        assert_eq!(Ui::aspect_fit(2560, 2080, (2560, 2080), DW, DH), None);
        assert_eq!(Ui::aspect_fit(1280, 1040, (1280, 1040), DW, DH), None);
    }

    #[test]
    fn within_one_pixel_tolerance() {
        // 1 px off the exact ratio is accepted as-is (no visible letterbox).
        assert_eq!(Ui::aspect_fit(2560, 2079, (2560, 2079), DW, DH), None);
        assert_eq!(Ui::aspect_fit(2560, 2081, (2560, 2081), DW, DH), None);
    }

    #[test]
    fn zero_resolution_is_noop() {
        // Guard against a divide-by-zero before the first frame publishes a res.
        assert_eq!(Ui::aspect_fit(800, 600, (800, 600), 0, 0), None);
    }
}
