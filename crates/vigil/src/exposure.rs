use std::fs::File;
use std::io::{BufRead, BufReader};
use std::os::unix::fs::FileExt;

use greetd_ipc::Response;
use vigil_core::{Canvas, FrameTarget};

use crate::keys::tests::{RETURN, Rig, secret, tap, text};
use crate::keys::{KeySink, route};
use crate::scripted_greetd::Greetd;

const ESCAPE: u32 = 0xff1b;
const SIZE: (u32, u32) = (1280, 800);

fn press(rig: &mut Rig, events: impl IntoIterator<Item = vigil_core::InputEvent>) {
    for event in events {
        route(rig, event);
    }
    rig.settle();
}

fn frame(rig: &mut Rig) -> Vec<u8> {
    let stride = SIZE.0 as usize * 4;
    let mut buffer = vec![0; stride * SIZE.1 as usize];
    rig.window.render(Canvas::Cpu(FrameTarget {
        buffer: &mut buffer,
        width: SIZE.0,
        height: SIZE.1,
        stride,
        damage: None,
    }));
    buffer
}

fn frames_of(rig: &mut Rig, texts: [&str; 2]) -> [Vec<u8>; 2] {
    texts.map(|typed| {
        press(rig, tap(ESCAPE, None));
        press(rig, text(typed));
        frame(rig)
    })
}

#[test]
fn a_hidden_field_paints_the_same_pixels_whatever_is_typed() {
    let Some(greetd) = Greetd::scripted("hidden-pixels", Vec::new()) else {
        return;
    };
    let mut rig = Rig::new(&greetd);
    rig.auth.begin(Some("alice"), &mut rig.window).unwrap();
    let empty = frame(&mut rig);
    let [wide, narrow] = frames_of(&mut rig, ["MMMMMMMM", "iiiiiiii"]);
    assert!(
        empty != wide,
        "typing changed no pixel, so equal frames prove nothing"
    );
    assert!(wide == narrow, "a hidden field painted what was typed");
    drop(rig);
    assert_eq!(greetd.finish(), Vec::<String>::new());
}

#[test]
fn a_visible_field_paints_what_is_typed() {
    let Some(greetd) = Greetd::scripted("visible-pixels", Vec::new()) else {
        return;
    };
    let mut rig = Rig::new(&greetd);
    rig.auth.begin(None, &mut rig.window).unwrap();
    let [wide, narrow] = frames_of(&mut rig, ["MMMMMMMM", "iiiiiiii"]);
    assert!(wide != narrow, "the frames cannot tell two texts apart");
    drop(rig);
    assert_eq!(greetd.finish(), Vec::<String>::new());
}

#[derive(Debug, Default, PartialEq)]
struct Residue {
    heap: usize,
    stack: usize,
    anonymous: usize,
    inside_a_greetd_request: usize,
    file: Vec<String>,
}

// The needle and the read buffer hold the text themselves, so hits inside
// them are skipped by address.
fn scan(needle: &[u8]) -> Residue {
    let memory = File::open("/proc/self/mem").expect("open /proc/self/mem");
    let maps = BufReader::new(File::open("/proc/self/maps").expect("open /proc/self/maps"));
    let mut buffer = vec![0u8; 1 << 20];
    let own = |at: usize, of: &[u8]| of.as_ptr_range().contains(&(at as *const u8));
    let mut residue = Residue::default();
    for line in maps.lines() {
        let line = line.expect("read /proc/self/maps");
        let mut fields = line.split_whitespace();
        let (Some(range), Some(perms)) = (fields.next(), fields.next()) else {
            continue;
        };
        let name = fields.nth(3).unwrap_or_default();
        if !perms.starts_with('r') || name.starts_with("[v") {
            continue;
        }
        let Some((start, end)) = range.split_once('-') else {
            continue;
        };
        let start = usize::from_str_radix(start, 16).expect("map start");
        let end = usize::from_str_radix(end, 16).expect("map end");
        let mut at = start;
        while at < end {
            let want = buffer.len().min(end - at);
            let Ok(got) = memory.read_at(&mut buffer[..want], at as u64) else {
                break;
            };
            if got == 0 {
                break;
            }
            for offset in buffer[..got]
                .windows(needle.len())
                .enumerate()
                .filter(|(_, window)| *window == needle)
                .map(|(offset, _)| offset)
            {
                let hit = at + offset;
                if own(hit, needle) || own(hit, &buffer) {
                    continue;
                }
                if buffer[..offset].ends_with(br#""response":""#) {
                    residue.inside_a_greetd_request += 1;
                }
                match name {
                    "[heap]" => residue.heap += 1,
                    "[stack]" => residue.stack += 1,
                    "" => residue.anonymous += 1,
                    file => residue.file.push(format!("{file} at {hit:#x}")),
                }
            }
            if got < needle.len() {
                break;
            }
            at += got - (needle.len() - 1);
        }
    }
    residue
}

// Built at run time and typed one key at a time, so the only whole copies
// are the ones the greeter makes.
fn one_time_password() -> Vec<char> {
    let seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos() as u64
        ^ u64::from(std::process::id()) << 32;
    (0..24)
        .map(|index| {
            let mixed = seed
                .rotate_left(index * 5)
                .wrapping_mul(0x9e37_79b9_7f4a_7c15)
                >> 58;
            char::from(
                b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789@#"[mixed as usize],
            )
        })
        .collect()
}

#[test]
fn what_a_submitted_password_leaves_in_process_memory() {
    let Some(greetd) = Greetd::scripted(
        "memory",
        vec![secret(), Response::Success, Response::Success],
    ) else {
        return;
    };
    let mut rig = Rig::new(&greetd);
    rig.auth.begin(Some("alice"), &mut rig.window).unwrap();
    let password = one_time_password();
    for key in &password {
        press(&mut rig, tap(*key as u32, Some(&key.to_string())));
    }
    let needle: Vec<u8> = password.iter().map(|key| *key as u8).collect();

    let typed = scan(&needle);
    press(&mut rig, tap(RETURN, None));
    assert!(rig.auth.is_complete());
    let submitted = scan(&needle);
    drop(rig);
    let dropped = scan(&needle);
    println!("exposure: in the field   {typed:?}");
    println!("exposure: after submit   {submitted:?}");
    println!("exposure: greeter dropped {dropped:?}");

    assert!(
        typed.heap + typed.anonymous > 0,
        "the scan cannot see the text while it sits in the field"
    );
    for stage in [&typed, &submitted, &dropped] {
        assert_eq!(
            stage.file,
            Vec::<String>::new(),
            "the password reached a file mapping"
        );
    }
    let sent = greetd.finish();
    assert_eq!(sent.len(), 3, "{}", sent.len());
}
