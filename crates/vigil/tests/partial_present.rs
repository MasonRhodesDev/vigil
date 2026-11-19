use vigil_core::{AuthUi, BufferDamage, FrameTarget, InputEvent, OutputId, scene_to_panel};
use vigil_theme::Theme;
use vigil_ui::{OutputWindow, VigilPlatform};

const PANEL: (u32, u32) = (800, 600);
const STRIDE: usize = PANEL.0 as usize * 4;
const LEN: usize = STRIDE * PANEL.1 as usize;
const SCALE: f32 = 1.5;
const CURSOR: (u32, u32) = (18, 28);
const SENTINEL: u8 = 0x5a;

struct Lcg(u64);

impl Lcg {
    fn below(&mut self, bound: u32) -> u32 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        ((self.0 >> 33) % u64::from(bound)) as u32
    }
}

struct Swapchain {
    slots: [Vec<u8>; 2],
    damage: BufferDamage,
    back: usize,
}

impl Swapchain {
    fn new() -> Self {
        Self {
            slots: [vec![0; LEN], vec![0; LEN]],
            damage: BufferDamage::new(2),
            back: 0,
        }
    }

    fn present(&mut self, window: &mut OutputWindow) -> bool {
        let mut frame = self.damage.begin(self.back);
        let drew = window.render_if_needed(FrameTarget {
            buffer: &mut self.slots[self.back],
            width: PANEL.0,
            height: PANEL.1,
            stride: STRIDE,
            damage: Some(&mut frame),
        });
        self.damage.finish(self.back, frame, drew);
        if drew {
            self.back ^= 1;
        }
        drew
    }

    fn front(&self) -> &[u8] {
        &self.slots[self.back ^ 1]
    }
}

fn untracked(window: &mut OutputWindow) -> Vec<u8> {
    let mut buffer = vec![0; LEN];
    window.request_present();
    assert!(window.render_if_needed(FrameTarget {
        buffer: &mut buffer,
        width: PANEL.0,
        height: PANEL.1,
        stride: STRIDE,
        damage: None,
    }));
    buffer
}

fn greeter(platform: &VigilPlatform, theme: &Theme, id: u32, transform: u8) -> OutputWindow {
    let component = theme.instantiate().expect("instantiate");
    let adapter = platform.claim_last_adapter().expect("adapter");
    let mut window = OutputWindow::with_transform(
        OutputId(id),
        PANEL.0,
        PANEL.1,
        SCALE,
        transform,
        adapter,
        component,
    )
    .expect("window");
    window.set_panel_visible(true);
    window.set_cursor_visible(true);
    window.set_clock("12:34");
    window.set_user_name("user");
    window.show_prompt("Password:", true);
    window
}

fn key(window: &mut OutputWindow, keysym: u32, text: &str) {
    for pressed in [true, false] {
        window.dispatch(InputEvent::Key {
            keysym,
            utf8: Some(text.into()),
            pressed,
        });
    }
}

fn first_difference(got: &[u8], want: &[u8]) -> Option<(usize, usize, usize)> {
    let pixels = || got.as_chunks::<4>().0.iter().zip(want.as_chunks::<4>().0);
    let differing = pixels().filter(|(a, b)| a != b).count();
    let first = pixels().position(|(a, b)| a != b)?;
    Some((
        differing,
        first % PANEL.0 as usize,
        first / PANEL.0 as usize,
    ))
}

