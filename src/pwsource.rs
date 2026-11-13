//! Native PipeWire Camera source node (milestone 7).
//!
//! The filtered feed is published directly as a PipeWire `Video/Source`
//! node with `media.role = Camera`, next to the v4l2loopback device.
//! PipeWire-based consumers — GNOME Snapshot, cosmic camera, browsers via
//! the camera portal — read this node natively and never touch the v4l2
//! SPA pipe. That sidesteps the pipewire 1.6.x bug that segfaults the
//! daemon whenever a consumer streams from a v4l2loopback device (USERPTR
//! buffer import into a driver that only supports MMAP; Ubuntu carries a
//! patchset for it, openSUSE's 1.6.11 does not).
//!
//! Threading model: `MainLoop` (like the rest of pipewire-rs) is not
//! `Send`, so the entire node — `MainLoop -> Context -> Core -> Stream`
//! with `StreamFlags::DRIVER` — lives on one dedicated pw thread. The
//! only state crossing the boundary is the latest-frame slot (a mutex)
//! and a shutdown atomic, which a watchdog timer polls so `Drop` can stop
//! the thread promptly. A repeating timer on the pw thread paces the
//! stream at the negotiated framerate via `trigger_process`, which makes
//! pipewire run the `process` callback: dequeue a buffer, memcpy the
//! slot, requeue. If no consumer is linked the callbacks are no-ops.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Once};
use std::thread;
use std::time::Duration;

use anyhow::{anyhow, Result};
use tracing::{info, warn};

use pipewire as pw;
use pw::{properties::properties, spa};

/// Format of the frames handed to us by the pipeline: tightly packed
/// RGBA8, row-major — exactly what `pipeline::mat_to_rgba` produces.
const STRIDE_PER_PIXEL: u32 = 4;

type FrameSlot = Arc<Mutex<Option<Arc<[u8]>>>>;

/// User data moved into the pw listener callbacks (all invoked on the pw
/// thread).
struct PwShared {
    slot: FrameSlot,
    w: u32,
    h: u32,
    delivered: u64,
}

pub struct PwSource {
    slot: FrameSlot,
    running: Arc<AtomicBool>,
    join: Option<thread::JoinHandle<()>>,
}

impl PwSource {
    /// Create and start the camera node. Returns once the pw thread is
    /// running; the node reaches the daemon asynchronously from there.
    pub fn new(w: u32, h: u32, num: u32, den: u32) -> Result<Self> {
        static INIT: Once = Once::new();
        INIT.call_once(|| {
            pw::init();
        });

        let slot: FrameSlot = Arc::new(Mutex::new(None));
        let running = Arc::new(AtomicBool::new(true));

        let pw_slot = Arc::clone(&slot);
        let pw_running = Arc::clone(&running);
        let join = thread::Builder::new()
            .name("easyfilters-pw".into())
            .spawn(move || {
                if let Err(e) = pw_thread(pw_slot, Arc::clone(&pw_running), w, h, num, den) {
                    warn!(error = %e, "pipewire source thread failed");
                }
                // The watchdog on the pw thread sees the flip via load()
                // below; this store is also PwSource::alive()'s signal.
                pw_running.store(false, Ordering::Release);
            })
            .map_err(|e| anyhow!("spawn pw thread: {e}"))?;

        info!(w, h, interval = format!("{num}/{den}s"), "pipewire camera node starting");
        Ok(Self {
            slot,
            running,
            join: Some(join),
        })
    }

    /// Whether the node thread is still alive (goes false if the pw
    /// connection died, e.g. after a pipewire daemon crash/restart).
    pub fn alive(&self) -> bool {
        self.running.load(Ordering::Acquire)
    }

    /// Publish one RGBA frame; the pw thread picks it up on the next tick.
    /// Latest-wins: if the pw thread lags, intermediate frames drop.
    pub fn push(&self, w: u32, h: u32, rgba: &[u8]) {
        if !self.running.load(Ordering::Acquire) {
            return;
        }
        if (w as usize) * (h as usize) * STRIDE_PER_PIXEL as usize != rgba.len() {
            warn!(w, h, len = rgba.len(), "pw frame size mismatch; dropping");
            return;
        }
        *self.slot.lock().unwrap() = Some(Arc::from(rgba));
    }
}

