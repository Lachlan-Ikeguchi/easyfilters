//! Thin V4L2 wrapper over `v4l2r`'s ioctl layer: device discovery, format
//! negotiation, MMAP buffer queues, qbuf/dqbuf and poll.

use std::fs::File;
use std::os::unix::io::{AsFd, AsRawFd, FromRawFd};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use v4l2r::ioctl::{
    self, Capability, Capabilities, FmtDesc, MemoryConsistency, PlaneMapping, QueryBuffer,
    V4l2Buffer,
};
use v4l2r::memory::MemoryType;
use v4l2r::{bindings, nix, Format, PixelFormat, PlaneLayout, QueueType};

/// One MMAP buffer of a queue.
pub struct Buffer {
    pub index: u32,
    pub offset: u32,
    pub length: u32,
    pub mapping: PlaneMapping,
}

/// A dequeued buffer.
pub struct Dequeued {
    pub index: u32,
    pub bytesused: u32,
    pub flags: u32,
    pub timestamp: bindings::timeval,
}

/// An opened V4L2 device.
pub struct Device {
    file: File,
    path: PathBuf,
    caps: Capability,
}

impl Device {
    pub fn open(path: &Path, non_blocking: bool) -> Result<Self> {
        let mut flags = nix::fcntl::OFlag::O_RDWR | nix::fcntl::OFlag::O_CLOEXEC;
        if non_blocking {
            flags |= nix::fcntl::OFlag::O_NONBLOCK;
        }
        let fd = nix::fcntl::open(path, flags, nix::sys::stat::Mode::empty())
            .with_context(|| format!("open {}", path.display()))?;
        // SAFETY: fd was just opened and is owned by us.
        let file = unsafe { File::from_raw_fd(fd) };
        let caps = ioctl::querycap::<Capability>(&file).context("VIDIOC_QUERYCAP")?;
        Ok(Self {
            file,
            path: path.to_path_buf(),
            caps,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
    pub fn driver(&self) -> &str {
        &self.caps.driver
    }
    pub fn card(&self) -> &str {
        &self.caps.card
    }
    pub fn device_caps(&self) -> Capabilities {
        self.caps.device_caps()
    }
    pub fn file(&self) -> &File {
        &self.file
    }

    /// Enumerate the pixel formats of a queue (VIDIOC_ENUM_FMT).
    pub fn enum_formats(&self, queue: QueueType) -> Vec<FmtDesc> {
        let mut out = Vec::new();
        for index in 0..u32::MAX {
            match ioctl::enum_fmt::<FmtDesc>(&self.file, queue, index) {
                Ok(desc) => out.push(desc),
                Err(_) => break,
            }
        }
        out
    }

    /// Discrete frame sizes (width, height) for a pixel format.
    pub fn frame_sizes(&self, pf: PixelFormat) -> Vec<(u32, u32)> {
        let mut out = Vec::new();
        for index in 0..u32::MAX {
            match ioctl::enum_frame_sizes::<bindings::v4l2_frmsizeenum>(&self.file, index, pf) {
                Ok(s) => {
                    if s.type_ == bindings::v4l2_frmsizetypes_V4L2_FRMSIZE_TYPE_DISCRETE as u32 {
                        // SAFETY: type_ says this union member is valid.
                        let d = unsafe { s.__bindgen_anon_1.discrete };
                        out.push((d.width, d.height));
                    }
                }
                Err(_) => break,
            }
        }
        out
    }

    /// Discrete frame intervals as (numerator, denominator) of one second.
    pub fn frame_intervals(&self, pf: PixelFormat, width: u32, height: u32) -> Vec<(u32, u32)> {
        let mut out = Vec::new();
        for index in 0..u32::MAX {
            match ioctl::enum_frame_intervals::<bindings::v4l2_frmivalenum>(
                &self.file,
                index,
                pf,
                width,
                height,
            ) {
                Ok(iv) => {
                    if iv.type_ == bindings::v4l2_frmivaltypes_V4L2_FRMIVAL_TYPE_DISCRETE as u32 {
                        // SAFETY: type_ says this union member is valid.
                        let d = unsafe { iv.__bindgen_anon_1.discrete };
                        out.push((d.numerator, d.denominator));
                    }
                }
                Err(_) => break,
            }
        }
        out
    }

    /// Set the format of a queue; returns the format actually applied.
    /// `sizeimage` is passed through (0 = let the driver decide, useful for
    /// capture; set explicitly for compressed output formats).
    pub fn set_format(
        &self,
        queue: QueueType,
        width: u32,
        height: u32,
        pf: PixelFormat,
        sizeimage: u32,
    ) -> Result<Format> {
        let format = Format {
            width,
            height,
            pixelformat: pf,
            plane_fmt: vec![PlaneLayout {
                sizeimage,
                bytesperline: 0,
            }],
        };
        // s_fmt takes &mut because it may adjust the format in place.
        let mut file = self.file.try_clone()?;
        ioctl::s_fmt::<_, Format>(&mut file, (queue, &format))
            .map_err(|e| anyhow!("VIDIOC_S_FMT {:?}: {:?}", queue, e))
    }

    /// Set the frame interval (timeperframe) of a queue, e.g. (1, 30) for 30 fps.
    pub fn set_fps(&self, queue: QueueType, numerator: u32, denominator: u32) -> Result<()> {
        let mut parm: bindings::v4l2_streamparm = Default::default();
        parm.type_ = queue as u32;
        parm.parm.capture = bindings::v4l2_captureparm {
            capability: bindings::V4L2_CAP_TIMEPERFRAME,
            timeperframe: bindings::v4l2_fract {
                numerator,
                denominator,
            },
            ..Default::default()
        };
        ioctl::s_parm::<_, bindings::v4l2_streamparm>(&self.file, parm)
            .map_err(|e| anyhow!("VIDIOC_S_PARM {:?}: {:?}", queue, e))?;
        Ok(())
    }

    /// Allocate `count` MMAP buffers on a queue.
    pub fn queue(&self, queue_type: QueueType, count: u32) -> Result<Queue> {
        Queue::new(self.file.try_clone()?, queue_type, count)
    }

    /// poll() for readability (a frame is ready to dequeue on capture queues).
    pub fn poll(&self, timeout: Duration) -> Result<bool> {
        let mut fds = [nix::poll::PollFd::new(
            self.file.as_fd(),
            nix::poll::PollFlags::POLLIN,
        )];
        let ms = u32::try_from(timeout.as_millis()).unwrap_or(u32::MAX);
        let timeout = nix::poll::PollTimeout::try_from(ms).map_err(|e| anyhow!("poll timeout: {:?}", e))?;
        // EINTR just means a signal (e.g. SIGTERM) arrived; report "not ready"
        // and let the caller check its shutdown flag.
        match nix::poll::poll(&mut fds, timeout) {
            Ok(n) => Ok(n > 0),
            Err(nix::errno::Errno::EINTR) => Ok(false),
            Err(e) => Err(anyhow!("poll: {:?}", e)),
        }
    }
}

// Raw VIDIOC_DQBUF: v4l2r's ioctl::dqbuf leaves `memory` unset, which the
// kernel rejects with EINVAL ("set the type, memory and reserved fields").
nix::ioctl_readwrite!(vidioc_dqbuf_raw, b'V', 17, bindings::v4l2_buffer);

/// A V4L2 queue with its MMAP buffers allocated.
pub struct Queue {
    file: File,
    queue_type: QueueType,
    pub buffers: Vec<Buffer>,
}

impl Queue {
    fn new(file: File, queue_type: QueueType, count: u32) -> Result<Self> {
        let req = ioctl::reqbufs::<bindings::v4l2_requestbuffers>(
            &file,
            queue_type,
            MemoryType::Mmap,
            count,
            MemoryConsistency::empty(),
        )
        .map_err(|e| anyhow!("VIDIOC_REQBUFS {:?}: {:?}", queue_type, e))?;
        let n = req.count;
        let mut buffers = Vec::with_capacity(n as usize);
        for i in 0..n as usize {
            let qb = ioctl::querybuf::<QueryBuffer>(&file, queue_type, i)
                .map_err(|e| anyhow!("VIDIOC_QUERYBUF {}: {:?}", i, e))?;
            let plane = &qb.planes[0];
            let mapping = ioctl::mmap(&file, plane.mem_offset, plane.length)
                .map_err(|e| anyhow!("mmap buffer {}: {:?}", i, e))?;
            buffers.push(Buffer {
                index: i as u32,
                offset: plane.mem_offset,
                length: plane.length,
                mapping,
            });
        }
        Ok(Self {
            file,
            queue_type,
            buffers,
        })
    }

    pub fn stream_on(&self) -> Result<()> {
        ioctl::streamon(&self.file, self.queue_type)
            .map_err(|e| anyhow!("VIDIOC_STREAMON {:?}: {:?}", self.queue_type, e))
    }

    pub fn stream_off(&self) -> Result<()> {
        ioctl::streamoff(&self.file, self.queue_type)
            .map_err(|e| anyhow!("VIDIOC_STREAMOFF {:?}: {:?}", self.queue_type, e))
    }

    /// Free all buffers (REQBUFS with count 0; also implicitly streamoffs).
    pub fn free(self) -> Result<()> {
        ioctl::reqbufs::<bindings::v4l2_requestbuffers>(
            &self.file,
            self.queue_type,
            MemoryType::Mmap,
            0,
            MemoryConsistency::empty(),
        )
        .map_err(|e| anyhow!("VIDIOC_REQBUFS(0) {:?}: {:?}", self.queue_type, e))?;
        Ok(())
    }

    /// Queue an MMAP buffer. For capture queues the driver fills it and
    /// `bytesused` is ignored; for output queues it is the frame size.
    pub fn qbuf(&self, index: u32, bytesused: u32, ts: bindings::timeval) -> Result<()> {
        let b = &self.buffers[index as usize];
        let mut buf = V4l2Buffer::new(self.queue_type, index, MemoryType::Mmap);
        // SAFETY: we own `buf` and set only single-plane MMAP fields.
        unsafe {
            let p = buf.as_mut_ptr();
            (*p).length = b.length;
            (*p).m.offset = b.offset;
            (*p).bytesused = bytesused;
            (*p).timestamp = ts;
        }
        ioctl::qbuf::<_, ()>(&self.file, buf)
            .map_err(|e| anyhow!("VIDIOC_QBUF {}: {:?}", index, e))
    }

    /// Dequeue a buffer; `Ok(None)` on EAGAIN/EPIPE (non-blocking fd, nothing
    /// ready or end-of-stream).
    pub fn try_dqbuf(&self) -> Result<Option<Dequeued>> {
        let mut raw: bindings::v4l2_buffer = Default::default();
        raw.type_ = self.queue_type as u32;
        raw.memory = bindings::v4l2_memory_V4L2_MEMORY_MMAP as u32;
        // SAFETY: fd is valid and raw is a properly initialized v4l2_buffer.
        match unsafe { vidioc_dqbuf_raw(self.file.as_raw_fd(), &mut raw) } {
            Ok(_) => Ok(Some(Dequeued {
                index: raw.index,
                bytesused: raw.bytesused,
                flags: raw.flags,
                timestamp: raw.timestamp,
            })),
            Err(nix::errno::Errno::EAGAIN) | Err(nix::errno::Errno::EPIPE) => Ok(None),
            Err(e) => Err(anyhow!("VIDIOC_DQBUF {:?}: {:?}", self.queue_type, e)),
        }
    }
}
