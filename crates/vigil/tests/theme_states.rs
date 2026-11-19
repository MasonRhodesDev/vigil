#![forbid(unsafe_code)]
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
            damage: None,
        })
    }

    fn px(&self, x: u32, y: u32) -> (u8, u8, u8) {
        let i = ((y * W + x) * 4) as usize;
        (self.buf[i + 2], self.buf[i + 1], self.buf[i])
    }

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

const CLOCK: (u32, u32, u32, u32) = (440, 213, 840, 318);
const CARD: (u32, u32, u32, u32) = (465, 354, 815, 488);
const FIELD_Y: u32 = 419;
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

    let masked = sample(&scene);
    let click_reveal = |scene: &mut Scene| {
        scene.window.dispatch(InputEvent::PointerAbsolute {
            x: REVEAL_X,
            y: FIELD_Y as f64,
        });
        scene.window.dispatch(InputEvent::PointerButton {
            button: 0x110,
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
            keysym: 0xff08,
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
    let dense: Vec<(u8, u8, u8)> = TEXT_X.map(|x| scene.px(x, FIELD_Y)).collect();
    assert!(
        dense.windows(2).all(|pair| pair[0] == pair[1]),
        "an empty revealed field must show nothing — a lone caret bar breaks row uniformity"
    );
    click_reveal(&mut scene);
    assert!(scene.render());

    scene.window.show_error("Wrong password");
    assert!(scene.render());
    assert!(
        scene.any_in(CARD, |(r, g, b)| {
            r > 120 && r > g.saturating_add(40) && r > b.saturating_add(40)
        }),
        "error state must surface red content in the card"
    );

    scene.window.show_prompt("Password:", true);
    scene.window.set_caps_lock(true);
    assert!(scene.render());
    assert!(
        scene.any_in(CARD, |(r, g, b)| r > 200 && g > 150 && b > 150),
        "caps-lock chip must surface Theme.warning-fg in the card"
    );
    scene.window.set_caps_lock(false);

    scene
        .window
        .set_status_banner("Approval sent to your phone…");
    assert!(scene.render());
    assert!(
        scene.any_in((340, 700, 940, 790), |p| p != BG),
        "status banner must render bottom-center"
    );
    scene.window.set_status_banner("");

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

#[test]
fn native_background_partial_repaint_matches_full() {
    let platform = VigilPlatform::install().expect("platform");
    let theme = Theme::load_or_default(None);
    let component = theme.instantiate().expect("instantiate");
    let adapter = platform.claim_last_adapter().expect("adapter");
    let window = OutputWindow::new(OutputId(4), W, H, 1.0, adapter, component).expect("window");
    let mut scene = Scene {
        window,
        buf: vec![0u8; (W * H * 4) as usize],
    };
    let background: std::sync::Arc<[u8]> = (0..W * H * 4)
        .map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8)
        .collect::<Vec<_>>()
        .into();

    scene.window.set_panel_visible(true);
    scene.window.set_user_name("alice");
    scene.window.set_clock("12:34");
    scene.window.show_prompt("Password:", true);
    assert!(
        scene
            .window
            .set_native_background_xrgb(background.clone(), W, H)
    );
    assert!(scene.render(), "first frame draws");

    let full_repaint_of_same_scene = |scene: &mut Scene| -> Vec<u8> {
        assert!(
            scene
                .window
                .set_native_background_xrgb(background.clone(), W, H)
        );
        let mut buf = vec![0u8; (W * H * 4) as usize];
        assert!(scene.window.render_if_needed(FrameTarget {
            buffer: &mut buf,
            width: W,
            height: H,
            stride: (W * 4) as usize,
            damage: None,
        }));
        buf
    };

    type Step<'a> = (&'a str, &'a dyn Fn(&mut OutputWindow));
    let steps: [Step; 5] = [
        ("clock", &|w| w.set_clock("12:35")),
        ("typing", &|w| {
            for pressed in [true, false] {
                w.dispatch(InputEvent::Key {
                    keysym: 'a' as u32,
                    utf8: Some("a".into()),
                    pressed,
                });
            }
        }),
        ("caps", &|w| w.set_caps_lock(true)),
        ("banner", &|w| {
            w.set_status_banner("Approval sent to your phone…")
        }),
        ("caps off", &|w| w.set_caps_lock(false)),
    ];
    for (name, step) in steps {
        step(&mut scene.window);
        assert!(
            scene.window.render_if_needed(FrameTarget {
                buffer: &mut scene.buf,
                width: W,
                height: H,
                stride: (W * 4) as usize,
                damage: None,
            }),
            "{name}: change must draw"
        );
        let partial = scene.buf.clone();
        let full = full_repaint_of_same_scene(&mut scene);
        let differing = partial
            .chunks(4)
            .zip(full.chunks(4))
            .filter(|(a, b)| a[..3] != b[..3])
            .count();
        assert_eq!(differing, 0, "{name}: partial repaint differs from full");
    }
}
