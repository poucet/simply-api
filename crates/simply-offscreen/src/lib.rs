//! Offscreen (headless) rendering of an egui frame: run it in an
//! `egui_kittest` wgpu harness until it settles, and return it as encoded
//! PNG bytes. It backs snapshot tests and screenshot tools alike, and knows
//! nothing about what is drawn: the caller passes the frame, and optionally
//! a `setup` that themes the context before the first one.
//!
//! Needs a working wgpu backend (Metal/Vulkan/GL) at runtime. On a machine
//! without one the harness panics deep inside wgpu, so every render is
//! wrapped in `catch_unwind` and returns an `Err` starting with
//! [`UNAVAILABLE`] instead of taking the caller down.
//!
//! ```no_run
//! let png = simply_offscreen::render_png(
//!     |ui| {
//!         ui.label("hello");
//!     },
//!     200.0,
//!     100.0,
//!     2.0,
//! )?;
//! assert_eq!((png.width, png.height), (400, 200));
//! # Ok::<(), String>(())
//! ```
//!
//! Provenance: the generic part of flux-editor's `shell/offscreen.rs` at
//! flux `bae34fcf` (change `okwotpou`). flux's own theme is replaced by the
//! `setup` hook of [`render_png_with`].

pub use egui_kittest::Harness;

/// Frames stepped after the setup pass, so egui areas and layout fully
/// settle before capture.
pub const SETTLE_FRAMES: usize = 3;

/// How every error for a missing wgpu backend (or any other panic inside
/// the harness) starts. A caller that can run without pixels, such as a
/// test on a CI runner with no GPU, matches on it to skip.
pub const UNAVAILABLE: &str = "offscreen rendering unavailable";

/// An encoded offscreen render.
#[derive(Debug, Clone)]
pub struct OffscreenPng {
    /// The PNG file bytes.
    pub bytes: Vec<u8>,
    /// Image width in pixels (`width_pt * pixels_per_point`).
    pub width: u32,
    /// Image height in pixels.
    pub height: u32,
}

/// Render one settled frame of `frame` offscreen as a PNG, with egui's
/// default style.
///
/// `frame` draws into the harness' root `Ui` every frame.
/// `width_pt`/`height_pt` are the canvas size in egui points;
/// `pixels_per_point` scales the output image (2.0 renders at retina
/// density).
pub fn render_png(
    frame: impl FnMut(&mut egui::Ui),
    width_pt: f32,
    height_pt: f32,
    pixels_per_point: f32,
) -> Result<OffscreenPng, String> {
    render_png_posed(frame, width_pt, height_pt, pixels_per_point, |_| {})
}

/// [`render_png`] with a **pose**: after the frame has settled and before it
/// is captured, `pose` drives the harness — a pointer pressed and moved, a
/// key held — and the capture is the frame the gesture is *in*. This is how
/// a still frame shows a drag: a gesture only exists while egui is fed the
/// pointer, and only the harness can feed it.
pub fn render_png_posed(
    frame: impl FnMut(&mut egui::Ui),
    width_pt: f32,
    height_pt: f32,
    pixels_per_point: f32,
    pose: impl FnOnce(&mut Harness<'_>),
) -> Result<OffscreenPng, String> {
    render_png_with(|_| {}, frame, width_pt, height_pt, pixels_per_point, pose)
}

/// The general form: [`render_png_posed`] with a `setup` that runs once on
/// the context before the first frame is drawn — install fonts, set
/// visuals or a theme, as an eframe app creator does.
///
/// `setup` runs inside the harness' first pass, and that pass draws
/// nothing: fonts set during a pass only take effect on the next one.
pub fn render_png_with(
    setup: impl FnOnce(&egui::Context),
    frame: impl FnMut(&mut egui::Ui),
    width_pt: f32,
    height_pt: f32,
    pixels_per_point: f32,
    pose: impl FnOnce(&mut Harness<'_>),
) -> Result<OffscreenPng, String> {
    // wgpu panics (rather than erroring) when no adapter/backend exists;
    // catch it so headless callers get a readable failure.
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
        render_inner(setup, frame, width_pt, height_pt, pixels_per_point, pose)
    }))
    .unwrap_or_else(|panic| Err(format!("{UNAVAILABLE} (wgpu: {})", panic_message(&*panic))))
}

fn render_inner(
    setup: impl FnOnce(&egui::Context),
    mut frame: impl FnMut(&mut egui::Ui),
    width_pt: f32,
    height_pt: f32,
    pixels_per_point: f32,
    pose: impl FnOnce(&mut Harness<'_>),
) -> Result<OffscreenPng, String> {
    let mut setup = Some(setup);
    let mut harness = Harness::builder()
        .with_size(egui::Vec2::new(width_pt, height_pt))
        .with_pixels_per_point(pixels_per_point)
        // A real frame: egui *times* its gestures (a press held ~600 ms
        // becomes a drag), so a pose fed at kittest's default quarter-second
        // steps would read as something else.
        .with_step_dt(1.0 / 60.0)
        // The machine this runs on, not kittest's default of Linux: an app
        // spells a key the way the OS does (`⌘S` on a mac, `Ctrl+S`
        // elsewhere). A pose for another OS sets it itself.
        .with_os(egui::os::OperatingSystem::from_target_os())
        // kittest's `PREDICTABLE` renderer filters textures in software,
        // and that path clamps every texel coordinate to the texture — it
        // ignores the sampler's wrap mode, so a repeating texture drawn with
        // UVs past 0..1 smears its edge texel. Hardware filtering here, so a
        // render is evidence of what the app draws; dithering stays off and
        // MSAA stays at one, so the rest of the frame is as predictable.
        .with_render_options(egui_wgpu::RendererOptions {
            predictable_texture_filtering: false,
            ..egui_wgpu::RendererOptions::PREDICTABLE
        })
        .wgpu()
        .build_ui(move |ui| match setup.take() {
            Some(setup) => setup(ui.ctx()),
            None => frame(ui),
        });
    // +1 for the setup pass, which drew nothing.
    harness.run_steps(SETTLE_FRAMES + 1);
    pose(&mut harness);
    let image = harness.render().map_err(|e| format!("wgpu render failed: {e}"))?;
    let (width, height) = image.dimensions();
    let mut bytes = Vec::new();
    image
        .write_to(&mut std::io::Cursor::new(&mut bytes), image::ImageFormat::Png)
        .map_err(|e| format!("png encoding failed: {e}"))?;
    Ok(OffscreenPng { bytes, width, height })
}

fn panic_message(panic: &(dyn std::any::Any + Send)) -> &str {
    panic
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| panic.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("unknown panic")
}
