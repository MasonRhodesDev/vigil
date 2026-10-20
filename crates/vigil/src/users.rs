//! Login-user enumeration for the greeter's user list (issue #21). The list
//! itself is pure parsing over /etc/passwd + /etc/login.defs, so it works
//! with no D-Bus at all; AccountsService (system bus) only adds display
//! names and pictures and hides `SystemAccount` users when it is present.

use std::collections::HashMap;
use std::path::PathBuf;

use vigil_login::AccountProfile;

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

/// How the greeter shows one account.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Profile {
    /// Full name, or empty to show the login name.
    pub display_name: String,
    pub icon: Option<PathBuf>,
}

/// Full names from the passwd GECOS field (text before the first comma).
pub fn gecos_names(passwd: &str) -> HashMap<String, String> {
    passwd
        .lines()
        .filter_map(|line| {
            let mut fields = line.split(':');
            let name = fields.next()?;
            let gecos = fields.nth(3)?; // passwd, uid, gid, gecos
            let full = gecos.split(',').next().unwrap_or("").trim();
            (!name.is_empty() && !full.is_empty()).then(|| (name.to_owned(), full.to_owned()))
        })
        .collect()
}

/// AccountsService wins over GECOS (it is what settings apps write). Users
/// AccountsService marks `SystemAccount` are dropped from `list`; "Other…"
/// still reaches them.
pub fn apply_profiles(
    list: &mut Vec<String>,
    gecos: &HashMap<String, String>,
    accounts: &HashMap<String, AccountProfile>,
) -> HashMap<String, Profile> {
    list.retain(|name| !accounts.get(name).is_some_and(|a| a.system_account));
    list.iter()
        .map(|name| {
            let account = accounts.get(name);
            let display_name = account
                .map(|a| a.real_name.trim())
                .filter(|n| !n.is_empty())
                .or_else(|| gecos.get(name).map(String::as_str))
                .unwrap_or("")
                .to_owned();
            let icon = account.and_then(|a| a.icon.clone());
            (name.clone(), Profile { display_name, icon })
        })
        .collect()
}

/// Profiles for `list` from the live system, filtering it in place.
pub fn profiles(list: &mut Vec<String>) -> HashMap<String, Profile> {
    if list.is_empty() {
        return HashMap::new();
    }
    let gecos = std::fs::read_to_string("/etc/passwd")
        .map(|p| gecos_names(&p))
        .unwrap_or_default();
    let accounts = vigil_login::account_profiles("vigil", list);
    apply_profiles(list, &gecos, &accounts)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gecos_names_take_the_full_name_field() {
        let fixture = "mason:x:1000:1000:Mason Rhodes,,,:/home/mason:/bin/bash
ada:x:1001:1001::/home/ada:/bin/bash
bad
";
        let names = gecos_names(fixture);
        assert_eq!(names.get("mason").map(String::as_str), Some("Mason Rhodes"));
        assert!(!names.contains_key("ada"));
    }

    #[test]
    fn profiles_prefer_accountsservice_and_hide_system_accounts() {
        let mut list = vec!["ada".to_owned(), "build".to_owned(), "mason".to_owned()];
        let gecos = HashMap::from([
            ("mason".to_owned(), "Mason R".to_owned()),
            ("ada".to_owned(), "Ada (gecos)".to_owned()),
        ]);
        let accounts = HashMap::from([
            (
                "ada".to_owned(),
                AccountProfile {
                    real_name: "Ada Lovelace".into(),
                    icon: Some("/var/lib/AccountsService/icons/ada".into()),
                    system_account: false,
                },
            ),
            (
                "build".to_owned(),
                AccountProfile {
                    system_account: true,
                    ..Default::default()
                },
            ),
            // Empty RealName falls back to GECOS.
            ("mason".to_owned(), AccountProfile::default()),
        ]);
        let profiles = apply_profiles(&mut list, &gecos, &accounts);
        assert_eq!(list, ["ada", "mason"]);
        assert_eq!(profiles["ada"].display_name, "Ada Lovelace");
        assert!(profiles["ada"].icon.is_some());
        assert_eq!(profiles["mason"].display_name, "Mason R");
        assert!(!profiles.contains_key("build"));
    }

    #[test]
    fn profiles_without_accountsservice_are_gecos_only() {
        let mut list = vec!["ada".to_owned()];
        let profiles = apply_profiles(&mut list, &HashMap::new(), &HashMap::new());
        assert_eq!(list, ["ada"]);
        assert_eq!(profiles["ada"], Profile::default());
    }

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
