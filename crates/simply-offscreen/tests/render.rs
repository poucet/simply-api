//! Renders on the machine's real wgpu backend. On a runner with none (a
//! Linux CI box with no GPU or software Vulkan/GL), each test prints why
//! and passes: compile and clippy still cover the code there.

use egui::{Color32, Rect, Sense, Vec2};
use simply_offscreen::{render_png, render_png_posed, render_png_with, Harness, UNAVAILABLE};

const RED: Color32 = Color32::from_rgb(0xe0, 0x20, 0x30);

/// The decoded image, or `None` (with a note) when there is no backend.
fn decode(result: Result<simply_offscreen::OffscreenPng, String>) -> Option<image::RgbaImage> {
    let png = match result {
        Ok(png) => png,
        Err(e) if e.starts_with(UNAVAILABLE) => {
            eprintln!("skipped, no wgpu backend: {e}");
            return None;
        }
        Err(e) => panic!("render failed: {e}"),
    };
    let image = image::load_from_memory_with_format(&png.bytes, image::ImageFormat::Png)
        .expect("valid PNG")
        .to_rgba8();
    assert_eq!(image.dimensions(), (png.width, png.height));
    Some(image)
}

fn pixel(image: &image::RgbaImage, x: u32, y: u32) -> [u8; 4] {
    image.get_pixel(x, y).0
}

/// A filled rect in the top-left 40×40 pt, and a label below it.
fn rect_and_label(ui: &mut egui::Ui) {
    let rect = Rect::from_min_size(ui.max_rect().min, Vec2::splat(40.0));
    ui.painter().rect_filled(rect, 0.0, RED);
    ui.add_space(44.0);
    ui.label("simply-offscreen");
}

#[test]
fn a_frame_renders_to_a_png_of_the_requested_size() {
    let Some(image) = decode(render_png(rect_and_label, 120.0, 80.0, 2.0)) else {
        return;
    };
    assert_eq!(image.dimensions(), (240, 160));
    // Inside the rect (pt 10,10 → px 20,20) is the fill colour…
    assert_eq!(pixel(&image, 20, 20), [0xe0, 0x20, 0x30, 0xff]);
    // …and outside it is not.
    assert_ne!(pixel(&image, 200, 20), [0xe0, 0x20, 0x30, 0xff]);
}

#[test]
fn setup_styles_the_context_before_the_first_frame() {
    let ink = Color32::from_rgb(0x10, 0x80, 0x40);
    let setup = move |ctx: &egui::Context| {
        // As an app's theme does: pin the theme, then set its visuals.
        ctx.set_theme(egui::ThemePreference::Dark);
        let mut visuals = egui::Visuals::dark();
        visuals.panel_fill = ink;
        ctx.set_visuals(visuals);
    };
    let frame = |ui: &mut egui::Ui| {
        // The frame reads the style: if it ever drew before `setup`, or
        // `setup` never ran, this is egui's default panel colour.
        let fill = ui.visuals().panel_fill;
        ui.painter().rect_filled(ui.max_rect(), 0.0, fill);
        ui.label("themed");
    };
    let Some(image) = decode(render_png_with(setup, frame, 60.0, 40.0, 1.0, |_| {})) else {
        return;
    };
    assert_eq!(pixel(&image, 45, 12), [0x10, 0x80, 0x40, 0xff]);
}

#[test]
fn a_pose_captures_the_frame_a_drag_is_in() {
    // A 20 pt square that follows the drag: before the pose it is at the
    // root Ui's origin (8, 8): kittest's root Ui sits inside an 8 pt margin;
    // the capture shows it about 60 pt to the right. Sample the square's
    // lower left, clear of its edges and of the pointer at (70, 10).
    let mut offset = Vec2::ZERO;
    let frame = move |ui: &mut egui::Ui| {
        let response = ui.allocate_rect(ui.max_rect(), Sense::drag());
        offset += response.drag_delta();
        let square = Rect::from_min_size(ui.max_rect().min + offset, Vec2::splat(20.0));
        ui.painter().rect_filled(square, 0.0, RED);
    };
    let pose = |harness: &mut Harness<'_>| {
        let at = |x: f32| egui::pos2(x, 10.0);
        harness.event(egui::Event::PointerMoved(at(10.0)));
        harness.event(egui::Event::PointerButton {
            pos: at(10.0),
            button: egui::PointerButton::Primary,
            pressed: true,
            modifiers: egui::Modifiers::NONE,
        });
        harness.step();
        for x in [20.0, 40.0, 70.0] {
            harness.event(egui::Event::PointerMoved(at(x)));
            harness.step();
        }
    };
    let Some(image) = decode(render_png_posed(frame, 100.0, 40.0, 1.0, pose)) else {
        return;
    };
    assert_ne!(pixel(&image, 12, 12), [0xe0, 0x20, 0x30, 0xff], "square left the origin");
    assert_eq!(pixel(&image, 72, 24), [0xe0, 0x20, 0x30, 0xff], "square is where the drag is");
}
