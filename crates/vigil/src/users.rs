//! Login-user enumeration for the greeter's user list (issue #21). Pure
//! parsing over /etc/passwd + /etc/login.defs so it works as the `greeter`
//! user with no D-Bus and no session bus.

/// Shell basenames that mean "this account cannot log in". A UID in the
/// login range is not enough on its own: build accounts (nix, jenkins)
/// live there too and would otherwise fill the list.
const NON_LOGIN_SHELLS: &[&str] = &["nologin", "false", "true", "sync", "shutdown", "halt"];

/// Users offered by the greeter, sorted, from the live system.
pub fn enumerate() -> Vec<String> {
    let (uid_min, uid_max) = std::fs::read_to_string("/etc/login.defs")
        .map(|defs| parse_uid_range(&defs))
        .unwrap_or((1000, 60000));
    std::fs::read_to_string("/etc/passwd")
        .map(|passwd| filter_users(&passwd, uid_min, uid_max))
        .unwrap_or_default()
}

/// `UID_MIN`/`UID_MAX` from login.defs contents; the documented defaults
/// when either is absent or unparseable.
pub fn parse_uid_range(defs: &str) -> (u32, u32) {
    let field = |key: &str, fallback: u32| {
        defs.lines()
            .map(str::trim)
            .filter(|line| !line.starts_with('#'))
            .find_map(|line| {
                let rest = line.strip_prefix(key)?;
                rest.split_whitespace().next()?.parse().ok()
            })
            .unwrap_or(fallback)
    };
    (field("UID_MIN", 1000), field("UID_MAX", 60000))
}

/// Login-capable users from /etc/passwd contents, sorted by name.
pub fn filter_users(passwd: &str, uid_min: u32, uid_max: u32) -> Vec<String> {
    let mut users: Vec<String> = passwd
        .lines()
        .filter_map(|line| {
            let mut fields = line.split(':');
            let name = fields.next()?;
            let _passwd = fields.next()?;
            let uid: u32 = fields.next()?.parse().ok()?;
            let shell = fields.nth(3)?; // gid, gecos, home, shell
            if !(uid_min..=uid_max).contains(&uid) || name.is_empty() {
                return None;
            }
            let basename = shell.rsplit('/').next().unwrap_or(shell);
            if shell.is_empty() || NON_LOGIN_SHELLS.contains(&basename) {
                return None;
            }
            Some(name.to_owned())
        })
        .collect();
    users.sort();
    users.dedup();
    users
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filter_users_keeps_only_login_capable() {
        let fixture = "root:x:0:0::/root:/bin/bash\n\
greeter:x:968:966::/var/lib/greetd:/sbin/nologin\n\
mason:x:1000:1000::/home/mason:/bin/bash\n\
nixbld1:x:30001:30000::/var/empty:/sbin/nologin\n\
svc:x:1500:1500::/srv:/bin/false\n";
        assert_eq!(filter_users(fixture, 1000, 60000), ["mason"]);
    }

    #[test]
    fn filter_users_sorts_and_respects_range() {
        let fixture = "zoe:x:1002:1002::/home/zoe:/bin/bash\n\
amy:x:1001:1001::/home/amy:/bin/bash\n\
late:x:70000:70000::/home/late:/bin/bash\n";
        assert_eq!(filter_users(fixture, 1000, 60000), ["amy", "zoe"]);
    }

    #[test]
    fn parse_uid_range_reads_login_defs() {
        assert_eq!(
            parse_uid_range("# c\nUID_MIN 500\nUID_MAX 2000\n"),
            (500, 2000)
        );
    }

    #[test]
    fn parse_uid_range_falls_back() {
        assert_eq!(parse_uid_range(""), (1000, 60000));
    }
}
