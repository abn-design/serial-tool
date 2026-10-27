use std::{
    io::{self, Read},
    sync::mpsc::{self, Receiver, Sender, TryRecvError},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use serialport::{ClearBuffer, DataBits, FlowControl, Parity, SerialPort, StopBits};

/// 读超时。
///
/// 注意 serialport 4.9 只有一个 timeout，它同时被用作写超时（Windows 上是
/// `WriteTotalTimeoutConstant`），因此长数据必须分片写，见 [`write_payload`]。
const READ_TIMEOUT: Duration = Duration::from_millis(100);

/// 线路错误统计的轮询间隔
const LINE_ERROR_POLL_INTERVAL: Duration = Duration::from_millis(250);

pub enum WorkerEvent {
    Received(Vec<u8>),
    /// 自打开端口以来累计的线路错误计数（仅在计数变化时发送）
    LineError(LineErrorCounts),
    Closed(String),
    Error(String),
}

/// 自打开端口以来累计的线路错误计数。
///
/// Windows 通过 `ClearCommError` 的 CE_* 标志累计（`ClearCommError` 会清标志，
/// 因此 250ms 轮询间隔内的一次或多次错误只记 1 次）；Linux 通过 `TIOCGICOUNT`
/// 读取驱动计数器并减去打开时的基线（精确计数）。其他平台（如 macOS）不提供统计。
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct LineErrorCounts {
    pub framing: u64,
    pub parity: u64,
    pub overrun: u64,
}

impl LineErrorCounts {
    pub fn is_zero(&self) -> bool {
        *self == Self::default()
    }
}

enum WorkerCommand {
    SendBatch(Vec<Vec<u8>>),
    SetDtr(bool),
    SetRts(bool),
    Close,
}

/// 传给后台线程的原生句柄。
///
/// Windows 的 `RawHandle` 是裸指针（不是 `Send`），这里存成整数再在调用前转回指针。
#[cfg(windows)]
type PortHandle = usize;
#[cfg(unix)]
type PortHandle = std::os::unix::io::RawFd;

pub struct SerialConnection {
    cmd_tx: Sender<WorkerCommand>,
    event_rx: Receiver<WorkerEvent>,
    handle: Option<JoinHandle<()>>,
}

impl SerialConnection {
    pub fn open(
        port_name: &str,
        baud_rate: u32,
        data_bits: DataBits,
        stop_bits: StopBits,
        parity: Parity,
    ) -> Result<Self, String> {
        let builder = serialport::new(port_name, baud_rate)
            .data_bits(data_bits)
            .stop_bits(stop_bits)
            .parity(parity)
            // 本工具不提供流控功能，显式关闭，避免依赖底层默认值
            .flow_control(FlowControl::None)
            .timeout(READ_TIMEOUT);

        let (port, port_handle) = open_native(&builder)?;

        // 打开前设备可能已经发出一段数据（尤其是持续发送的对端），
        // 这里先清空收发缓冲，避免日志开头混入旧字节。
        let _ = port.clear(ClearBuffer::All);

        let (cmd_tx, cmd_rx) = mpsc::channel();
        let (event_tx, event_rx) = mpsc::channel();
        let thread_name = format!("serial-worker-{port_name}");

        let handle = thread::Builder::new()
            .name(thread_name)
            .spawn(move || worker_loop(port, port_handle, baud_rate, cmd_rx, event_tx))
            .map_err(|err| format!("启动串口后台线程失败：{err}"))?;

        Ok(Self {
            cmd_tx,
            event_rx,
            handle: Some(handle),
        })
    }

    pub fn send_bytes(&self, payloads: Vec<Vec<u8>>) -> Result<(), String> {
        self.cmd_tx
            .send(WorkerCommand::SendBatch(payloads))
            .map_err(|_| "串口后台线程已退出".to_owned())
    }

    pub fn set_dtr(&self, level: bool) -> Result<(), String> {
        self.cmd_tx
            .send(WorkerCommand::SetDtr(level))
            .map_err(|_| "串口后台线程已退出".to_owned())
    }

    pub fn set_rts(&self, level: bool) -> Result<(), String> {
        self.cmd_tx
            .send(WorkerCommand::SetRts(level))
            .map_err(|_| "串口后台线程已退出".to_owned())
    }

    pub fn try_recv(&self) -> Result<WorkerEvent, TryRecvError> {
        self.event_rx.try_recv()
    }
}

impl Drop for SerialConnection {
    fn drop(&mut self) {
        let _ = self.cmd_tx.send(WorkerCommand::Close);

        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// 打开平台原生串口，同时取出原始 fd / handle 供线路错误统计使用。
///
/// `SerialPortBuilder::open` 只返回 `Box<dyn SerialPort>`，而 `SerialPort` trait
/// 不暴露原生句柄，所以这里直接使用平台相关的 `TTYPort` / `COMPort`。
#[cfg(unix)]
fn open_native(
    builder: &serialport::SerialPortBuilder,
) -> Result<(Box<dyn SerialPort>, PortHandle), String> {
    use std::os::unix::io::AsRawFd;

    let port = serialport::TTYPort::open(builder).map_err(|err| format!("打开串口失败：{err}"))?;
    let handle = port.as_raw_fd();
    Ok((Box::new(port), handle))
}

#[cfg(windows)]
fn open_native(
    builder: &serialport::SerialPortBuilder,
) -> Result<(Box<dyn SerialPort>, PortHandle), String> {
    use std::os::windows::io::AsRawHandle;

    let port = serialport::COMPort::open(builder).map_err(|err| format!("打开串口失败：{err}"))?;
    let handle = port.as_raw_handle() as usize;
    Ok((Box::new(port), handle))
}

fn worker_loop(
    mut port: Box<dyn SerialPort>,
    port_handle: PortHandle,
    baud_rate: u32,
    cmd_rx: Receiver<WorkerCommand>,
    event_tx: Sender<WorkerEvent>,
) {
    let mut buffer = [0_u8; 4096];
    let mut error_probe = LineErrorProbe::new();
    let mut last_error_poll = Instant::now();

    loop {
        match cmd_rx.try_recv() {
            Ok(WorkerCommand::SendBatch(batch)) => {
                for payload in batch {
                    if let Err(err) = write_payload(port.as_mut(), &payload, baud_rate) {
                        let _ = event_tx.send(WorkerEvent::Error(format!("发送失败：{err}")));
                        return;
                    }
                }
            }
            Ok(WorkerCommand::SetDtr(level)) => {
                if let Err(err) = port.write_data_terminal_ready(level) {
                    let _ = event_tx.send(WorkerEvent::Error(format!("设置 DTR 失败：{err}")));
                    return;
                }
            }
            Ok(WorkerCommand::SetRts(level)) => {
                if let Err(err) = port.write_request_to_send(level) {
                    let _ = event_tx.send(WorkerEvent::Error(format!("设置 RTS 失败：{err}")));
                    return;
                }
            }
            Ok(WorkerCommand::Close) | Err(TryRecvError::Disconnected) => {
                let _ = event_tx.send(WorkerEvent::Closed("串口已关闭".to_owned()));
                return;
            }
            Err(TryRecvError::Empty) => {}
        }

        match port.read(&mut buffer) {
            Ok(size) if size > 0 => {
                let _ = event_tx.send(WorkerEvent::Received(buffer[..size].to_vec()));
            }
            Ok(_) => {}
            Err(err) if err.kind() == io::ErrorKind::TimedOut => {}
            Err(err) => {
                let _ = event_tx.send(WorkerEvent::Error(format!("接收失败：{err}")));
                return;
            }
        }

        if last_error_poll.elapsed() >= LINE_ERROR_POLL_INTERVAL {
            last_error_poll = Instant::now();

            if let Some(counts) = error_probe.poll(port_handle) {
                let _ = event_tx.send(WorkerEvent::LineError(counts));
            }
        }
    }
}

/// 分片写入单条数据。
///
/// serialport 4.9 的 timeout 同时是写超时，直接 `write_all` 一条长数据会超时
/// （9600 波特率下 100 ms 只够约 96 字节），已写入的部分还会被截断。
/// 这里按约 25 ms 的线路时间切片，并在每片后 flush，保证每片都能在超时内完成。
fn write_payload(port: &mut dyn SerialPort, payload: &[u8], baud_rate: u32) -> io::Result<()> {
    // 25 ms 线路时间对应的字节数（每字节 10 bit）
    let chunk_size = ((baud_rate as usize) / 400).clamp(8, 4096);

    for chunk in payload.chunks(chunk_size) {
        port.write_all(chunk)?;
        port.flush()?;
    }

    Ok(())
}

struct LineErrorProbe {
    totals: LineErrorCounts,
    #[cfg(target_os = "linux")]
    baseline: Option<LineErrorCounts>,
}

impl LineErrorProbe {
    fn new() -> Self {
        Self {
            totals: LineErrorCounts::default(),
            #[cfg(target_os = "linux")]
            baseline: None,
        }
    }

    /// 返回自打开端口以来的累计计数；计数未变化时返回 `None`。
    fn poll(&mut self, handle: PortHandle) -> Option<LineErrorCounts> {
        #[cfg(windows)]
        {
            let errors = read_comm_errors(handle)?;
            let previous = self.totals;

            if errors & CE_FRAME != 0 {
                self.totals.framing += 1;
            }
            if errors & CE_RXPARITY != 0 {
                self.totals.parity += 1;
            }
            if errors & (CE_OVERRUN | CE_RXOVER) != 0 {
                self.totals.overrun += 1;
            }

            (self.totals != previous).then_some(self.totals)
        }

        #[cfg(target_os = "linux")]
        {
            let absolute = read_icount(handle)?;
            let baseline = *self.baseline.get_or_insert(absolute);
            let totals = LineErrorCounts {
                framing: absolute.framing.saturating_sub(baseline.framing),
                parity: absolute.parity.saturating_sub(baseline.parity),
                overrun: absolute.overrun.saturating_sub(baseline.overrun),
            };

            if totals != self.totals {
                self.totals = totals;
                Some(totals)
            } else {
                None
            }
        }

        #[cfg(not(any(windows, target_os = "linux")))]
        {
            let _ = handle;
            None
        }
    }
}

#[cfg(windows)]
const CE_RXOVER: u32 = 0x0001;
#[cfg(windows)]
const CE_OVERRUN: u32 = 0x0002;
#[cfg(windows)]
const CE_RXPARITY: u32 = 0x0004;
#[cfg(windows)]
const CE_FRAME: u32 = 0x0008;

/// Win32 的 COMSTAT，`ClearCommError` 用它回传队列长度等信息。
#[cfg(windows)]
#[repr(C)]
#[derive(Default)]
struct ComStat {
    flags: u32,
    bytes_in_queue: u32,
    bytes_out_queue: u32,
}

#[cfg(windows)]
#[link(name = "kernel32")]
extern "system" {
    fn ClearCommError(file: *mut core::ffi::c_void, errors: *mut u32, stat: *mut ComStat) -> i32;
}

#[cfg(windows)]
fn read_comm_errors(handle: PortHandle) -> Option<u32> {
    let mut errors = 0_u32;
    let mut stat = ComStat::default();
    let ok = unsafe { ClearCommError(handle as *mut core::ffi::c_void, &mut errors, &mut stat) };

    (ok != 0).then_some(errors)
}

/// Linux `struct serial_icounter_struct`（libc 未提供该类型定义）。
#[cfg(target_os = "linux")]
#[repr(C)]
#[derive(Default, Clone, Copy)]
struct SerialIcounter {
    cts: i32,
    dsr: i32,
    rng: i32,
    dcd: i32,
    rx: i32,
    tx: i32,
    frame: i32,
    overrun: i32,
    parity: i32,
    brk: i32,
    buf_overrun: i32,
    reserved: [i32; 9],
}

#[cfg(target_os = "linux")]
fn read_icount(fd: PortHandle) -> Option<LineErrorCounts> {
    let mut counters = SerialIcounter::default();
    let result = unsafe {
        libc::ioctl(
            fd,
            libc::TIOCGICOUNT as libc::c_ulong,
            &mut counters as *mut SerialIcounter,
        )
    };

    // USB 转串口驱动通常没有实现 TIOCGICOUNT，失败时静默跳过
    if result != 0 {
        return None;
    }

    Some(LineErrorCounts {
        framing: counters.frame.max(0) as u64,
        parity: counters.parity.max(0) as u64,
        overrun: counters.overrun.max(0) as u64 + counters.buf_overrun.max(0) as u64,
    })
}

#[cfg(all(test, target_os = "linux"))]
mod pty_smoke {
    use super::*;
    use std::ffi::CStr;
    use std::os::unix::io::RawFd;

    fn open_pty() -> (RawFd, String) {
        unsafe {
            let master = libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY);
            assert!(master >= 0);
            assert_eq!(libc::grantpt(master), 0);
            assert_eq!(libc::unlockpt(master), 0);
            let mut buf = [0 as libc::c_char; 128];
            assert_eq!(libc::ptsname_r(master, buf.as_mut_ptr(), buf.len()), 0);
            let name = CStr::from_ptr(buf.as_ptr()).to_string_lossy().into_owned();
            (master, name)
        }
    }

    #[test]
    fn worker_roundtrip_over_pty() {
        let (master, slave_name) = open_pty();
        // 9600 下 `write_payload` 的分片大小是 24 字节，便于覆盖多片写入路径
        let conn = SerialConnection::open(
            &slave_name,
            9600,
            DataBits::Eight,
            StopBits::One,
            Parity::None,
        )
        .unwrap();

        let payload = [b'h', b'e', b'l', b'l', b'o', 0x00, 0xFF];
        let written = unsafe {
            libc::write(
                master,
                payload.as_ptr() as *const libc::c_void,
                payload.len(),
            )
        };
        assert_eq!(written, payload.len() as isize);

        let mut received = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(2);
        while received.len() < payload.len() && Instant::now() < deadline {
            match conn.try_recv() {
                Ok(WorkerEvent::Received(bytes)) => received.extend_from_slice(&bytes),
                Ok(_) => {}
                Err(TryRecvError::Empty) => thread::sleep(Duration::from_millis(5)),
                Err(_) => panic!("worker disconnected"),
            }
        }
        assert_eq!(received, payload);

        let outgoing: Vec<u8> = (0..300).map(|index| (index % 251) as u8).collect();
        conn.send_bytes(vec![outgoing.clone()]).unwrap();

        let mut out = vec![0_u8; outgoing.len()];
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut read_total = 0;
        while read_total < out.len() && Instant::now() < deadline {
            let n = unsafe {
                libc::read(
                    master,
                    out[read_total..].as_mut_ptr() as *mut libc::c_void,
                    out.len() - read_total,
                )
            };
            assert!(n > 0, "read failed");
            read_total += n as usize;
        }
        assert_eq!(out, outgoing);

        drop(conn);
        unsafe { libc::close(master) };
    }
}
