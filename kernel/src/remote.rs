//! A renderer in another task.
//!
//! The compositor draws through the `Renderer` trait and does not know what
//! is behind it. This is what is behind it when a display driver has been
//! started: every call becomes an IPC call to that driver, with the bulk of
//! it -- command records, coverage for the atlas, rectangles to show --
//! written into memory the two share.
//!
//! That is the design the render crate was shaped for from the start.
//! Commands are plain data so that a command list can leave the process that
//! recorded it; [`corrode_render::wire`] is the format they leave in. The
//! driver decodes them and draws with the same software renderer the kernel
//! would have used, into memory its device scans out, and so the frame on
//! the glass is the same frame. What changed is who owns the device.
//!
//! # Trust runs one way
//!
//! The kernel trusts the driver with nothing it did not hand it. The shared
//! memory is the kernel's to write and the driver's to read; what comes
//! back is a status word. A driver that lies about having drawn draws a
//! wrong picture, which is its own business, and a driver that never
//! answers stalls the compositor -- the known gap of every synchronous call,
//! met on the frame path.

use corrode_abi::gpu;
use corrode_render::damage::Damage;
use corrode_render::{wire, CommandList, PixelFormat, Rect, Renderer, SurfaceInfo};

use crate::cap::{CapError, CapPtr, Rights};
use crate::ipc::{self, Message};
use crate::memory;

/// Why a remote call did not happen, or did and failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// The call itself: no endpoint, refused, and so on.
    Ipc(CapError),
    /// The driver answered with an error code.
    Driver(u64),
    /// More than fits in shared memory in one call.
    TooBig,
}

pub struct RemoteRenderer {
    /// Bootstrap's send capability on the driver's endpoint.
    endpoint: CapPtr,
    shared: *mut u8,
    shared_len: usize,
    info: SurfaceInfo,
    /// How many displays the device said it has.
    pub displays: u64,
}

// SAFETY: the shared mapping is only written by the task holding this
// renderer, and only between calls, while the driver is blocked waiting for
// the next one; the driver never writes it at all.
unsafe impl Send for RemoteRenderer {}

impl RemoteRenderer {
    /// Ask the driver behind `endpoint` to set its device up, and wrap the
    /// answer. `shared` is the memory object both sides hold, mapped in the
    /// kernel when it was made.
    pub fn connect(endpoint: CapPtr, shared: usize) -> Result<Self, Error> {
        let (base, len) = memory::in_kernel(shared).ok_or(Error::TooBig)?;
        let answer = call(endpoint, Message::from_word(gpu::INIT, 0), None)?;
        let [width, height, pitch, displays] = answer;
        if width == 0 || height == 0 || pitch < width * 4 {
            return Err(Error::Driver(0));
        }
        Ok(Self {
            endpoint,
            shared: base,
            shared_len: len,
            info: SurfaceInfo {
                width: width as u32,
                height: height as u32,
                pitch: pitch as u32,
                format: PixelFormat::XRGB8888,
            },
            displays,
        })
    }

    /// Hand the driver a surface to draw `Command::Image` from: a read-only
    /// copy of the capability in `slot`, attached to the call.
    pub fn attach_surface(&mut self, slot: CapPtr, surface: u32) -> Result<(), Error> {
        call(
            self.endpoint,
            Message::from_word(gpu::SURFACE, surface as u64),
            Some((slot, Rights::READ)),
        )
        .map(|_| ())
    }

    /// The shared memory, as bytes.
    fn shared(&mut self) -> &mut [u8] {
        // SAFETY: `memory::create` mapped `shared_len` bytes writable at
        // `shared` in the kernel half, for good, and nothing else in the
        // kernel holds a view of them. See the `Send` comment for the driver.
        unsafe { core::slice::from_raw_parts_mut(self.shared, self.shared_len) }
    }
}

/// Call the driver and read its answer: a zero label is success, and the
/// four words are whatever it returned.
fn call(
    endpoint: CapPtr,
    message: Message,
    transfer: Option<(CapPtr, Rights)>,
) -> Result<[u64; 4], Error> {
    let answer = ipc::call(endpoint, message, transfer, None).map_err(Error::Ipc)?;
    match answer.message.label {
        0 => Ok(answer.message.words),
        code => Err(Error::Driver(code)),
    }
}

impl Renderer for RemoteRenderer {
    type Error = Error;

    fn surface_info(&self) -> SurfaceInfo {
        self.info
    }

    fn submit(&mut self, list: &CommandList<'_>) -> Result<(), Error> {
        let commands = list.commands();
        if commands.len() * wire::RECORD > self.shared_len {
            return Err(Error::TooBig);
        }
        let shared = self.shared();
        let records = shared.as_chunks_mut::<{ wire::RECORD }>().0;
        for (command, record) in commands.iter().zip(records) {
            wire::encode(command, record);
        }
        call(
            self.endpoint,
            Message::from_word(gpu::SUBMIT, commands.len() as u64),
            None,
        )
        .map(|_| ())
    }

    fn upload_mask(&mut self, at: Rect, coverage: &[u8]) -> Result<(), Error> {
        if coverage.len() > self.shared_len {
            return Err(Error::TooBig);
        }
        self.shared()[..coverage.len()].copy_from_slice(coverage);
        call(
            self.endpoint,
            Message {
                label: gpu::UPLOAD,
                words: [at.x as u64, at.y as u64, at.width as u64, at.height as u64],
            },
            None,
        )
        .map(|_| ())
    }

    fn present(&mut self, damage: &Damage) -> Result<(), Error> {
        let rects = damage.rects();
        if rects.is_empty() {
            return Ok(());
        }
        let shared = self.shared();
        for (rect, bytes) in rects.iter().zip(shared.as_chunks_mut::<{ wire::RECT }>().0) {
            wire::put_rect(rect, bytes);
        }
        call(
            self.endpoint,
            Message::from_word(gpu::PRESENT, rects.len() as u64),
            None,
        )
        .map(|_| ())
    }
}
