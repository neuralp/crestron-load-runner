//! The frosted backdrop behind a modal dialog.
//!
//! egui cannot read back what it has already drawn, so a real blur has to go
//! through the renderer: a paint callback halves the rendered frame into a
//! chain of framebuffers and stretches the smallest one back over the window.
//! Every failure path here simply draws nothing, leaving the plain dim that is
//! painted over the blur in any case — which is also what headless tests and
//! any non-glow renderer get.

use std::sync::{Arc, Mutex};

use eframe::{
    egui, egui_glow,
    glow::{self, HasContext as _},
};

/// How many times the frame is halved before being stretched back. Three steps
/// blur across roughly eight pixels while costing three hardware blits.
const STEPS: usize = 3;

/// Darkening painted over the blur, so dialog text keeps its contrast.
pub const DIM: egui::Color32 = egui::Color32::from_black_alpha(90);

#[derive(Clone, Default)]
pub struct Backdrop {
    /// Renderer objects live as long as the window and are shared with the
    /// paint callback, which the renderer may call from its own thread.
    resources: Arc<Mutex<Option<Resources>>>,
}

impl Backdrop {
    /// Frosts everything already drawn in `ctx`. Call once per frame, before
    /// the modals themselves, which must then use a transparent backdrop of
    /// their own.
    pub fn paint(&self, ctx: &egui::Context) {
        let rect = ctx.content_rect();
        // A layer that is not an area is drawn above every area of the same
        // order, so this covers the panels and windows but stays under the
        // modal, which egui puts in the foreground.
        let painter = ctx.layer_painter(egui::LayerId::new(
            egui::Order::Middle,
            egui::Id::new("modal_backdrop"),
        ));
        let resources = self.resources.clone();
        painter.add(egui::Shape::Callback(egui::PaintCallback {
            rect,
            callback: Arc::new(egui_glow::CallbackFn::new(move |info, painter| {
                let size = [info.screen_size_px[0] as i32, info.screen_size_px[1] as i32];
                let Ok(mut slot) = resources.lock() else {
                    return;
                };
                let gl = painter.gl();
                if slot.as_ref().is_none_or(|held| held.size != size) {
                    if let Some(old) = slot.take() {
                        old.release(gl);
                    }
                    *slot = Resources::new(gl, size);
                }
                if let Some(resources) = slot.as_ref() {
                    resources.blur(gl, painter.intermediate_fbo());
                }
            })),
        }));
        painter.rect_filled(rect, 0.0, DIM);
    }
}

struct Resources {
    program: glow::Program,
    vertex_array: glow::VertexArray,
    /// Successively smaller copies of the frame, largest first.
    steps: Vec<(glow::Texture, glow::Framebuffer, i32, i32)>,
    size: [i32; 2],
}

impl Resources {
    fn new(gl: &glow::Context, size: [i32; 2]) -> Option<Self> {
        if size[0] < 16 || size[1] < 16 {
            return None;
        }
        // Reading a multisampled frame into a smaller buffer is not a legal
        // blit, and there is no cheap way around it.
        if unsafe { gl.get_parameter_i32(glow::SAMPLES) } > 1 {
            return None;
        }
        let mut steps = Vec::with_capacity(STEPS);
        let (mut width, mut height) = (size[0], size[1]);
        for _ in 0..STEPS {
            width = (width / 2).max(1);
            height = (height / 2).max(1);
            let (texture, framebuffer) = unsafe { attachment(gl, width, height) }?;
            steps.push((texture, framebuffer, width, height));
        }
        let program = unsafe { program(gl) }?;
        let vertex_array = unsafe { gl.create_vertex_array() }.ok()?;
        Some(Self {
            program,
            vertex_array,
            steps,
            size,
        })
    }

    fn blur(&self, gl: &glow::Context, target: Option<glow::Framebuffer>) {
        unsafe {
            let (mut width, mut height) = (self.size[0], self.size[1]);
            let mut source = target;
            for &(_, framebuffer, step_width, step_height) in &self.steps {
                gl.bind_framebuffer(glow::READ_FRAMEBUFFER, source);
                gl.bind_framebuffer(glow::DRAW_FRAMEBUFFER, Some(framebuffer));
                gl.blit_framebuffer(
                    0,
                    0,
                    width,
                    height,
                    0,
                    0,
                    step_width,
                    step_height,
                    glow::COLOR_BUFFER_BIT,
                    glow::LINEAR,
                );
                source = Some(framebuffer);
                width = step_width;
                height = step_height;
            }
            let Some(&(texture, ..)) = self.steps.last() else {
                return;
            };
            // Back to the frame egui is drawing, where the scissor rectangle it
            // set keeps this inside the dialog's clip rectangle.
            gl.bind_framebuffer(glow::FRAMEBUFFER, target);
            gl.viewport(0, 0, self.size[0], self.size[1]);
            gl.disable(glow::BLEND);
            gl.use_program(Some(self.program));
            gl.active_texture(glow::TEXTURE0);
            gl.bind_texture(glow::TEXTURE_2D, Some(texture));
            if let Some(sampler) = gl.get_uniform_location(self.program, "u_frame") {
                gl.uniform_1_i32(Some(&sampler), 0);
            }
            gl.bind_vertex_array(Some(self.vertex_array));
            gl.draw_arrays(glow::TRIANGLES, 0, 3);
            gl.bind_vertex_array(None);
            gl.enable(glow::BLEND);
        }
    }

