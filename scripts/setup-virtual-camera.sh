#!/usr/bin/env bash
# setup-virtual-camera.sh — one-shot system setup for EasyFilters' virtual camera.
#
# Installs and configures the v4l2loopback kernel module so that a virtual
# camera device (default label "EasyFilters Virtual Camera") exists and
# autoloads at boot. Run once as root:
#
#   sudo scripts/setup-virtual-camera.sh
#
# Options:
#   --label "NAME"        card label shown in app camera pickers
#                         (default: "EasyFilters Virtual Camera")
#   --exclusive-caps 0|1  0 = visible to both PipeWire and V4L2 apps (default)
#                         1 = visible to V4L2 apps only (Discord/Chrome style)
#   --video-nr N          pin the /dev/videoN number (-1 = auto, default)
#   --max-buffers N       v4l2loopback max_buffers option (default: 6)
#                         0.15.x grants exactly max_buffers mmap buffers per
#                         REQBUFS regardless of the count requested, so this
#                         must cover apps demanding an exact buffer count
#                         (cameractrls preview: 6)
#   --pin-caps CAPS       pin the caps pipe-processor apps see at boot
#                         (default: MJPG:1280x720@30/1, matching easyfilters
#                         defaults; 'any' disables). WirePlumber's v4l2 SPA
#                         caches the caps it first probes; without pinning it
#                         races easyfilters at session start and caches the
#                         loopback's unconfigured default (BGRx ranges).
#   --status              show current state, no changes (no root needed)
#   --uninstall           remove the config files and unload the module
#   --dry-run             print what would be done, change nothing
#
# The script is idempotent. Our modprobe file is named 99-... so it overrides
# any earlier v4l2loopback options (e.g. an OBS one) for the same parameters.
set -euo pipefail

LABEL="${EASYFILTERS_LABEL:-EasyFilters Virtual Camera}"
EXCLUSIVE_CAPS="${EASYFILTERS_EXCLUSIVE_CAPS:-0}"
VIDEO_NR="${EASYFILTERS_VIDEO_NR:--1}"
MAX_BUFFERS="${EASYFILTERS_MAX_BUFFERS:-6}"
PIN_CAPS="${EASYFILTERS_PIN_CAPS:-MJPG:1280x720@30/1}"
DRY_RUN=0
ACTION=setup

MODPROBE_CONF=/etc/modprobe.d/99-easyfilters-v4l2loopback.conf
LOAD_CONF=/etc/modules-load.d/easyfilters-v4l2loopback.conf
CAPS_UNIT=/etc/systemd/system/easyfilters-vcam-caps.service

usage() { sed -n '2,24p' "$0"; }

while [[ $# -gt 0 ]]; do
  case "$1" in
    --label)          LABEL="$2"; shift 2 ;;
    --exclusive-caps) EXCLUSIVE_CAPS="$2"; shift 2 ;;
    --video-nr)       VIDEO_NR="$2"; shift 2 ;;
    --max-buffers)    MAX_BUFFERS="$2"; shift 2 ;;
    --pin-caps)       PIN_CAPS="$2"; shift 2 ;;
    --status)         ACTION=status; shift ;;
    --uninstall)      ACTION=uninstall; shift ;;
    --dry-run)        DRY_RUN=1; shift ;;
    -h|--help)        usage; exit 0 ;;
    *) echo "unknown option: $1" >&2; usage; exit 2 ;;
  esac
done

run() {
  if [[ $DRY_RUN -eq 1 ]]; then
    echo "+ $*"
  else
    "$@"
  fi
}

# Print the /dev/videoN whose card label matches $1, or nothing.
find_device_by_label() {
  local label="$1" dev name
  for dev in /sys/class/video4linux/video*; do
    [[ -r "$dev/name" ]] || continue
    name="$(cat "$dev/name")"
    if [[ "$name" == "$label" ]]; then
      echo "/dev/${dev##*/}"
      return 0
    fi
  done
  return 1
}

install_package() {
  local id flavor pkg
  id="$(awk -F= '/^ID=/{gsub(/"/,"",$2); print $2}' /etc/os-release)"
  case "$id" in
    opensuse*|suse|sles)
      # kmp package must match the running kernel flavor (…-default, …-xen, …)
      flavor="$(uname -r)"; flavor="${flavor##*-}"
      pkg="v4l2loopback-kmp-$flavor"
      run zypper --non-interactive install "$pkg"
      ;;
    debian|ubuntu|linuxmint|pop)
      run apt-get update
      run apt-get install -y v4l2loopback-dkms
      ;;
    fedora|rhel|centos)
      # Fedora ships it via RPM Fusion
      run dnf install -y v4l2loopback
      ;;
    arch|manjaro|endeavouros)
      run pacman -S --noconfirm v4l2loopback-dkms
      ;;
    *)
      echo "Unsupported distro '$id'. Install the v4l2loopback DKMS package" >&2
      echo "manually (see https://github.com/v4l2loopback/v4l2loopback) and re-run." >&2
      [[ $DRY_RUN -eq 1 ]] || exit 1
      ;;
  esac
}

write_configs() {
  echo "+ writing $MODPROBE_CONF"
  if [[ $DRY_RUN -eq 1 ]]; then
    echo "  options v4l2loopback exclusive_caps=$EXCLUSIVE_CAPS card_label=\"$LABEL\" video_nr=$VIDEO_NR max_buffers=$MAX_BUFFERS"
    echo "+ writing $LOAD_CONF"
    echo "  v4l2loopback"
  else
    printf 'options v4l2loopback exclusive_caps=%s card_label="%s" video_nr=%s max_buffers=%s\n' \
      "$EXCLUSIVE_CAPS" "$LABEL" "$VIDEO_NR" "$MAX_BUFFERS" > "$MODPROBE_CONF"
    echo "v4l2loopback" > "$LOAD_CONF"
  fi
  write_caps_unit
}