impl Drop for PwSource {
    fn drop(&mut self) {
        // The pw thread's watchdog timer polls this and quits its main
        // loop; MainLoop itself is not Send so it must be quitted in
        // thread.
        self.running.store(false, Ordering::Release);
        if let Some(handle) = self.join.take() {
            if handle.join().is_err() {
                warn!("pipewire source thread panicked during shutdown");
            }
        }
    }
}

/// Everything that must live on the pw thread while `mainloop.run()` blocks.
fn pw_thread(
    slot: FrameSlot,
    running: Arc<AtomicBool>,
    w: u32,
    h: u32,
    num: u32,
    den: u32,
) -> Result<()> {
    let running_loop = Arc::clone(&running);
    let mainloop =
        pw::main_loop::MainLoop::new(None).map_err(|e| anyhow!("pw mainloop: {e}"))?;
    let context =
        pw::context::Context::new(&mainloop).map_err(|e| anyhow!("pw context: {e}"))?;
    let core = context
        .connect(None)
        .map_err(|e| anyhow!("pw connect: {e}"))?;

    let stream = pw::stream::Stream::new(
        &core,
        "easyfilters",
        properties! {
            *pw::keys::MEDIA_TYPE => "Video",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_ROLE => "Camera",
            *pw::keys::MEDIA_CLASS => "Video/Source",
            *pw::keys::NODE_NAME => "easyfilters",
            *pw::keys::NODE_DESCRIPTION => "EasyFilters Virtual Camera",
            *pw::keys::APP_NAME => "easyfilters",
        },
    )
    .map_err(|e| anyhow!("pw stream: {e}"))?;

    let user_data = PwShared {
        slot,
        w,
        h,
        delivered: 0,
    };

    let _listener = stream
        .add_local_listener_with_user_data(user_data)
        .state_changed(|_, _data, _old, new| {
            use pw::stream::StreamState;
            match new {
                StreamState::Streaming => info!("pipewire source: streaming"),
                StreamState::Paused => info!("pipewire source: paused"),
                StreamState::Error(_) => warn!("pipewire source: error state"),
                _ => {}
            }
        })
        .param_changed(|_, _data, id, param| {
            // We advertise a single fixed format, so any negotiated Format
            // param equals what we set up; nothing to reconfigure.
            if id == pw::spa::param::ParamType::Format.as_raw() && param.is_some() {
                info!("pipewire source: format set");
            }
        })
        .process(|stream, data| {
            let Some(mut buffer) = stream.dequeue_buffer() else {
                return;
            };
            let datas = buffer.datas_mut();
            if datas.is_empty() {
                return;
            }
            let data_buf = &mut datas[0];
            let stride = (data.w * STRIDE_PER_PIXEL) as usize;
            let n = stride * data.h as usize;
            let Some(dst) = data_buf.data() else { return };
            if dst.len() < n {
                warn!(len = dst.len(), expected = n, "pw buffer too small");
                return;
            }
            match data.slot.lock().unwrap().take() {
                Some(bytes) => {
                    dst[..n].copy_from_slice(&bytes[..n]);
                    data.delivered += 1;
                }
                None => {
                    // No fresh frame since the last tick. The buffer still
                    // holds the previously delivered frame, so repeat it;
                    // before the very first frame, show black.
                    if data.delivered == 0 {
                        dst[..n].fill(0);
                    }
                }
            }
            let chunk = data_buf.chunk_mut();
            *chunk.offset_mut() = 0;
            *chunk.stride_mut() = stride as i32;
            *chunk.size_mut() = n as u32;
        })
        .register()
        .map_err(|e| anyhow!("pw listener: {e}"))?;

    // Fixed raw RGBA format at the negotiated size and framerate: no
    // choices, nothing for consumers to negotiate into a corner.
    let values = enum_format_bytes(w, h, num, den)?;
    let mut params = [pw::spa::pod::Pod::from_bytes(&values)
        .ok_or_else(|| anyhow!("enum-format pod deserialize"))?];
    stream
        .connect(
            spa::utils::Direction::Output,
            None,
            pw::stream::StreamFlags::AUTOCONNECT
                | pw::stream::StreamFlags::MAP_BUFFERS
                | pw::stream::StreamFlags::RT_PROCESS
                | pw::stream::StreamFlags::DRIVER,
            &mut params,
        )
        .map_err(|e| anyhow!("pw connect stream: {e}"))?;

    // DRIVE the graph from this thread at the negotiated framerate. The
    // v4l2 interval is time-per-frame (num/den seconds), so the tick is
    // num/den — at 30 fps that is 1/30 s ≈ 33 ms.
    let interval = Duration::from_secs_f64(num as f64 / den as f64);
    let driver = mainloop.loop_().add_timer(move |_expirations| {
        if let Err(e) = stream.trigger_process() {
            warn!(error = %e, "pw trigger_process failed");
        }
    });
    driver
        .update_timer(Some(interval), Some(interval))
        .into_result()
        .map_err(|e| anyhow!("pw timer: {e}"))?;

    // Watchdog: the only way this thread learns Drop happened. The
    // mainloop is Rc-backed and cannot cross into the closure, so quit it
    // through a weak handle upgraded inside the callback.
    let weak_mainloop = mainloop.downgrade();
    let watchdog = mainloop.loop_().add_timer(move |_expirations| {
        if !running_loop.load(Ordering::Acquire) {
            if let Some(ml) = weak_mainloop.upgrade() {
                ml.quit();
            }
        }
    });
    const WATCHDOG_TICK: Duration = Duration::from_millis(250);
    watchdog
        .update_timer(Some(WATCHDOG_TICK), Some(WATCHDOG_TICK))
        .into_result()
        .map_err(|e| anyhow!("pw watchdog timer: {e}"))?;

    mainloop.run();
    info!("pipewire source thread exiting");
    Ok(())
}