    fn release(self, gl: &glow::Context) {
        unsafe {
            for (texture, framebuffer, ..) in self.steps {
                gl.delete_framebuffer(framebuffer);
                gl.delete_texture(texture);
            }
            gl.delete_vertex_array(self.vertex_array);
            gl.delete_program(self.program);
        }
    }
}

unsafe fn attachment(
    gl: &glow::Context,
    width: i32,
    height: i32,
) -> Option<(glow::Texture, glow::Framebuffer)> {
    unsafe {
        let texture = gl.create_texture().ok()?;
        gl.bind_texture(glow::TEXTURE_2D, Some(texture));
        gl.tex_image_2d(
            glow::TEXTURE_2D,
            0,
            glow::RGBA8 as i32,
            width,
            height,
            0,
            glow::RGBA,
            glow::UNSIGNED_BYTE,
            glow::PixelUnpackData::Slice(None),
        );
        for (parameter, value) in [
            (glow::TEXTURE_MIN_FILTER, glow::LINEAR),
            (glow::TEXTURE_MAG_FILTER, glow::LINEAR),
            (glow::TEXTURE_WRAP_S, glow::CLAMP_TO_EDGE),
            (glow::TEXTURE_WRAP_T, glow::CLAMP_TO_EDGE),
        ] {
            gl.tex_parameter_i32(glow::TEXTURE_2D, parameter, value as i32);
        }
        let framebuffer = gl.create_framebuffer().ok()?;
        gl.bind_framebuffer(glow::FRAMEBUFFER, Some(framebuffer));
        gl.framebuffer_texture_2d(
            glow::FRAMEBUFFER,
            glow::COLOR_ATTACHMENT0,
            glow::TEXTURE_2D,
            Some(texture),
            0,
        );
        let complete = gl.check_framebuffer_status(glow::FRAMEBUFFER) == glow::FRAMEBUFFER_COMPLETE;
        gl.bind_framebuffer(glow::FRAMEBUFFER, None);
        gl.bind_texture(glow::TEXTURE_2D, None);
        complete.then_some((texture, framebuffer))
    }
}

/// A vertex shader that needs no buffers: one oversized triangle covers the
/// frame, and its position doubles as the texture coordinate.
unsafe fn program(gl: &glow::Context) -> Option<glow::Program> {
    let header = if gl.version().is_embedded {
        "#version 300 es\nprecision mediump float;\n"
    } else {
        "#version 330\n"
    };
    let sources = [
        (
            glow::VERTEX_SHADER,
            "out vec2 v_uv;
void main() {
    v_uv = vec2(float((gl_VertexID << 1) & 2), float(gl_VertexID & 2));
    gl_Position = vec4(v_uv * 2.0 - 1.0, 0.0, 1.0);
}",
        ),
        (
            glow::FRAGMENT_SHADER,
            "in vec2 v_uv;
uniform sampler2D u_frame;
out vec4 f_color;
void main() { f_color = texture(u_frame, v_uv); }",
        ),
    ];
    unsafe {
        let program = gl.create_program().ok()?;
        let mut shaders = Vec::with_capacity(sources.len());
        for (stage, source) in sources {
            let Ok(shader) = gl.create_shader(stage) else {
                gl.delete_program(program);
                return None;
            };
            gl.shader_source(shader, &format!("{header}{source}"));
            gl.compile_shader(shader);
            if !gl.get_shader_compile_status(shader) {
                log_failure(&gl.get_shader_info_log(shader));
                gl.delete_shader(shader);
                for shader in shaders {
                    gl.delete_shader(shader);
                }
                gl.delete_program(program);
                return None;
            }
            gl.attach_shader(program, shader);
            shaders.push(shader);
        }
        gl.link_program(program);
        let linked = gl.get_program_link_status(program);
        if !linked {
            log_failure(&gl.get_program_info_log(program));
        }
        for shader in shaders {
            gl.detach_shader(program, shader);
            gl.delete_shader(shader);
        }
        if linked {
            Some(program)
        } else {
            gl.delete_program(program);
            None
        }
    }
}

fn log_failure(message: &str) {
    if cfg!(debug_assertions) {
        eprintln!("Modal backdrop blur unavailable: {message}");
    }
}
