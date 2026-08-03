use std::fs::{File, OpenOptions};
use std::io::{self, Read};
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::raw::{c_int, c_ulong};
use std::path::Path;
use std::time::Duration;

use crate::pwm::DigitalOutput;

const GPIO_MAX_NAME_SIZE: usize = 32;
const GPIO_V2_LINES_MAX: usize = 64;
const GPIO_V2_LINE_NUM_ATTRS_MAX: usize = 10;

const GPIO_V2_LINE_FLAG_OUTPUT: u64 = 1 << 3;
const GPIO_V2_LINE_FLAG_INPUT: u64 = 1 << 2;
const GPIO_V2_LINE_FLAG_EDGE_RISING: u64 = 1 << 4;
const GPIO_V2_LINE_FLAG_EDGE_FALLING: u64 = 1 << 5;
const GPIO_V2_LINE_ATTR_ID_OUTPUT_VALUES: u32 = 2;
const GPIO_V2_LINE_ATTR_ID_DEBOUNCE: u32 = 3;

const GPIO_V2_GET_LINE_IOCTL: c_ulong = iowr::<GpioV2LineRequest>(0xB4, 0x07);
const GPIO_V2_LINE_SET_VALUES_IOCTL: c_ulong = iowr::<GpioV2LineValues>(0xB4, 0x0F);
const GPIO_V2_LINE_GET_VALUES_IOCTL: c_ulong = iowr::<GpioV2LineValues>(0xB4, 0x0E);

const GPIO_V2_LINE_EVENT_RISING_EDGE: u32 = 1;
const GPIO_V2_LINE_EVENT_FALLING_EDGE: u32 = 2;

const POLLIN: i16 = 0x0001;
const POLLERR: i16 = 0x0008;
const POLLHUP: i16 = 0x0010;

const IOC_NRBITS: c_ulong = 8;
const IOC_TYPEBITS: c_ulong = 8;
const IOC_SIZEBITS: c_ulong = 14;

const IOC_NRSHIFT: c_ulong = 0;
const IOC_TYPESHIFT: c_ulong = IOC_NRSHIFT + IOC_NRBITS;
const IOC_SIZESHIFT: c_ulong = IOC_TYPESHIFT + IOC_TYPEBITS;
const IOC_DIRSHIFT: c_ulong = IOC_SIZESHIFT + IOC_SIZEBITS;

