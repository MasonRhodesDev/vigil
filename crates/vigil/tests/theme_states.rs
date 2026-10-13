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
const BG: (u8, u8, u8) = (0x12, 0x13, 0x1a);

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

// Geometry at 1280x800 with no user row (the theme fixes every row height,
// so none of this depends on host font metrics):
//   clock 88px font in a 105px row, then a 33px gap;
//   card 360 wide x 144 tall (24 pad + 16 label + 8 + 44 field + 8 + 20
//   status + 24 pad), the clock+card column centered over the 736px above
//   the footer less a 32px lift -> column top 211, card x 460..820, y 349..493.
const CLOCK: (u32, u32, u32, u32) = (440, 211, 840, 316);
const CARD: (u32, u32, u32, u32) = (465, 354, 815, 488);
// Input field interior row: card top 349 + 24 + 16 + 8, plus half the field.
const FIELD_Y: u32 = 419;
// The field's text area: field x 484..796 less 12px left padding and the
// reveal toggle (742..790) on the right.
const TEXT_X: std::ops::Range<u32> = 500..735;
const REVEAL_X: f64 = 766.0;

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
        scene.any_in(CLOCK, |p| p != BG),
        "clock must render above the card"
    );

    // --- typed secret input shows bullets in the field ---
    let sample =
        |s: &Scene| -> Vec<(u8, u8, u8)> { TEXT_X.step_by(2).map(|x| s.px(x, FIELD_Y)).collect() };
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

    // --- reveal: toggling shows text, and an EMPTY revealed field is empty
    // (regression for the stray solid caret bar: cursor_flash_cycle is
    // pinned to zero, so with a nonzero text-cursor-width Slint draws a
    // permanent "|" beside — or alone instead of — the revealed password) ---
    let masked = sample(&scene);
    let click_reveal = |scene: &mut Scene| {
        scene.window.dispatch(InputEvent::PointerAbsolute {
            x: REVEAL_X,
            y: FIELD_Y as f64,
        });
        scene.window.dispatch(InputEvent::PointerButton {
            button: 0x110, // BTN_LEFT
            pressed: true,
        });
        scene.window.dispatch(InputEvent::PointerButton {
            button: 0x110,
            pressed: false,
        });
    };
    click_reveal(&mut scene);
    assert!(scene.render(), "reveal toggle must dirty the scene");
    assert_ne!(masked, sample(&scene), "revealed text must replace bullets");
    for _ in 0..4 {
        scene.window.dispatch(InputEvent::Key {
            keysym: 0xff08, // BackSpace
            utf8: None,
            pressed: true,
        });
        scene.window.dispatch(InputEvent::Key {
            keysym: 0xff08,
            utf8: None,
            pressed: false,
        });
    }
    assert!(scene.render(), "clearing the field must dirty the scene");
    // TEXT_X stays inside the text area: the reveal toggle occupies the
    // field's right end and would break uniformity legitimately.
    let dense: Vec<(u8, u8, u8)> = TEXT_X.map(|x| scene.px(x, FIELD_Y)).collect();
    assert!(
        dense.windows(2).all(|pair| pair[0] == pair[1]),
        "an empty revealed field must show nothing — a lone caret bar breaks row uniformity"
    );
    // Restore the masked state for the assertions that follow.
    click_reveal(&mut scene);
    assert!(scene.render());

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
        scene.any_in(CARD, |(r, g, b)| r > 200 && g > 150 && b > 150),
        "caps-lock chip must surface Theme.warning-fg in the card"
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

    // --- session picker: appears in the footer, bottom-left, once there is
    // more than one session (footer row y 736..772, picker x 28..300) ---
    let footer_left = |s: &Scene| -> Vec<(u8, u8, u8)> {
        (738..772)
            .step_by(2)
            .flat_map(|y| (28..310).step_by(2).map(move |x| (x, y)))
            .map(|(x, y)| s.px(x, y))
            .collect()
    };
    scene.window.set_sessions(&["Hyprland".into()]);
    scene.window.set_session_index(0);
    assert!(scene.render());
    let single = footer_left(&scene);
    scene
        .window
        .set_sessions(&["Hyprland".into(), "Sway".into()]);
    assert!(scene.render(), "second session must dirty the scene");
    assert_ne!(
        single,
        footer_left(&scene),
        "session picker must render bottom-left"
    );

    // --- panel hidden: the card goes, and the clock (now 112px in a 134px
    // row, centered: y 285..419) takes the middle alone. The card's lower
    // part, below the clock, collapses to pure background ---
    scene.window.set_panel_visible(false);
    assert!(scene.render());
    assert!(
        scene.uniform_bg((465, 430, 815, 488)),
        "card region must be background-only when panel-hidden"
    );
    assert!(
        scene.any_in((440, 285, 840, 419), |p| p != BG),
        "clock must take the middle of an output without the panel"
    );
}

/// Moving the pointer must dirty the frame.
///
/// The software cursor is composited at present time and never touches the
/// Slint scene, so nothing else marks the frame dirty when only the pointer
/// moved. Lose that and the cursor stops tracking -- it jumps once a second
/// when the clock happens to redraw. No other test moves the pointer, so
/// this is the only thing standing between that regression and metal.
#[test]
fn pointer_motion_alone_dirties_the_frame() {
    let platform = VigilPlatform::install().expect("platform");
    let theme = Theme::load_or_default(None);
    let component = theme.instantiate().expect("instantiate");
    let adapter = platform.claim_last_adapter().expect("adapter");
    let window = OutputWindow::new(OutputId(2), W, H, 1.0, adapter, component).expect("window");
    let mut scene = Scene {
        window,
        buf: vec![0u8; (W * H * 4) as usize],
    };

    scene.window.set_panel_visible(true);
    scene.window.set_cursor_visible(true);
    assert!(scene.render(), "first frame draws");
    // Settle: with nothing changing, a present is not needed.
    scene.render();
    assert!(
        !scene.render(),
        "an unchanged scene must not keep presenting"
    );

    scene
        .window
        .dispatch(vigil_core::InputEvent::PointerAbsolute { x: 0.5, y: 0.5 });
    assert!(
        scene.render(),
        "pointer motion must dirty the frame, or the cursor stops tracking"
    );
}

/// The inverse property: an output whose cursor is NOT composited into the
/// scene (hardware cursor, or simply not the panel output) must not present
/// for pointer motion alone — that is the entire point of a cursor plane.
#[test]
fn pointer_motion_without_scene_cursor_does_not_present() {
    let platform = VigilPlatform::install().expect("platform");
    let theme = Theme::load_or_default(None);
    let component = theme.instantiate().expect("instantiate");
    let adapter = platform.claim_last_adapter().expect("adapter");
    let window = OutputWindow::new(OutputId(3), W, H, 1.0, adapter, component).expect("window");
    let mut scene = Scene {
        window,
        buf: vec![0u8; (W * H * 4) as usize],
    };

    scene.window.set_panel_visible(true);
    scene.window.set_cursor_visible(false);
    assert!(scene.render(), "first frame draws");
    scene.render();
    assert!(
        !scene.render(),
        "an unchanged scene must not keep presenting"
    );
    scene
        .window
        .dispatch(vigil_core::InputEvent::PointerAbsolute { x: 0.5, y: 0.5 });
    assert!(
        !scene.render(),
        "motion with no scene cursor must not force a present"
    );
}
