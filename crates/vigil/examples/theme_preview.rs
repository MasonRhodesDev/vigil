use vigil_core::OutputId;
use vigil_theme::Theme;
use vigil_ui::{OutputWindow, VigilPlatform};

const W: u32 = 1280;
const H: u32 = 800;

fn main() {
    let mut args = std::env::args().skip(1);
    let out = args.next().unwrap_or_else(|| "theme-preview.png".into());
    let theme_path = args.next().map(std::path::PathBuf::from);

    let transform: u8 = std::env::var("PREVIEW_TRANSFORM")
        .ok()
        .and_then(|x| x.parse().ok())
        .unwrap_or(0);

    let platform = VigilPlatform::install().expect("platform");
    let theme = Theme::load_or_default(theme_path.as_deref());
    let component = theme.instantiate().expect("instantiate");
    let adapter = platform.claim_last_adapter().expect("adapter");
    let mut window =
        OutputWindow::with_transform(OutputId(1), W, H, 1.0, transform, adapter, component)
            .expect("window");
    let (pw, ph) = (W, H);

    window.on_ui_message(std::rc::Rc::new(|m| vigil_log::out!("ui-message: {m:?}")));
    window.set_panel_visible(true);
    window.set_cursor_visible(true);
    window.set_clock("13:37");
    if std::env::var_os("PREVIEW_LIGHT").is_some() {
        window.apply_kit_tokens(&slint_kit::embedded_fallback("light"));
    } else {
        vigil_ui::apply_kit_tokens_from_disk(&mut window, "dark");
    }
    window.set_users(&["mason".into(), "Other…".into()]);
    window.set_user_index(0);
    window.set_user_name("mason");
    window.set_sessions(&["Other DE".into(), "Test DE".into()]);
    window.set_session_index(0);
    use vigil_core::AuthUi;
    window.show_prompt("Password:", true);

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

    window.dispatch(vigil_core::InputEvent::PointerAbsolute { x: 98.0, y: 754.0 });
    window.dispatch(vigil_core::InputEvent::PointerButton {
        button: 0x110,
        pressed: true,
    });
    window.dispatch(vigil_core::InputEvent::PointerButton {
        button: 0x110,
        pressed: false,
    });

    window.dispatch(vigil_core::InputEvent::PointerAbsolute {
        x: 1118.0,
        y: 754.0,
    });
    window.dispatch(vigil_core::InputEvent::PointerButton {
        button: 0x110,
        pressed: true,
    });
    window.dispatch(vigil_core::InputEvent::PointerButton {
        button: 0x110,
        pressed: false,
    });

    vigil_ui::advance_timers();
    let mut buf = vec![0u8; (pw * ph * 4) as usize];
    let drew = window.render_if_needed(vigil_core::FrameTarget {
        buffer: &mut buf,
        width: pw,
        height: ph,
        stride: (pw * 4) as usize,
        damage: None,
    });
    vigil_log::out!("drew: {drew}");

    let mut rgb = Vec::with_capacity((W * H * 3) as usize);
    for px in buf.as_chunks::<4>().0 {
        rgb.extend_from_slice(&[px[2], px[1], px[0]]);
    }
    image::save_buffer(&out, &rgb, pw, ph, image::ColorType::Rgb8).expect("png");
    vigil_log::out!("wrote {out}");
}