# Pin the advertised caps of the loopback at boot via a oneshot unit ordered
# before the display manager. WirePlumber's v4l2 SPA probes the device when
# the user session starts — before easyfilters has configured the format —
# and caches whatever the unconfigured loopback advertises (raw BGRx ranges
# up to max_width/max_height). Camera stacks (GStreamer device monitor,
# snapshot/cosmic pickers) then see bogus caps until the node is recreated.
write_caps_unit() {
  if [[ $PIN_CAPS == any ]]; then
    run rm -f "$CAPS_UNIT"
    return 0
  fi
  echo "+ writing $CAPS_UNIT (advertised caps: $PIN_CAPS)"
  if [[ $DRY_RUN -eq 1 ]]; then
    echo "  (oneshot after modules-load, before display-manager: $PIN_CAPS)"
    return 0
  fi
  cat > "$CAPS_UNIT" <<EOF
[Unit]
Description=Pin EasyFilters virtual camera caps (advertised format)
Documentation=man:v4l2loopback-ctl(1)
After=systemd-modules-load.service
Before=display-manager.service graphical.target

[Service]
Type=oneshot
ExecStart=/bin/sh -c 'S="$LABEL"; for f in /sys/class/video4linux/video*; do if [ -r "\$f/name" ] && [ "\$(cat "\$f/name")" = "\$S" ]; then /usr/bin/v4l2loopback-ctl set-caps /dev/\${f##*/} "$PIN_CAPS"; fi; done; true'
RemainAfterExit=yes

[Install]
WantedBy=multi-user.target
EOF
}

module_loaded() {
  [[ -d /sys/module/v4l2loopback ]]
}

load_module() {
  if module_loaded; then
    echo "v4l2loopback already loaded; reloading to apply new options"
    run rmmod v4l2loopback || true
  fi
  if run modprobe v4l2loopback; then
    return 0
  fi
  echo "ERROR: could not load v4l2loopback for kernel $(uname -r)." >&2
  echo "The DKMS module may not be built for the running kernel yet —" >&2
  echo "reboot into a kernel that has it built, then re-run this script." >&2
  [[ $DRY_RUN -eq 1 ]] || exit 1
}

show_status() {
  echo "kernel:        $(uname -r)"
  echo "module:        $(module_loaded && echo loaded || echo 'not loaded')"
  if [[ -d /sys/module/v4l2loopback/parameters ]]; then
    echo "exclusive_caps: $(cat /sys/module/v4l2loopback/parameters/exclusive_caps 2>/dev/null | cut -d, -f1)"
    echo "max_buffers:    $(cat /sys/module/v4l2loopback/parameters/max_buffers 2>/dev/null | cut -d, -f1)"
    echo "module version: $(cat /sys/module/v4l2loopback/version 2>/dev/null)"
  fi
  if dev="$(find_device_by_label "$LABEL")"; then
    echo "device:        $dev ($LABEL)"
    if command -v v4l2loopback-ctl >/dev/null; then
      echo "advertised:    $(v4l2loopback-ctl get-caps "$dev" 2>/dev/null)"
    fi
  else
    echo "device:        no /dev/videoN with label '$LABEL' (try: $0 --label 'OBS Virtual Camera' --status)"
  fi
  if command -v pw-dump >/dev/null; then
    # with exclusive_caps=0 wireplumber exposes the loopback as a source node
    # named v4l2_input._sys_devices_virtual_video4linux_videoN
    if pw-dump 2>/dev/null | grep '"node.name": *"v4l2_input\._sys_devices_virtual_video4linux' >/dev/null; then
      echo "pipewire:      virtual camera visible to PipeWire clients"
    else
      echo "pipewire:      virtual camera NOT visible to PipeWire clients (exclusive_caps=1 or not loaded)"
    fi
  fi
}

case "$ACTION" in
  status)
    show_status
    ;;
  uninstall)
    [[ $EUID -eq 0 || $DRY_RUN -eq 1 ]] || { echo "run as root (or --dry-run)" >&2; exit 1; }
    run systemctl disable --now easyfilters-vcam-caps.service
    run rm -f "$MODPROBE_CONF" "$LOAD_CONF" "$CAPS_UNIT"
    run rmmod v4l2loopback || true
    echo "Config removed and module unloaded. The package itself was left installed."
    ;;
  setup)
    [[ $EUID -eq 0 || $DRY_RUN -eq 1 ]] || { echo "run as root: sudo $0 (or --dry-run)" >&2; exit 1; }
    install_package
    write_configs
    load_module
    if [[ $PIN_CAPS != any ]]; then
      run systemctl enable easyfilters-vcam-caps.service
      run systemctl start easyfilters-vcam-caps.service
    fi
    if dev="$(find_device_by_label "$LABEL")"; then
      echo "OK: virtual camera available at $dev (\"$LABEL\")"
      echo "Pass it to easyfilters with --output $dev, or let the app find it by label."
    elif [[ $DRY_RUN -eq 1 ]]; then
      echo "(dry-run) would verify that a /dev/videoN with label \"$LABEL\" now exists"
    else
      echo "Module loaded but no device with label '$LABEL' yet." >&2
      echo "If the running kernel had no matching DKMS build, reboot and re-run." >&2
      exit 1
    fi
    echo "Done. The device will reappear automatically at boot (modules-load.d)."
    ;;
esac
