# Milestone 7 — Native PipeWire camera source

Goal: **publish the filtered feed as a native PipeWire Camera source node**,
next to the v4l2loopback device. This is the "Pure-PipeWire alternative"
from `docs/idea.md`, promoted from "revisit later" to now by hard
evidence: every PipeWire-based consumer app on this machine (GNOME
Snapshot, cosmic camera, browsers via the portal) is unable to stream the
v4l2loopback device because of a pipewire 1.6.11 bug, and no host-side
configuration changes that. A native pw node sidesteps the broken path
entirely — apps that enumerate cameras through PipeWire get a source they
can actually read.

## Background findings (verified live, 2026-11-13)

- Item list refresh from the milestone-6-era diagnosis:
  - pipewire **1.6.11** (openSUSE Tumbleweed current, no update pending)
    segfaults the daemon — `libspa-v4l2.so` inside `pw_impl_port_use_buffers`,
    SIGSEGV, two identical coredumps — whenever *any* consumer streams from
    the v4l2loopback device through PipeWire. PipeWire imports its peer
    buffers as USERPTR; v4l2loopback only supports MMAP/READ; the fallback
    path in the shipped build crashes. Ubuntu carries a five-patch set for
    exactly this (`v4l2-Add-mmap-fallback-when-USERPTR-is-not-supported`,
    LP#2131647, merged 2025-12-02); openSUSE does not.
  - WirePlumber's v4l2 SPA caches the caps it sees when the session starts
    — before easyfilters configures the loopback — and serves stale junk
    (`BGRx 2x1..8192x8192` raw ranges). Fixed in the setup script by
    pinning advertised caps at boot (`--pin-caps`, oneshot unit, commit
    `ee86f99`), but that only makes the device *listable*; the stream
    still crashes pipewire.
  - COSMIC camera is libcamera-based (no v4l2loopback support at all) and
    its PipeWire fallback hits the same crash. Snapshot (Aperture) is
    PipeWire-only.
- Conclusion: PipeWire compatibility has to come from a **native pw source
  node**, matching what `docs/idea.md` proposed as the alternative.

## Design

- `src/pwsource.rs`: a `pipewire-rs` 0.8 (`v0_3_34` feature gated APIs:
  `trigger_process`) stream node:
  - properties: `media.type=Video`, `media.category=Capture`,
    `media.role=Camera`, `media.class=Video/Source`,
    `node.name=easyfilters`, `node.description="EasyFilters Virtual
    Camera"`, so camera stacks list it exactly like a real webcam.
  - flags: `DRIVER | MAP_BUFFERS | RT_PROCESS | AUTOCONNECT`; the pw thread
    paces itself with a repeating loop timer at the negotiated
    framerate and calls `trigger_process`, so the stream runs whether or
    not a consumer drives the graph — and pauses gracefully when nobody
    is linked.
  - format: **raw RGBA** (`VideoFormat::RGBA`) at the negotiated WxH and
    fps — the pipeline already produces exactly that for the GUI preview
    (`mat_to_rgba`), so the pw node is fed from the same composite output,
    no extra decode/encode stage.
- Threading: pipewire's `MainLoop` is `Rc`-backed (not `Send`), so
  `MainLoop -> Context -> Core -> Stream` all live on one dedicated pw
  thread. The capture loop crosses the boundary via a latest-wins
  `Mutex<Option<Arc<[u8]>>>` slot; a 250 ms watchdog timer polls a
  shutdown atomic and quits the main loop on `Drop`.
- Pipeline integration: `capture_loop` gets `Option<&PwSource>`;
  `filter_frame` gains a `produce_rgba` flag (GUI preview *or* pw node) —
  passthrough mode decodes the frame for the pw node too, so the pw feed
  is never stale in passthrough.
- Lifecycle: the node is created per stream generation (a resolution
  change restarts the stream and recreates the node with matching caps);
  failure to create it logs and continues — the v4l2loopback path is
  unaffected.
- Config/settings: `pipewire = true` by default in `Settings`, persisted in
  `config.toml` (`pipewire` field), `--no-pipewire` CLI flag.

## Follow-ups (out of scope here)

- GUI toggle for the pw node (the settings plumbing is ready for it).
- Hiding the v4l2loopback node from WirePlumber while pipewire 1.6.11
  cannot stream it (protects users/apps from picking the crashable
  entry); revisit when a fixed pipewire lands.
- Upstream bug report with the coredump evidence to openSUSE.

## Acceptance criteria

1. `cargo test` passes (pod serialization unit test included).
2. `nix build` produces the updated binary; with the app running,
   `pw-dump` shows a `Video/Source` node from easyfilters with
   `media.role=Camera` and `node.description="EasyFilters Virtual Camera"`.
3. A PipeWire consumer can actually stream it: GNOME Snapshot lists it
   and shows the live filtered feed (through the portal/pw path, no
   v4l2loopback involvement), and the pipewire daemon stays alive (no
   core dumps during or after).
4. The v4l2loopback path is unchanged: easyfilters still writes the
   loopback at 30 fps (cameractrls preview keeps working).
5. `--no-pipewire` disables the node cleanly; the app keeps running.

## Live findings

- (2026-11-13, implementation session) All build-side acceptance criteria hold:
  `cargo test` passes (83 tests incl. the pod-serialization test), `nix
  build` produces the node binary, and with the app running the node
  registers correctly:
  `pw-dump` shows `Video/Source, node.name=easyfilters, role=Camera,
  description="EasyFilters Virtual Camera"`, and `pw-cli enum-params`
  reports exactly `RGBA, 1280x720, framerate 30/1`. The v4l2loopback path is
  untouched (queues 6, 30 fps, v4l2-ctl consumer streams), and the pw
  daemon **survives every attempt on the new node** — no core dumps
  (any select of the old `(V4L2)` node still crashes pw 1.6.11, as before).
- End-to-end app verification is **partial**:
  - GNOME Snapshot lists the node (Aperture's filter accepts it) but the
    stream fails with "could not play camera device". Important context:
    on this machine Snapshot also fails *identically on the physical
    camera* — which easyfilters holds for its input — so its viewfinder is
    broken for other reasons in this test setup (camerabin filesink
    errors in its log, unrelated to the source element). Needs a retest
    with easyfilters stopped to isolate.
  - The GStreamer `pipewiredeviceprovider` does **not** enumerate the
    node — verified with `gst-device-monitor-1.0` inside the cosmic
    flatpak: only the two `(V4L2)` nodes appear. gst/portal camera stacks
    list *device-backed* cameras (v4l2/libcamera nodes with WirePlumber's
    `object.path`/`device.id` props); a client stream node is filtered
    out. Consequently:
    - cosmic camera (enum via that provider) never sees the pw node —
      unchanged behavior for it.
    - Browsers via the xdg camera portal will not list it either — the
      portal's camera list is device-backed cameras only.
  - Known bug fixed during bring-up: the v4l2 frame interval is
    time-per-frame (num/den seconds); both the pw advertised framerate
    and the driver timer must invert it (`den/num`, `num/den s`).
- Follow-ups for real consumer compatibility (next session):
  1. Either mimic device-camera props on the stream node (investigate the
     exact `pipewiredeviceprovider` filter in gst-plugins-good and what
     the xdg camera portal accepts) or attach the node to a synthetic pw
     Device object.
  2. Retest Snapshot's viewfinder with easyfilters stopped (is the local
     breakage unrelated?) and with portal debug logs.
  3. Consider that until openSUSE ships a pipewire with the USERPTR/mmap
     fallback (Ubuntu SRU set), the canonical virtual-camera path for
     portals/browsers remains device-based (v4l2loopback) — the native pw
     node is the forward-compatible companion, not the portal fix.
