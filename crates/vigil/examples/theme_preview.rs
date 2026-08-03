//! Dev harness: render a theme headlessly into a PNG — no DRM, no VM.
//! Also the seed of the golden-image test matrix (DESIGN.md §8).
//!
//!   cargo run -p vigil --example theme_preview -- out.png [theme.slint]

use vigil_core::OutputId;
use vigil_theme::Theme;
use vigil_ui::{OutputWindow, VigilPlatform};

const W: u32 = 1280;
const H: u32 = 800;

fn main() {
    let mut args = std::env::args().skip(1);
    let out = args.next().unwrap_or_else(|| "theme-preview.png".into());
    let theme_path = args.next().map(std::path::PathBuf::from);

    let platform = VigilPlatform::install().expect("platform");
    let theme = Theme::load_or_default(theme_path.as_deref());
    let component = theme.instantiate().expect("instantiate");
    let adapter = platform.claim_last_adapter().expect("adapter");
    let mut window = OutputWindow::new(OutputId(1), W, H, 1.0, adapter, component).expect("window");

    window.on_ui_message(std::rc::Rc::new(|m| println!("ui-message: {m:?}")));
    window.set_panel_visible(true);
    window.set_clock("13:37");
    use vigil_core::AuthUi;
    window.show_prompt("Password:", true);

    // Type a character to verify the input field has keyboard focus.
    window.dispatch(vigil_core::InputEvent::Key {
        keysym: 97,
        utf8: Some("a".into()),
        pressed: true,
    });
    window.dispatch(vigil_core::InputEvent::Key {
        keysym: 97,
        utf8: Some("a".into()),
        pressed: false,
    });

    window.dispatch(vigil_core::InputEvent::Key {
        keysym: 0xff0d,
        utf8: Some("\r".into()),
        pressed: true,
    });

    vigil_ui::advance_timers();
    let mut buf = vec![0u8; (W * H * 4) as usize];
    let drew = window.render_if_needed(vigil_core::FrameTarget {
        buffer: &mut buf,
        width: W,
        height: H,
        stride: (W * 4) as usize,
    });
    println!("drew: {drew}");

    // XRGB -> RGB for the PNG
    let mut rgb = Vec::with_capacity((W * H * 3) as usize);
    for px in buf.chunks_exact(4) {
        rgb.extend_from_slice(&[px[2], px[1], px[0]]);
    }
    image::save_buffer(&out, &rgb, W, H, image::ColorType::Rgb8).expect("png");
    println!("wrote {out}");
}
