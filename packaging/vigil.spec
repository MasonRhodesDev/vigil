# RPM spec for vigil (repo: vigil). Built in COPR from a local SRPM produced
# by packaging/build-srpm.sh (source tarball from the git tag + vendored
# cargo deps as Source1 — no rust-*-devel packages needed).
#
# The package installs files only: binaries, the tmpfiles.d snippet for the
# greeter-writable state dir, and reference copies of the example config and
# default theme. The real /etc/greetd/vigil.toml stays operator-owned
# (greetd owns that directory). /etc/pam.d/vigil-lock ships as a
# pass-through hook (auth include login) — a named place for operator
# policy, not an opinion about it.
#
# The test suite runs by default; disable for a one-off build with
# --without check.
%bcond_without check

Name:           vigil
Version:        0.2.5
Release:        1%{?dist}
Summary:        Compositor-less greetd greeter and matching session lockscreen
License:        GPL-3.0-only
URL:            https://github.com/MasonRhodesDev/vigil
Source0:        %{url}/archive/v%{version}/%{name}-%{version}.tar.gz
Source1:        %{name}-%{version}-vendor.tar.xz

BuildRequires:  cargo-rpm-macros >= 24
BuildRequires:  systemd-rpm-macros
# smithay backends: libseat + libinput + libdrm/gbm + libudev; xkbcommon for
# keymaps; pam for the locker (pam-sys runs bindgen, hence clang).
BuildRequires:  libseat-devel
BuildRequires:  libinput-devel
BuildRequires:  libxkbcommon-devel
BuildRequires:  systemd-devel
BuildRequires:  libdrm-devel
BuildRequires:  mesa-libgbm-devel
BuildRequires:  pam-devel
BuildRequires:  clang-devel
BuildRequires:  fontconfig-devel
# The greeter runs as greetd's `greeter` user on greetd's socket.
Requires:       greetd
# groupadd in %pre for the shared monitor-profiles group.
Requires(pre):  shadow-utils
# The package creates this shared group in %%pre. Declare the capability so
# RPM's file-owner dependency for /etc/monitor-profiles is self-satisfied.
Provides:       group(monitor-profiles)

%description
vigil is a multi-monitor, themeable greetd greeter that renders directly on
KMS/DRM — no compositor in the login path — plus vigil-lock, a session
lockscreen speaking ext-session-lock-v1 that shares the greeter's theme,
config, and auth seams. One Rust binary per surface, Slint scenes per
output, runtime .slint themes with a compiled-in fallback.

%prep
# -a1 unpacks the vendor tarball (vendor/ at its root) into the source dir.
%autosetup -p1 -a1
%cargo_prep -v vendor
# cargo-rpm-macros replaces crates.io with the vendor directory, but it does
# not emit the source stanza required for pinned Git dependencies. cargo
# vendor includes both standards libraries in Source1; map those sources
# explicitly so the offline RPM build can resolve them.
cat >> .cargo/config.toml <<'EOF'

[source."git+https://github.com/MasonRhodesDev/monitor-profiles?rev=aef5f0e"]
git = "https://github.com/MasonRhodesDev/monitor-profiles"
rev = "aef5f0e"
replace-with = "vendored-sources"

[source."git+https://github.com/MasonRhodesDev/appearance-profiles.git?rev=75d831a"]
git = "https://github.com/MasonRhodesDev/appearance-profiles.git"
rev = "75d831a"
replace-with = "vendored-sources"

[source."git+https://github.com/MasonRhodesDev/slint-kit?rev=ec810d9d6f1b079ee0f4cf3d89f26ad7e63ec125"]
git = "https://github.com/MasonRhodesDev/slint-kit"
rev = "ec810d9d6f1b079ee0f4cf3d89f26ad7e63ec125"
replace-with = "vendored-sources"
EOF

%build
# vigil's default features include `gl` (FemtoVG over GBM/EGL).
%cargo_build
%{cargo_license_summary}
%{cargo_license} > LICENSE.dependencies

%install
# The build phase already produced both workspace binaries. Installing those
# artifacts avoids a second `cargo install` resolution from each member crate,
# which has no workspace lockfile and cannot resolve vendored Git sources.
install -Dpm0755 target/rpm/vigil %{buildroot}%{_bindir}/vigil
install -Dpm0755 target/rpm/vigil-lock %{buildroot}%{_bindir}/vigil-lock

install -Dpm0644 dist/vigil.tmpfiles %{buildroot}%{_tmpfilesdir}/vigil.conf
install -Dpm0644 dist/vigil-lock.pam %{buildroot}%{_sysconfdir}/pam.d/vigil-lock
install -Dpm0644 dist/vigil.toml.example %{buildroot}%{_datadir}/vigil/vigil.toml.example
install -Dpm0644 themes/default/theme.slint %{buildroot}%{_datadir}/vigil/themes/default.slint
install -d -m2775 %{buildroot}%{_sysconfdir}/monitor-profiles

%if %{with check}
%check
%cargo_test
%endif

%pre
# Shared monitor-profile group. The greeter reads layouts from
# /etc/monitor-profiles so the login screen arranges monitors the way the
# session will; the directory is group-writable so a desktop user can edit
# them without root, with no username baked in. hyprstate creates the same
# group and co-owns the directory with identical attributes -- either package
# may be installed alone.
getent group monitor-profiles >/dev/null || groupadd -r monitor-profiles || :

# /var/lib/vigil itself comes from the tmpfiles.d snippet via systemd's file
# triggers (no scriptlet needed on current Fedora).

%files
%license LICENSE LICENSE.dependencies
%doc README.md
%{_bindir}/vigil
%{_bindir}/vigil-lock
%{_tmpfilesdir}/vigil.conf
%config(noreplace) %{_sysconfdir}/pam.d/vigil-lock
%{_datadir}/vigil/
%dir %attr(2775,root,monitor-profiles) %{_sysconfdir}/monitor-profiles

%changelog
* Fri Aug 14 2026 Mason Rhodes <mrhodesdev@gmail.com> - 0.2.5-1
- Resolve per-user backgrounds through appearance-profiles.
- Cache and render user backgrounds off the UI thread.
- Focus the Hyprstate monitor-profile origin in vigil and vigil-lock.

* Thu Aug 13 2026 Mason Rhodes <mrhodesdev@gmail.com> - 0.2.4-1
- Reload shared monitor profiles while the greeter is running (#31).
- Pin monitor-profiles to aef5f0e (to_toml/CLI release).

* Thu Aug 06 2026 Mason Rhodes <mrhodesdev@gmail.com> - 0.2.2-1
- logind integration (SetLockedHint, Lock/Unlock, sleep invalidates
  grace); user list with Other… fallback; session default policy

* Wed Aug 05 2026 Mason Rhodes <mrhodesdev@gmail.com> - 0.2.1-1
- Remember last user/session (empty username = one-keypress relogin);
  /etc/pam.d/vigil-lock policy-free pass-through hook

* Wed Aug 05 2026 Mason Rhodes <mrhodesdev@gmail.com> - 0.2.0-1
- First packaged release: greeter (M1.5 spec-complete, multi-GPU) +
  vigil-lock (L1 + grace), vigil.toml config, tmpfiles.d state dir
