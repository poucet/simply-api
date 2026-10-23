//! A machine with no wgpu backend gets an `Err`, not a panic. Its own test
//! binary, because the backend is chosen through the process environment.

#[test]
fn no_backend_is_an_err_not_a_panic() {
    // A backend that exists on no runner this crate is tested on (macOS and
    // Linux): wgpu then finds no adapter and egui_kittest panics.
    std::env::set_var("WGPU_BACKEND", "dx12");
    let result = simply_offscreen::render_png(|ui| drop(ui.label("x")), 10.0, 10.0, 1.0);
    let err = result.expect_err("no adapter, no image");
    assert!(err.starts_with(simply_offscreen::UNAVAILABLE), "{err}");
}