#[test]
fn partial_presents_match_full_presents_for_every_transform() {
    let platform = VigilPlatform::install().expect("platform");
    let theme = Theme::load_or_default(None);
    for transform in 0..4u8 {
        let mut subject = greeter(&platform, &theme, 1, transform);
        let mut reference = greeter(&platform, &theme, 2, transform);
        let scene = subject.scene_size();
        let mut chain = Swapchain::new();
        let mut rng = Lcg(u64::from(transform) + 1);
        let mut cursor_visible = true;
        let mut history = Vec::new();
        for step in 0..80 {
            let op = rng.below(10);
            match op {
                0 | 1 => {
                    let (x, y) = (rng.below(scene.0), rng.below(scene.1));
                    for window in [&mut subject, &mut reference] {
                        window.dispatch(InputEvent::PointerAbsolute {
                            x: f64::from(x),
                            y: f64::from(y),
                        });
                    }
                }
                2 => [&mut subject, &mut reference]
                    .into_iter()
                    .for_each(|w| key(w, 'a' as u32, "a")),
                3 => [&mut subject, &mut reference]
                    .into_iter()
                    .for_each(|w| key(w, 0xff08, "\u{8}")),
                4 => {
                    let text = format!("{:02}:{:02}", rng.below(24), rng.below(60));
                    subject.set_clock(&text);
                    reference.set_clock(&text);
                }
                5 => {
                    let on = rng.below(2) == 1;
                    subject.set_caps_lock(on);
                    reference.set_caps_lock(on);
                }
                6 => {
                    let text = "status ".repeat(rng.below(4) as usize);
                    subject.set_status_banner(&text);
                    reference.set_status_banner(&text);
                }
                7 => {
                    cursor_visible = !cursor_visible;
                    subject.set_cursor_visible(cursor_visible);
                    reference.set_cursor_visible(cursor_visible);
                }
                8 => {
                    chain.damage.invalidate();
                    for slot in &mut chain.slots {
                        slot.fill(SENTINEL);
                    }
                }
                _ => {
                    subject.set_clock("elsewhere");
                    reference.set_clock("elsewhere");
                    vigil_ui::advance_timers();
                    untracked(&mut subject);
                }
            }
            history.push(op);
            vigil_ui::advance_timers();
            subject.request_present();
            assert!(chain.present(&mut subject), "step {step}: forced present");
            let want = untracked(&mut reference);
            if let Some((differing, x, y)) = first_difference(chain.front(), &want) {
                panic!(
                    "transform {transform} step {step}: {differing} pixels differ from a full \
                     present, first at panel ({x}, {y}); ops so far {history:?}"
                );
            }
        }
    }
}

fn cursor_mask(transform: u8, scene: (u32, u32), pointers: &[(u32, u32)]) -> Vec<bool> {
    let mut mask = vec![false; (PANEL.0 * PANEL.1) as usize];
    for &(x, y) in pointers {
        for sy in y..(y + CURSOR.1).min(scene.1) {
            for sx in x..(x + CURSOR.0).min(scene.0) {
                let (px, py) = scene_to_panel(
                    transform,
                    scene.0 as usize,
                    scene.1 as usize,
                    sx as usize,
                    sy as usize,
                );
                mask[py * PANEL.0 as usize + px] = true;
            }
        }
    }
    mask
}

#[test]
fn pointer_motion_writes_only_the_cursor_rects() {
    let platform = VigilPlatform::install().expect("platform");
    let theme = Theme::load_or_default(None);
    for transform in 0..4u8 {
        let mut window = greeter(&platform, &theme, 1, transform);
        let scene = window.scene_size();
        let mut chain = Swapchain::new();
        let path = [(10, 10), (40, 25), (43, 31), (120, 90)];
        window.dispatch(InputEvent::PointerAbsolute {
            x: f64::from(path[0].0),
            y: f64::from(path[0].1),
        });
        for _ in 0..3 {
            vigil_ui::advance_timers();
            window.request_present();
            assert!(chain.present(&mut window));
        }
        for slot in &mut chain.slots {
            slot.fill(SENTINEL);
        }
        for moved in 1..path.len() {
            let (x, y) = path[moved];
            window.dispatch(InputEvent::PointerAbsolute {
                x: f64::from(x),
                y: f64::from(y),
            });
            vigil_ui::advance_timers();
            assert!(
                chain.present(&mut window),
                "transform {transform}: moving the cursor must present"
            );
            let allowed = cursor_mask(transform, scene, &path[..=moved]);
            let front = chain.front();
            let mut written = 0;
            let pixels = front.as_chunks::<4>().0.iter().zip(&allowed);
            for (pixel, (bytes, allowed)) in pixels.enumerate() {
                if *bytes != [SENTINEL; 4] {
                    written += 1;
                    assert!(
                        allowed,
                        "transform {transform} move {moved}: wrote panel ({}, {}) outside \
                         the cursor rects",
                        pixel % PANEL.0 as usize,
                        pixel / PANEL.0 as usize
                    );
                }
            }
            let (px, py) = scene_to_panel(
                transform,
                scene.0 as usize,
                scene.1 as usize,
                x as usize,
                y as usize,
            );
            assert_eq!(
                front[py * STRIDE + px * 4..py * STRIDE + px * 4 + 4],
                [0, 0, 0, 0],
                "transform {transform} move {moved}: the cursor outline is at the pointer"
            );
            assert!(written > 0);
        }
    }
}
