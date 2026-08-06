use std::{
    io::{self, Read, Write},
    sync::mpsc::{self, Receiver, Sender, TryRecvError},
    thread::{self, JoinHandle},
    time::Duration,
};

use serialport::{DataBits, Parity, SerialPort, StopBits};

pub enum WorkerEvent {
    Received(Vec<u8>),
    Closed(String),
    Error(String),
}

enum WorkerCommand {
    SendBatch(Vec<Vec<u8>>),
    SetDtr(bool),
    SetRts(bool),
    Close,
}

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
        let port = serialport::new(port_name, baud_rate)
            .data_bits(data_bits)
            .stop_bits(stop_bits)
            .parity(parity)
            .timeout(Duration::from_millis(100))
            .open()
            .map_err(|err| format!("打开串口失败：{err}"))?;

        let (cmd_tx, cmd_rx) = mpsc::channel();
        let (event_tx, event_rx) = mpsc::channel();
        let thread_name = format!("serial-worker-{port_name}");

        let handle = thread::Builder::new()
            .name(thread_name)
            .spawn(move || worker_loop(port, cmd_rx, event_tx))
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

fn worker_loop(
    mut port: Box<dyn SerialPort>,
    cmd_rx: Receiver<WorkerCommand>,
    event_tx: Sender<WorkerEvent>,
) {
    let mut buffer = [0_u8; 4096];

    loop {
        match cmd_rx.try_recv() {
            Ok(WorkerCommand::SendBatch(batch)) => {
                for payload in batch {
                    if let Err(err) = port.write_all(&payload).and_then(|_| port.flush()) {
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
    }
}
