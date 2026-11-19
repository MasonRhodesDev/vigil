#![no_main]

use libfuzzer_sys::fuzz_target;
use vigil_config::Config;

// 0xff never occurs in UTF-8, so it splits the system file from the user file.
fuzz_target!(|data: &[u8]| {
    let mut parts = data.splitn(2, |byte| *byte == 0xff);
    let system = parts.next().unwrap_or_default();
    let user = parts.next().unwrap_or_default();
    let (Ok(system), Ok(user)) = (std::str::from_utf8(system), std::str::from_utf8(user)) else {
        return;
    };
    let mut config = vigil_config::parse(system).unwrap_or_default();
    let _ = config.validate_warning();
    config.lock.warning.clamp();
    config.lock.transition.clamp();
    let Ok(user) = toml::from_str::<toml::Table>(user) else {
        return;
    };
    let before = config.clone();
    let _ = config.overlay(&user);
    let floor = |config: &Config| {
        let warning = &config.lock.warning;
        (
            config.lock.grace_secs,
            warning.cancel_on_motion_px.to_bits(),
            warning.duration_ms,
            warning.wallpaper_hold_max_ms,
            warning.wallpaper_in_ms,
        )
    };
    assert_eq!(
        floor(&config),
        floor(&before),
        "the user file moved a security floor key"
    );
    assert!(
        config.greeter == before.greeter
            && config.keyboard == before.keyboard
            && config.power == before.power
            && config.render == before.render
            && config.sessions == before.sessions
            && config.users == before.users,
        "the user file changed a greeter section"
    );
});
