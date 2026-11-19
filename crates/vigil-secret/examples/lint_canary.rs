#![deny(unsafe_code)]

use vigil_secret::Secret;

fn leak(secret: &Secret) -> usize {
    secret.expose().len()
}

fn undocumented(p: *const u8) -> u8 {
    unsafe { *p }
}

unsafe fn bare(p: *const u8) -> u8 {
    *p
}

fn two_operations(p: *const u8) -> u8 {
    // SAFETY: never run; this file exists to be rejected.
    unsafe { bare(p) + *p }
}

fn main() {
    let byte = 0u8;
    let _ = (
        leak(&Secret::from("canary")),
        undocumented(&byte),
        two_operations(&byte),
    );
}
