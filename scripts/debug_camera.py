#!/usr/bin/env python3
"""Debug a V4L2 camera device (real or virtual/loopback).

Opens the device with OpenCV, reports the negotiated format, reads frames
(default: until q/Ctrl-C), measures fps, flags black/garbage frames, shows a
live window when a display is available (quit with q) and can save a
snapshot (--save).

Examples:
    python3 scripts/debug_camera.py /dev/video0
    python3 scripts/debug_camera.py /dev/video0 --fourcc MJPG --width 1280 --height 720
    python3 scripts/debug_camera.py /dev/video0 --fourcc YUYV --width 640 --height 480 --show
"""
import argparse
import sys
import time

import cv2


def fourcc_str(v: float) -> str:
    v = int(v)
    return "".join(chr((v >> (8 * i)) & 0xFF) for i in range(4))


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("device", nargs="?", default="/dev/video0")
    ap.add_argument("--frames", type=int, default=0, help="frames to read (0 = run until q/Ctrl-C)")
    ap.add_argument("--width", type=int, default=0, help="request width (0 = leave as-is)")
    ap.add_argument("--height", type=int, default=0, help="request height (0 = leave as-is)")
    ap.add_argument("--fps", type=float, default=0, help="request fps (0 = leave as-is)")
    ap.add_argument("--fourcc", default="", help="request fourcc, e.g. MJPG or YUYV")
    ap.add_argument("--show", action="store_true", help="force imshow window (q quits)")
    ap.add_argument("--no-show", action="store_true", help="never open a window")
    ap.add_argument("--save", default="", help="save first decoded frame to this path")
    args = ap.parse_args()

    # Show a window by default when a display seems available.
    have_display = bool(__import__("os").environ.get("DISPLAY") or __import__("os").environ.get("WAYLAND_DISPLAY"))
    show = args.show or (have_display and not args.no_show)

    cap = cv2.VideoCapture(args.device, cv2.CAP_V4L2)
    if not cap.isOpened():
        print(f"FAIL: cannot open {args.device}")
        return 1
    if args.width:
        cap.set(cv2.CAP_PROP_FRAME_WIDTH, args.width)
    if args.height:
        cap.set(cv2.CAP_PROP_FRAME_HEIGHT, args.height)
    if args.fps:
        cap.set(cv2.CAP_PROP_FPS, args.fps)
    if args.fourcc:
        cap.set(cv2.CAP_PROP_FOURCC, cv2.VideoWriter_fourcc(*args.fourcc.ljust(4)[:4]))

    w = cap.get(cv2.CAP_PROP_FRAME_WIDTH)
    h = cap.get(cv2.CAP_PROP_FRAME_HEIGHT)
    fps = cap.get(cv2.CAP_PROP_FPS)
    fc = fourcc_str(cap.get(cv2.CAP_PROP_FOURCC))
    print(f"opened {args.device}: {w:.0f}x{h:.0f} @ {fps:.1f} fps, fourcc={fc!r}")

    ok = 0
    black = 0
    first = None
    last = None
    window_ok = False
    if show:
        try:
            cv2.namedWindow(f"{args.device} (q=quit)", cv2.WINDOW_NORMAL)
            window_ok = True
        except cv2.error as e:
            print(f"WARN: cannot open imshow window ({e}); continuing headless — use --save for a snapshot")
            show = False
    if show:
        print(f"showing {args.device} — press q in the window or Ctrl-C to quit")
    t0 = time.time()
    last_stats = t0
    fails = 0
    i = 0
    try:
        while args.frames == 0 or i < args.frames:
            i += 1
            ret, frame = cap.read()
            if not ret or frame is None:
                fails += 1
                if fails == 1:
                    print("frame read failed (no writer? waiting...)")
                elif args.frames == 0 and time.time() - last_stats >= 5:
                    print(f"  ... still no frames after {fails} reads — is easyfilters running? (Ctrl-C to quit)")
                    last_stats = time.time()
                continue
            ok += 1
            if first is None:
                first = frame
            last = frame
            if frame.mean() < 2.0:
                black += 1
            if show:
                try:
                    cv2.imshow(f"{args.device} (q=quit)", frame)
                    if cv2.waitKey(1) & 0xFF == ord("q"):
                        break
                except cv2.error:
                    show = False
            if args.frames == 0 and time.time() - last_stats >= 5:
                print(f"  ... {ok} frames, {ok / (time.time() - t0):.1f} fps")
                last_stats = time.time()
    except KeyboardInterrupt:
        print("interrupted")
    dt = time.time() - t0

    total = args.frames if args.frames > 0 else i
    print(f"read {ok}/{total} frames in {dt:.2f}s -> {ok / dt if dt > 0 else 0:.1f} fps; black frames: {black}")
    if last is not None:
        print(f"last frame: shape={last.shape} mean={last.mean():.1f} std={last.std():.1f}")
    if args.save and first is not None:
        cv2.imwrite(args.save, first)
        print(f"saved first frame to {args.save}")
    if window_ok:
        cv2.destroyAllWindows()
    if ok == 0:
        print("NO FRAMES — is easyfilters running and writing to this device? "
              "(start it first: cargo run -- --output <this device>)")
    print("OK" if ok > 0 else "NO FRAMES")
    return 0 if ok > 0 else 2


if __name__ == "__main__":
    sys.exit(main())
