//! Render-state matrix for the default theme (DESIGN.md §8).
//!
//! Deliberately NOT pixel-perfect golden images: text rasterization differs
//! across host font stacks (Fedora dev box vs CI runners), so exact PNGs
//! would be flaky. Instead each auth state is rendered headlessly and
//! checked with region-level assertions that survive font differences.
//! True golden comparison becomes possible once a font is embedded (M3).
//!
//! Everything runs in ONE test: the Slint platform is process-global and
//! its windows are not Send, so parallel #[test] functions cannot share it.

use vigil_core::{AuthUi, FrameTarget, InputEvent, OutputId};
use vigil_theme::Theme;
use vigil_ui::{OutputWindow, VigilPlatform};

const W: u32 = 1280;
const H: u32 = 800;
const BG: (u8, u8, u8) = (0x0f, 0x15, 0x1c);

struct Scene {
    window: OutputWindow,
    buf: Vec<u8>,
}

impl Scene {
    fn render(&mut self) -> bool {
        vigil_ui::advance_timers();
        self.window.render_if_needed(FrameTarget {
            buffer: &mut self.buf,
            width: W,
            height: H,
            stride: (W * 4) as usize,
        })
    }

    fn px(&self, x: u32, y: u32) -> (u8, u8, u8) {
        let i = ((y * W + x) * 4) as usize;
        // XRGB8888 little-endian: B, G, R, X
        (self.buf[i + 2], self.buf[i + 1], self.buf[i])
    }

    /// Any sampled pixel in the rect satisfying `pred`.
    fn any_in(&self, rect: (u32, u32, u32, u32), pred: impl Fn((u8, u8, u8)) -> bool) -> bool {
        let (x0, y0, x1, y1) = rect;
        (y0..y1)
            .step_by(4)
            .any(|y| (x0..x1).step_by(4).any(|x| pred(self.px(x, y))))
    }

    fn uniform_bg(&self, rect: (u32, u32, u32, u32)) -> bool {
        !self.any_in(rect, |p| p != BG)
    }
}

// Card geometry at 1280x800: 400x240 centered -> x 440..840, y 280..520.
const CARD: (u32, u32, u32, u32) = (445, 285, 835, 515);
// Input field interior row.
const FIELD_Y: u32 = 368;

#[test]
fn default_theme_state_matrix() {
    let platform = VigilPlatform::install().expect("platform");
    let theme = Theme::load_or_default(None);
    let component = theme.instantiate().expect("instantiate");
    let adapter = platform.claim_last_adapter().expect("adapter");
    let window = OutputWindow::new(OutputId(1), W, H, 1.0, adapter, component).expect("window");
    let mut scene = Scene {
        window,
        buf: vec![0u8; (W * H * 4) as usize],
    };

    // --- idle, panel visible: card, field, and clock render ---
    scene.window.set_panel_visible(true);
    scene.window.set_clock("12:34");
    scene.window.show_prompt("Password:", true);
    assert!(scene.render(), "initial state must draw");
    assert!(
        scene.any_in(CARD, |p| p != BG),
        "login card must render when panel-visible"
    );
    assert!(
        scene.any_in((400, 20, 1270, 90), |p| p != BG),
        "clock must render top-right"
    );

    // --- typed secret input shows bullets in the field ---
    let sample = |s: &Scene| -> Vec<(u8, u8, u8)> {
        (475..805).step_by(2).map(|x| s.px(x, FIELD_Y)).collect()
    };
    let field_before = sample(&scene);
    for _ in 0..4 {
        scene.window.dispatch(InputEvent::Key {
            keysym: 'a' as u32,
            utf8: Some("a".into()),
            pressed: true,
        });
        scene.window.dispatch(InputEvent::Key {
            keysym: 'a' as u32,
            utf8: Some("a".into()),
            pressed: false,
        });
    }
    assert!(scene.render(), "typing must dirty the scene");
    assert_ne!(field_before, sample(&scene), "password bullets must appear");

    // --- error state: reddish content appears in the card ---
    scene.window.show_error("Wrong password");
    assert!(scene.render());
    assert!(
        scene.any_in(CARD, |(r, g, b)| {
            r > 120 && r > g.saturating_add(40) && r > b.saturating_add(40)
        }),
        "error state must surface red content in the card"
    );

    // --- caps lock indicator ---
    scene.window.show_prompt("Password:", true); // clears the error
    scene.window.set_caps_lock(true);
    assert!(scene.render());
    assert!(
        scene.any_in(CARD, |(r, g, b)| r > 150 && g > 120 && b < 120),
        "caps-lock warning must surface yellow content"
    );
    scene.window.set_caps_lock(false);

    // --- status banner (reserved host-integration surface) ---
    scene
        .window
        .set_status_banner("Approval sent to your phone…");
    assert!(scene.render());
    assert!(
        scene.any_in((340, 700, 940, 790), |p| p != BG),
        "status banner must render bottom-center"
    );
    scene.window.set_status_banner("");

    // --- panel hidden: card region collapses to pure background ---
    scene.window.set_panel_visible(false);
    assert!(scene.render());
    assert!(
        scene.uniform_bg(CARD),
        "card region must be background-only when panel-hidden"
    );
}