const IOC_WRITE: c_ulong = 1;
const IOC_READ: c_ulong = 2;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct GpioV2LineValues {
    bits: u64,
    mask: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct GpioV2LineAttribute {
    id: u32,
    padding: u32,
    value: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct GpioV2LineConfigAttribute {
    attr: GpioV2LineAttribute,
    mask: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct GpioV2LineConfig {
    flags: u64,
    num_attrs: u32,
    padding: [u32; 5],
    attrs: [GpioV2LineConfigAttribute; GPIO_V2_LINE_NUM_ATTRS_MAX],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct GpioV2LineRequest {
    offsets: [u32; GPIO_V2_LINES_MAX],
    consumer: [u8; GPIO_MAX_NAME_SIZE],
    config: GpioV2LineConfig,
    num_lines: u32,
    event_buffer_size: u32,
    padding: [u32; 5],
    fd: i32,
}

impl Default for GpioV2LineRequest {
    fn default() -> Self {
        Self {
            offsets: [0; GPIO_V2_LINES_MAX],
            consumer: [0; GPIO_MAX_NAME_SIZE],
            config: GpioV2LineConfig::default(),
            num_lines: 0,
            event_buffer_size: 0,
            padding: [0; 5],
            fd: -1,
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct GpioV2LineEvent {
    timestamp_ns: u64,
    id: u32,
    offset: u32,
    seqno: u32,
    line_seqno: u32,
    padding: [u32; 6],
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct PollFd {
    fd: c_int,
    events: i16,
    revents: i16,
}

unsafe extern "C" {
    fn ioctl(fd: c_int, request: c_ulong, ...) -> c_int;
    fn poll(fds: *mut PollFd, nfds: c_ulong, timeout: c_int) -> c_int;
}

#[derive(Debug)]
pub struct GpioLine {
    line: File,
}

impl GpioLine {
    pub fn request_output(
        chip_path: impl AsRef<Path>,
        offset: u32,
        initial_active: bool,
        consumer: &str,
    ) -> io::Result<Self> {
        let chip = OpenOptions::new().read(true).write(true).open(chip_path)?;
        let mut request = output_request(offset, initial_active, consumer);

        gpio_get_line(chip.as_raw_fd(), &mut request)?;

        if request.fd < 0 {
            return Err(io::Error::other(
                "GPIO line request returned invalid file descriptor",
            ));
        }

        let line = unsafe {
            // SAFETY: GPIO_V2_GET_LINE_IOCTL returned this file descriptor on success,
            // and this File becomes its sole owner for the lifetime of GpioLine.
            File::from_raw_fd(request.fd)
        };

        Ok(Self { line })
    }

    pub fn request_input_edges(
        chip_path: impl AsRef<Path>,
        offset: u32,
        debounce: Duration,
        consumer: &str,
    ) -> io::Result<Self> {
        let chip = OpenOptions::new().read(true).write(true).open(chip_path)?;
        let mut request = input_edges_request(offset, debounce, consumer);

        gpio_get_line(chip.as_raw_fd(), &mut request)?;

        if request.fd < 0 {
            return Err(io::Error::other(
                "GPIO line request returned invalid file descriptor",
            ));
        }

        let line = unsafe {
            // SAFETY: GPIO_V2_GET_LINE_IOCTL returned this file descriptor on success,
            // and this File becomes its sole owner for the lifetime of GpioLine.
            File::from_raw_fd(request.fd)
        };

        Ok(Self { line })
    }

    pub fn read_edge_event_timeout(
        &mut self,
        timeout: Duration,
    ) -> io::Result<Option<GpioEdgeEvent>> {
        if !poll_line(self.line.as_raw_fd(), timeout)? {
            return Ok(None);
        }

        let mut raw_event = GpioV2LineEvent::default();
        let event_buf = unsafe {
            // SAFETY: raw_event is a properly aligned initialized C-shaped buffer,
            // and the byte slice covers exactly its storage for kernel read().
            std::slice::from_raw_parts_mut(
                (&mut raw_event as *mut GpioV2LineEvent).cast::<u8>(),
                size_of::<GpioV2LineEvent>(),
            )
        };
        self.line.read_exact(event_buf)?;

        let kind = match raw_event.id {
            GPIO_V2_LINE_EVENT_RISING_EDGE => GpioEdgeKind::Rising,
            GPIO_V2_LINE_EVENT_FALLING_EDGE => GpioEdgeKind::Falling,
            id => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("unknown GPIO edge event id {id}"),
                ));
            }
        };

        Ok(Some(GpioEdgeEvent {
            kind,
            timestamp_ns: raw_event.timestamp_ns,
        }))
    }

    pub fn read_value(&mut self) -> io::Result<bool> {
        let mut values = GpioV2LineValues { bits: 0, mask: 1 };
        gpio_get_line_values(self.line.as_raw_fd(), &mut values)?;
        Ok(values.bits & 1 != 0)
    }
}

impl DigitalOutput for GpioLine {
    fn set_active(&mut self, active: bool) -> io::Result<()> {
        let mut values = GpioV2LineValues {
            bits: u64::from(active),
            mask: 1,
        };

        gpio_set_line_values(self.line.as_raw_fd(), &mut values)
    }
}

fn output_request(offset: u32, initial_active: bool, consumer: &str) -> GpioV2LineRequest {
    let mut request = GpioV2LineRequest::default();
    request.offsets[0] = offset;
    request.num_lines = 1;
    copy_consumer_label(&mut request.consumer, consumer);

    request.config.flags = GPIO_V2_LINE_FLAG_OUTPUT;
    request.config.num_attrs = 1;
    request.config.attrs[0].attr.id = GPIO_V2_LINE_ATTR_ID_OUTPUT_VALUES;
    request.config.attrs[0].attr.value = u64::from(initial_active);
    request.config.attrs[0].mask = 1;

    request
}

fn input_edges_request(offset: u32, debounce: Duration, consumer: &str) -> GpioV2LineRequest {
    let mut request = GpioV2LineRequest::default();
    request.offsets[0] = offset;
    request.num_lines = 1;
    request.event_buffer_size = 16;
    copy_consumer_label(&mut request.consumer, consumer);

    request.config.flags =
        GPIO_V2_LINE_FLAG_INPUT | GPIO_V2_LINE_FLAG_EDGE_RISING | GPIO_V2_LINE_FLAG_EDGE_FALLING;

    let debounce_us = debounce.as_micros().min(u128::from(u64::MAX)) as u64;
    if debounce_us > 0 {
        request.config.num_attrs = 1;
        request.config.attrs[0].attr.id = GPIO_V2_LINE_ATTR_ID_DEBOUNCE;
        request.config.attrs[0].attr.value = debounce_us;
        request.config.attrs[0].mask = 1;
    }

    request
}

fn copy_consumer_label(buf: &mut [u8; GPIO_MAX_NAME_SIZE], consumer: &str) {
    let bytes = consumer.as_bytes();
    let len = bytes.len().min(GPIO_MAX_NAME_SIZE - 1);
    buf[..len].copy_from_slice(&bytes[..len]);
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct GpioEdgeEvent {
    pub kind: GpioEdgeKind,
    pub timestamp_ns: u64,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum GpioEdgeKind {
    Rising,
    Falling,
}

fn poll_line(fd: RawFd, timeout: Duration) -> io::Result<bool> {
    let timeout_ms = timeout.as_millis().min(c_int::MAX as u128) as c_int;
    let mut pollfd = PollFd {
        fd,
        events: POLLIN,
        revents: 0,
    };
    let rc = unsafe {
        // SAFETY: pollfd points to one valid pollfd-shaped buffer for this call.
        poll(&mut pollfd, 1, timeout_ms)
    };

    if rc < 0 {
        return Err(io::Error::last_os_error());
    }

    if rc == 0 {
        return Ok(false);
    }

    if pollfd.revents & (POLLERR | POLLHUP) != 0 {
        return Err(io::Error::other(format!(
            "GPIO line poll returned error flags 0x{:x}",
            pollfd.revents
        )));
    }

    Ok(pollfd.revents & POLLIN != 0)
}

fn gpio_get_line(chip_fd: RawFd, request: &mut GpioV2LineRequest) -> io::Result<()> {
    let rc = unsafe {
        // SAFETY: chip_fd is an open GPIO chip fd, request points to a valid
        // gpio_v2_line_request-shaped buffer, and the kernel initializes request.fd.
        ioctl(chip_fd, GPIO_V2_GET_LINE_IOCTL, request)
    };
    ioctl_result(rc)
}

fn gpio_set_line_values(line_fd: RawFd, values: &mut GpioV2LineValues) -> io::Result<()> {
    let rc = unsafe {
        // SAFETY: line_fd is a GPIO line request fd and values points to a valid
        // gpio_v2_line_values-shaped buffer for the single requested output line.
        ioctl(line_fd, GPIO_V2_LINE_SET_VALUES_IOCTL, values)
    };
    ioctl_result(rc)
}

fn gpio_get_line_values(line_fd: RawFd, values: &mut GpioV2LineValues) -> io::Result<()> {
    let rc = unsafe {
        // SAFETY: line_fd is a GPIO line request fd and values points to a valid
        // gpio_v2_line_values-shaped buffer for the single requested line.
        ioctl(line_fd, GPIO_V2_LINE_GET_VALUES_IOCTL, values)
    };
    ioctl_result(rc)
}

fn ioctl_result(rc: c_int) -> io::Result<()> {
    if rc < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

const fn iowr<T>(ty: c_ulong, nr: c_ulong) -> c_ulong {
    ioc(IOC_READ | IOC_WRITE, ty, nr, size_of::<T>() as c_ulong)
}

const fn ioc(dir: c_ulong, ty: c_ulong, nr: c_ulong, size: c_ulong) -> c_ulong {
    (dir << IOC_DIRSHIFT) | (ty << IOC_TYPESHIFT) | (nr << IOC_NRSHIFT) | (size << IOC_SIZESHIFT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_single_output_request() {
        let request = output_request(27, false, "radxa-penta-top-hat-rs");

        assert_eq!(request.offsets[0], 27);
        assert_eq!(request.num_lines, 1);
        assert_eq!(request.config.flags, GPIO_V2_LINE_FLAG_OUTPUT);
        assert_eq!(request.config.num_attrs, 1);
        assert_eq!(
            request.config.attrs[0].attr.id,
            GPIO_V2_LINE_ATTR_ID_OUTPUT_VALUES
        );
        assert_eq!(request.config.attrs[0].attr.value, 0);
        assert_eq!(request.config.attrs[0].mask, 1);
        assert_eq!(request.consumer[0], b'r');
    }

    #[test]
    fn builds_single_input_edges_request() {
        let request = input_edges_request(17, Duration::from_millis(10), "radxa-penta-top-hat-rs");

        assert_eq!(request.offsets[0], 17);
        assert_eq!(request.num_lines, 1);
        assert_eq!(request.event_buffer_size, 16);
        assert_eq!(
            request.config.flags,
            GPIO_V2_LINE_FLAG_INPUT
                | GPIO_V2_LINE_FLAG_EDGE_RISING
                | GPIO_V2_LINE_FLAG_EDGE_FALLING
        );
        assert_eq!(request.config.num_attrs, 1);
        assert_eq!(
            request.config.attrs[0].attr.id,
            GPIO_V2_LINE_ATTR_ID_DEBOUNCE
        );
        assert_eq!(request.config.attrs[0].attr.value, 10_000);
        assert_eq!(request.config.attrs[0].mask, 1);
    }

    #[test]
    fn truncates_consumer_label_with_nul_room() {
        let mut buf = [0u8; GPIO_MAX_NAME_SIZE];
        copy_consumer_label(&mut buf, "abcdefghijklmnopqrstuvwxyz0123456789");

        assert_eq!(buf[30], b'4');
        assert_eq!(buf[31], 0);
    }
}
