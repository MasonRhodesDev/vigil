use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::Arc;
use std::time::{Duration, Instant};

use vigil_core::{AuthUi, BufferDamage, FrameTarget, InputEvent, OutputId};
use vigil_theme::Theme;
use vigil_ui::{OutputWindow, VigilPlatform};

struct Output {
    name: &'static str,
    panel: (u32, u32),
    transform: u8,
    scale: f32,
}

const OUTPUTS: [Output; 2] = [
    Output {
        name: "portrait-4k",
        panel: (3840, 2160),
        transform: 3,
        scale: 1.5,
    },
    Output {
        name: "ultrawide",
        panel: (3440, 1440),
        transform: 0,
        scale: 1.0,
    },
];

const WARMUP: usize = 10;

#[derive(Clone, Copy, PartialEq)]
enum Map {
    PerFrame,
    PerFramePopulated,
    Persistent,
}

struct Slot {
    fd: OwnedFd,
    len: usize,
    map: Map,
    held: Option<Mapping>,
}

struct Mapping {
    ptr: *mut u8,
    len: usize,
}

impl Mapping {
    fn bytes(&mut self) -> &mut [u8] {
        unsafe { std::slice::from_raw_parts_mut(self.ptr, self.len) }
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        unsafe { libc::munmap(self.ptr.cast(), self.len) };
    }
}

impl Slot {
    fn new(len: usize, map: Map) -> Self {
        let fd = unsafe { libc::memfd_create(c"present-bench".as_ptr(), libc::MFD_CLOEXEC) };
        assert!(fd >= 0, "memfd_create failed");
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        let sized = unsafe { libc::ftruncate(fd.as_raw_fd(), len as libc::off_t) };
        assert_eq!(sized, 0, "ftruncate failed");
        let mut slot = Self {
            fd,
            len,
            map,
            held: None,
        };
        if map == Map::Persistent {
            slot.held = Some(slot.map());
        }
        slot
    }

    fn map(&self) -> Mapping {
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                self.len,
                libc::PROT_READ | libc::PROT_WRITE,
                if self.map == Map::PerFramePopulated {
                    libc::MAP_SHARED | libc::MAP_POPULATE
                } else {
                    libc::MAP_SHARED
                },
                self.fd.as_raw_fd(),
                0,
            )
        };
        assert_ne!(ptr, libc::MAP_FAILED, "mmap failed");
        Mapping {
            ptr: ptr.cast(),
            len: self.len,
        }
    }

    fn with_bytes<R>(&mut self, f: impl FnOnce(&mut [u8]) -> R) -> R {
        match self.held.as_mut() {
            Some(mapping) => f(mapping.bytes()),
            None => f(self.map().bytes()),
        }
    }
}

struct Chain {
    slots: [Slot; 2],
    damage: BufferDamage,
    back: usize,
    panel: (u32, u32),
}

impl Chain {
    fn new(panel: (u32, u32), map: Map) -> Self {
        let len = panel.0 as usize * panel.1 as usize * 4;
        Self {
            slots: [Slot::new(len, map), Slot::new(len, map)],
            damage: BufferDamage::new(2),
            back: 0,
            panel,
        }
    }

    fn present(&mut self, window: &mut OutputWindow) -> bool {
        let (width, height) = self.panel;
        let mut frame = self.damage.begin(self.back);
        let drew = self.slots[self.back].with_bytes(|buffer| {
            window.render_if_needed(FrameTarget {
                buffer,
                width,
                height,
                stride: width as usize * 4,
                damage: Some(&mut frame),
            })
        });
        self.damage.finish(self.back, frame, drew);
        if drew {
            self.back ^= 1;
        }
        drew
    }

    fn front_hash(&mut self) -> u64 {
        self.slots[self.back ^ 1].with_bytes(|buffer| {
            buffer
                .as_chunks::<8>()
                .0
                .iter()
                .fold(0xcbf2_9ce4_8422_2325, |h, c| {
                    (h ^ u64::from_le_bytes(*c)).wrapping_mul(0x0000_0100_0000_01b3)
                })
        })
    }
}

#[derive(Default)]
struct Samples {
    draw: Vec<Duration>,
    out: Vec<Duration>,
    total: Vec<Duration>,
    skipped: usize,
}

fn micros(sorted: &[Duration], quantile: f64) -> u128 {
    if sorted.is_empty() {
        return 0;
    }
    let index = ((sorted.len() - 1) as f64 * quantile).round() as usize;
    sorted[index].as_micros()
}

fn timed_present(window: &mut OutputWindow, chain: &mut Chain, samples: Option<&mut Samples>) {
    vigil_ui::advance_timers();
    let t0 = Instant::now();
    let needed = window.scene_needs_present();
    let t1 = Instant::now();
    let drew = needed && chain.present(window);
    let t2 = Instant::now();
    let Some(samples) = samples else {
        return;
    };
    if !drew {
        samples.skipped += 1;
        return;
    }
    samples.draw.push(t1 - t0);
    samples.out.push(t2 - t1);
    samples.total.push(t2 - t0);
}

fn settle(window: &mut OutputWindow, chain: &mut Chain) {
    let mut idle = 0;
    while idle < 10 {
        std::thread::sleep(Duration::from_millis(20));
        vigil_ui::advance_timers();
        if window.scene_needs_present() && chain.present(window) {
            idle = 0;
        } else {
            idle += 1;
        }
    }
}