/// Serialize the SPA "EnumFormat" object for raw video RGBA w×h@num/den.
/// Returns the raw pod bytes; the caller materializes the `Pod` view over
/// them (`Pod` borrows its memory).
fn enum_format_bytes(w: u32, h: u32, num: u32, den: u32) -> Result<Vec<u8>> {
    use pw::spa::utils::{Fraction, Rectangle};
    let obj = pw::spa::pod::object!(
        pw::spa::utils::SpaTypes::ObjectParamFormat,
        pw::spa::param::ParamType::EnumFormat,
        pw::spa::pod::property!(
            pw::spa::param::format::FormatProperties::MediaType,
            Id,
            pw::spa::param::format::MediaType::Video
        ),
        pw::spa::pod::property!(
            pw::spa::param::format::FormatProperties::MediaSubtype,
            Id,
            pw::spa::param::format::MediaSubtype::Raw
        ),
        pw::spa::pod::property!(
            pw::spa::param::format::FormatProperties::VideoFormat,
            Choice,
            Enum,
            Id,
            pw::spa::param::video::VideoFormat::RGBA,
            pw::spa::param::video::VideoFormat::RGBx,
            pw::spa::param::video::VideoFormat::BGRx
        ),
        pw::spa::pod::property!(
            pw::spa::param::format::FormatProperties::VideoSize,
            Choice,
            Range,
            Rectangle,
            Rectangle { width: w, height: h },
            Rectangle { width: w, height: h },
            Rectangle { width: w, height: h }
        ),
        pw::spa::pod::property!(
            pw::spa::param::format::FormatProperties::VideoFramerate,
            Choice,
            Range,
            Fraction,
            // V4L2 reports time-per-frame (num/den seconds); pw wants the
            // framerate fraction: den/num frames per second.
            Fraction {
                num: den,
                denom: num
            },
            Fraction {
                num: den,
                denom: num
            },
            Fraction {
                num: den,
                denom: num
            }
        ),
    );
    let values: Vec<u8> = pw::spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &pw::spa::pod::Value::Object(obj),
    )
    .map_err(|e| anyhow!("serialize pod: {e:?}"))?
    .0
    .into_inner();
    Ok(values)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enum_format_pod_serializes() {
        // No pw daemon needed: this only exercises the pod serialization.
        let bytes = enum_format_bytes(1280, 720, 30, 1).unwrap();
        assert!(!bytes.is_empty());
        assert!(pw::spa::pod::Pod::from_bytes(&bytes).is_some());
    }
}
