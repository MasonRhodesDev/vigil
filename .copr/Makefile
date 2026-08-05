srpm:
	dnf -y install git cargo rpm-build xz libseat-devel libinput-devel libxkbcommon-devel systemd-devel libdrm-devel mesa-libgbm-devel pam-devel clang-devel
	git config --global --add safe.directory '*'
	./packaging/build-srpm.sh --head
	cp $$HOME/rpmbuild/SRPMS/*.src.rpm "$(outdir)/"