fn background(scene: (u32, u32), seed: u32) -> Arc<[u8]> {
    let (w, h) = scene;
    let mut pixels = Vec::with_capacity(w as usize * h as usize * 4);
    for y in 0..h {
        for x in 0..w {
            let noise = (x ^ seed).wrapping_mul(2_654_435_761) ^ y.wrapping_mul(40_503);
            pixels.extend_from_slice(&[
                (x * 255 / w) as u8,
                (y * 255 / h) as u8,
                (noise >> 24) as u8 / 4 + (seed as u8 & 0x3f),
                0,
            ]);
        }
    }
    pixels.into()
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

struct Pointer {
    at: (f64, f64),
    step: (f64, f64),
    bounds: (f64, f64),
}

impl Pointer {
    fn advance(&mut self) -> (f64, f64) {
        for (at, step, bound) in [
            (&mut self.at.0, &mut self.step.0, self.bounds.0),
            (&mut self.at.1, &mut self.step.1, self.bounds.1),
        ] {
            *at += *step;
            if *at < 0.0 || *at > bound {
                *step = -*step;
                *at += 2.0 * *step;
            }
        }
        self.at
    }
}

fn run(output: &Output, presents: usize, map: Map, in_frame: bool, platform: &VigilPlatform) {
    let theme = Theme::load_or_default(None);
    let component = theme.instantiate().expect("instantiate");
    let adapter = platform.claim_last_adapter().expect("adapter");
    let mut window = OutputWindow::with_transform(
        OutputId(1),
        output.panel.0,
        output.panel.1,
        output.scale,
        output.transform,
        adapter,
        component,
    )
    .expect("window");
    let scene = window.scene_size();
    window.set_panel_visible(true);
    window.set_cursor_visible(in_frame);
    window.set_clock("13:37");
    window.apply_kit_tokens(&slint_kit::embedded_fallback("dark"));
    window.set_users(&["user".into()]);
    window.set_user_index(0);
    window.set_user_name("user");
    window.set_sessions(&["Session".into()]);
    window.set_session_index(0);
    window.show_prompt("Password:", true);

    let backgrounds = [background(scene, 1), background(scene, 2)];
    assert!(window.set_native_background_xrgb(backgrounds[0].clone(), scene.0, scene.1));

    let mut chain = Chain::new(output.panel, map);
    for _ in 0..2 {
        window.request_present();
        timed_present(&mut window, &mut chain, None);
    }
    settle(&mut window, &mut chain);

    let mut pointer = Pointer {
        at: (100.0, 100.0),
        step: (11.0, 7.0),
        bounds: (f64::from(scene.0 - 1), f64::from(scene.1 - 1)),
    };
    type Step<'a> = &'a mut dyn FnMut(&mut OutputWindow, usize);
    let scenarios: [(&str, Step); 3] = [
        ("motion", &mut |window, _| {
            let (x, y) = pointer.advance();
            window.dispatch(InputEvent::PointerAbsolute { x, y });
        }),
        ("typing", &mut |window, step| {
            if step.is_multiple_of(2) {
                key(window, 'a' as u32, "a");
            } else {
                key(window, 0xff08, "\u{8}");
            }
        }),
        ("full", &mut |window, step| {
            let pixels = backgrounds[(step + 1) % 2].clone();
            assert!(window.set_native_background_xrgb(pixels, scene.0, scene.1));
        }),
    ];
    for (name, step) in scenarios {
        let mut samples = Samples::default();
        for i in 0..WARMUP + presents {
            step(&mut window, i);
            let record = (i >= WARMUP).then_some(&mut samples);
            timed_present(&mut window, &mut chain, record);
        }
        settle(&mut window, &mut chain);
        for series in [&mut samples.draw, &mut samples.out, &mut samples.total] {
            series.sort();
        }
        vigil_log::out!(
            "output={} scenario={name} presents={} skipped={} \
             total_us_min={} total_us_p50={} total_us_p95={} total_us_max={} \
             draw_us_p50={} out_us_p50={} out_us_p95={} frame_hash={:016x}",
            output.name,
            samples.total.len(),
            samples.skipped,
            micros(&samples.total, 0.0),
            micros(&samples.total, 0.5),
            micros(&samples.total, 0.95),
            micros(&samples.total, 1.0),
            micros(&samples.draw, 0.5),
            micros(&samples.out, 0.5),
            micros(&samples.out, 0.95),
            chain.front_hash(),
        );
    }
}

fn main() {
    let mut presents = 120;
    let mut map = ("persistent", Map::Persistent);
    let mut cursor = "frame";
    let mut only = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--presents" => {
                presents = args
                    .next()
                    .and_then(|v| v.parse().ok())
                    .expect("--presents needs a count");
            }
            "--map" => {
                map = match args.next().as_deref() {
                    Some("per-frame") => ("per-frame", Map::PerFrame),
                    Some("per-frame-populated") => ("per-frame-populated", Map::PerFramePopulated),
                    Some("persistent") => ("persistent", Map::Persistent),
                    _ => panic!("--map needs per-frame, per-frame-populated or persistent"),
                };
            }
            "--cursor" => {
                cursor = match args.next().as_deref() {
                    Some("frame") => "frame",
                    Some("plane") => "plane",
                    _ => panic!("--cursor needs frame or plane"),
                };
            }
            "--output" => only = Some(args.next().expect("--output needs a name")),
            other => panic!("unknown argument {other}"),
        }
    }
    vigil_log::out!(
        "present_bench map={} cursor={cursor} presents={presents}",
        map.0
    );
    let platform = VigilPlatform::install().expect("platform");
    for output in &OUTPUTS {
        if only.as_deref().is_none_or(|name| name == output.name) {
            run(output, presents, map.1, cursor == "frame", &platform);
        }
    }
}
