#!/bin/sh
# Runs INSIDE the virtme-ng guest (root, host rootfs read-only). The scene
# run's counterpart of guest.sh: a fixed fake greetd, the splash and scene
# settings from $VIGIL_E2E_DIR, and DEBUG_STAGE_TAGS on so the frames say
# which stage owns each screen.
REPO=${VIGIL_REPO:?}
DIR=${VIGIL_E2E_DIR:?}
BIN=${VIGIL_E2E_BIN:-$REPO/target/release/vigil}
LOG=$DIR/vigil.log
: > "$LOG"
mkdir -p "$DIR/sessions"
printf '[Desktop Entry]\nName=Test DE\nExec=/bin/true\n' > "$DIR/sessions/test-de.desktop"
export VIGIL_SESSION_DIRS=$DIR/sessions
export VIGIL_POWER_INHIBIT=1
python3 "$REPO/tests/e2e/fake_greetd.py" /tmp/greetd.sock hunter2 >>"$LOG" 2>&1 &
sleep 0.5
for i in $(seq 100); do [ -e /dev/dri/card0 ] && break; sleep 0.2; done
command -v udevadm >/dev/null && { udevadm trigger --action=add 2>/dev/null; udevadm settle 2>/dev/null; }
seatd 2>>"$LOG" &
for i in $(seq 50); do [ -e /run/seatd.sock ] && break; sleep 0.1; done
export LIBSEAT_BACKEND=seatd
[ "${VIGIL_E2E_TAGS:-key}" = env ] && export DEBUG_STAGE_TAGS=1
# VIGIL_E2E_NOFBCON=1 unbinds the framebuffer console, so that a console
# repaint when the seat is disabled cannot be mistaken for the next owner.
if [ -n "${VIGIL_E2E_NOFBCON:-}" ]; then
    for v in /sys/class/vtconsole/vtcon*; do
        grep -q "frame buffer" "$v/name" 2>/dev/null && echo 0 > "$v/bind"
    done
fi
# The cue the host driver waits for; it then watches the screens from t=0.
echo "VIGIL-START" >> "$LOG"
"$BIN" --socket /tmp/greetd.sock --config "$VIGIL_E2E_CONFIG" 2>>"$LOG"
RC=$?
echo "VIGIL-EXIT:$RC" | tee -a "$LOG"
# VIGIL_E2E_NEXT=modetest plays the session: after a delay (greetd starting
# the session) a DRM client takes master and puts its own framebuffer on
# every screen. The holder (if on) must keep vigil's last frame up until then
# and be gone afterwards.
if [ -n "${VIGIL_E2E_NEXT:-}" ]; then
    holders() { for c in /proc/[0-9]*/comm; do [ "$(cat "$c" 2>/dev/null)" = vigil-hold ] && echo "${c%/comm}"; done; }
    echo "HOLDERS-AT-EXIT: $(holders | tr '\n' ' ')" >> "$LOG"
    sleep "${VIGIL_E2E_NEXT_DELAY:-3}"
    echo "NEXT-START" >> "$LOG"
    if [ "$VIGIL_E2E_NEXT" = hyprland ]; then
        # A real compositor: Hyprland on every card, solid-colour background,
        # no animations, started as the session would be (new libseat client).
        export XDG_RUNTIME_DIR=/tmp/xdg-next
        mkdir -p "$XDG_RUNTIME_DIR" && chmod 700 "$XDG_RUNTIME_DIR"
        printf 'misc {\n disable_hyprland_logo = true\n disable_splash_rendering = true\n background_color = 0xff2060c0\n force_default_wallpaper = 0\n}\nanimations { enabled = false }\n' > "$DIR/hypr-next.conf"
        export AQ_DRM_DEVICES=$(ls /dev/dri/card* | tr '\n' ':' | sed 's/:$//')
        Hyprland --i-am-really-stupid -c "$DIR/hypr-next.conf" > "$DIR/next.log" 2>&1 &
        NEXT=$!
        for i in $(seq 200); do grep -qi "monitor.*added\|Monitor.*ready\|Initialized" "$DIR/next.log" 2>/dev/null && break; sleep 0.1; done
        echo NEXT-OWNER-SET >> "$DIR/next.log"
    else
        python3 "$REPO/tests/e2e/next-owner.py" "$DIR/next.log" >> "$LOG" 2>&1 &
        NEXT=$!
        for i in $(seq 100); do grep -q "NEXT-OWNER-SET" "$DIR/next.log" 2>/dev/null && break; sleep 0.1; done
    fi
    echo "NEXT-SET" >> "$LOG"
    for i in $(seq 300); do [ -z "$(holders)" ] && break; sleep 0.1; done
    echo "HOLDERS-AFTER-NEXT: $(holders | tr '\n' ' ')" >> "$LOG"
    sleep 2
    echo "NEXT-DONE" >> "$LOG"
    sleep 0.3
    kill "$NEXT" 2>/dev/null
else
    echo "NEXT-DONE" >> "$LOG"
fi
dmesg > "$DIR/dmesg.txt" 2>/dev/null
