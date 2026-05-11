use std::{
    sync::mpsc::TryRecvError,
    time::{Duration, Instant},
};

use chrono::Local;
use eframe::{
    egui::{self, Color32, RichText, ScrollArea, TextEdit},
    CreationContext, Frame,
};

use crate::serial_worker::{SerialConnection, WorkerEvent};

const MAX_LOG_ENTRIES: usize = 1000;
const DEFAULT_BAUD_RATE: u32 = 115200;
const DEFAULT_INTERVAL_MS: u64 = 1000;

struct StatusBanner {
    text: String,
    is_error: bool,
}

pub struct SerialToolApp {
    ports: Vec<String>,
    selected_port: Option<String>,
    baud_rates: Vec<u32>,
    selected_baud: u32,
    connection: Option<SerialConnection>,
    receive_log: Vec<String>,
    send_inputs: Vec<String>,
    interval_options: Vec<u64>,
    selected_interval_ms: u64,
    continuous_enabled: bool,
    continuous_active: bool,
    next_continuous_send_at: Option<Instant>,
    status: Option<StatusBanner>,
}

impl SerialToolApp {
    pub fn new(_cc: &CreationContext<'_>) -> Self {
        let mut app = Self {
            ports: Vec::new(),
            selected_port: None,
            baud_rates: vec![9600, 19200, 38400, 57600, 115200, 230400, 460800, 921600],
            selected_baud: DEFAULT_BAUD_RATE,
            connection: None,
            receive_log: Vec::new(),
            send_inputs: vec![String::new()],
            interval_options: vec![100, 200, 500, 1000, 2000, 5000],
            selected_interval_ms: DEFAULT_INTERVAL_MS,
            continuous_enabled: false,
            continuous_active: false,
            next_continuous_send_at: None,
            status: None,
        };

        app.refresh_ports();
        app
    }

    fn refresh_ports(&mut self) {
        match serialport::available_ports() {
            Ok(ports) => {
                let mut names = ports.into_iter().map(|port| port.port_name).collect::<Vec<_>>();
                names.sort();

                self.ports = names;

                if self.ports.is_empty() {
                    self.selected_port = None;
                    self.set_status("未发现可用串口设备".to_owned(), false);
                    return;
                }

                let current = self.selected_port.clone();
                self.selected_port = current
                    .filter(|name| self.ports.iter().any(|port| port == name))
                    .or_else(|| self.ports.first().cloned());

                self.set_status(format!("已发现 {} 个设备", self.ports.len()), false);
            }
            Err(err) => self.set_status(format!("刷新设备失败：{err}"), true),
        }
    }

    fn set_status(&mut self, text: String, is_error: bool) {
        self.status = Some(StatusBanner { text, is_error });
    }

    fn open_port(&mut self) {
        let Some(port_name) = self.selected_port.clone() else {
            self.set_status("请先选择串口设备".to_owned(), true);
            return;
        };

        match SerialConnection::open(&port_name, self.selected_baud) {
            Ok(connection) => {
                self.connection = Some(connection);
                self.continuous_active = false;
                self.next_continuous_send_at = None;
                self.set_status(
                    format!("已打开串口：{port_name} @ {} baud", self.selected_baud),
                    false,
                );
            }
            Err(err) => self.set_status(err, true),
        }
    }

    fn close_port(&mut self) {
        self.continuous_active = false;
        self.next_continuous_send_at = None;
        self.connection.take();
        self.set_status("串口已关闭".to_owned(), false);
    }

    fn drain_worker_events(&mut self) {
        let mut should_drop_connection = false;

        loop {
            let event = match self.connection.as_ref() {
                Some(connection) => match connection.try_recv() {
                    Ok(event) => Some(event),
                    Err(TryRecvError::Empty) => None,
                    Err(TryRecvError::Disconnected) => {
                        should_drop_connection = true;
                        None
                    }
                },
                None => None,
            };

            match event {
                Some(WorkerEvent::Received(bytes)) => self.push_receive_log(bytes),
                Some(WorkerEvent::Closed(message)) => {
                    self.continuous_active = false;
                    self.next_continuous_send_at = None;
                    self.set_status(message, false);
                    should_drop_connection = true;
                }
                Some(WorkerEvent::Error(message)) => {
                    self.continuous_active = false;
                    self.next_continuous_send_at = None;
                    self.set_status(message, true);
                    should_drop_connection = true;
                }
                None => break,
            }
        }

        if should_drop_connection {
            self.connection.take();
        }
    }

    fn push_receive_log(&mut self, bytes: Vec<u8>) {
        self.receive_log.push(format_receive_entry(&bytes));

        if self.receive_log.len() > MAX_LOG_ENTRIES {
            let overflow = self.receive_log.len() - MAX_LOG_ENTRIES;
            self.receive_log.drain(0..overflow);
        }
    }

    fn collect_payloads(&self) -> Result<Vec<String>, String> {
        let payloads = self
            .send_inputs
            .iter()
            .filter(|text| !text.trim().is_empty())
            .cloned()
            .collect::<Vec<_>>();

        if payloads.is_empty() {
            return Err("请至少填写一条发送数据".to_owned());
        }

        Ok(payloads)
    }

    fn send_payloads(&mut self) -> bool {
        let Some(connection) = self.connection.as_ref() else {
            self.set_status("请先打开串口".to_owned(), true);
            return false;
        };

        let payloads = match self.collect_payloads() {
            Ok(payloads) => payloads,
            Err(err) => {
                self.set_status(err, true);
                return false;
            }
        };

        match connection.send_strings(&payloads) {
            Ok(()) => true,
            Err(err) => {
                self.continuous_active = false;
                self.next_continuous_send_at = None;
                self.set_status(format!("发送失败：{err}"), true);
                false
            }
        }
    }

    fn handle_send_button(&mut self) {
        if self.continuous_enabled {
            if self.continuous_active {
                self.continuous_active = false;
                self.next_continuous_send_at = None;
                self.set_status("已停止持续发送".to_owned(), false);
                return;
            }

            if self.send_payloads() {
                self.continuous_active = true;
                self.next_continuous_send_at =
                    Some(Instant::now() + Duration::from_millis(self.selected_interval_ms));
                self.set_status(
                    format!("已开始持续发送，周期 {} ms", self.selected_interval_ms),
                    false,
                );
            }

            return;
        }

        if self.send_payloads() {
            self.set_status("发送完成".to_owned(), false);
        }
    }

    fn tick_continuous_send(&mut self) {
        if !self.continuous_enabled || !self.continuous_active {
            return;
        }

        let Some(next_send_at) = self.next_continuous_send_at else {
            self.next_continuous_send_at =
                Some(Instant::now() + Duration::from_millis(self.selected_interval_ms));
            return;
        };

        if Instant::now() < next_send_at {
            return;
        }

        if self.send_payloads() {
            self.next_continuous_send_at =
                Some(Instant::now() + Duration::from_millis(self.selected_interval_ms));
        }
    }
}

impl eframe::App for SerialToolApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut Frame) {
        self.drain_worker_events();
        self.tick_continuous_send();

        let was_continuous_enabled = self.continuous_enabled;
        let is_open = self.connection.is_some();
        let mut refresh_requested = false;
        let mut open_or_close_requested = false;
        let mut send_requested = false;
        let mut add_input_requested = false;

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("Rust 串口工具");
            ui.label("原生桌面 UI，支持 Windows 和 Linux");
            ui.separator();

            ui.horizontal(|ui| {
                ui.add_enabled_ui(!is_open, |ui| {
                    egui::ComboBox::from_label("设备列表")
                        .selected_text(
                            self.selected_port
                                .clone()
                                .unwrap_or_else(|| "未发现设备".to_owned()),
                        )
                        .show_ui(ui, |ui| {
                            for port in &self.ports {
                                ui.selectable_value(
                                    &mut self.selected_port,
                                    Some(port.clone()),
                                    port,
                                );
                            }
                        });

                    egui::ComboBox::from_label("波特率")
                        .selected_text(self.selected_baud.to_string())
                        .show_ui(ui, |ui| {
                            for baud in &self.baud_rates {
                                ui.selectable_value(
                                    &mut self.selected_baud,
                                    *baud,
                                    baud.to_string(),
                                );
                            }
                        });

                    if ui.button("刷新").clicked() {
                        refresh_requested = true;
                    }
                });

                let button_text = if is_open { "关闭" } else { "打开" };
                if ui.button(button_text).clicked() {
                    open_or_close_requested = true;
                }
            });

            if let Some(status) = &self.status {
                let color = if status.is_error {
                    Color32::from_rgb(220, 70, 70)
                } else {
                    Color32::from_rgb(90, 180, 120)
                };

                ui.label(RichText::new(&status.text).color(color));
            }

            ui.separator();
            ui.label("接收数据");
            ScrollArea::vertical()
                .max_height(280.0)
                .stick_to_bottom(true)
                .show(ui, |ui| {
                    if self.receive_log.is_empty() {
                        ui.weak("暂无接收数据");
                    } else {
                        for line in &self.receive_log {
                            ui.monospace(line);
                        }
                    }
                });

            ui.separator();
            ui.label("发送数据");

            let last_input_index = self.send_inputs.len().saturating_sub(1);

            for (index, input) in self.send_inputs.iter_mut().enumerate() {
                ui.horizontal(|ui| {
                    ui.label(format!("数据 {}", index + 1));
                    ui.add(
                        TextEdit::singleline(input)
                            .desired_width(420.0)
                            .hint_text("输入要发送的文本"),
                    );

                    if index == last_input_index && ui.button("Add").clicked() {
                        add_input_requested = true;
                    }
                });
            }

            ui.separator();
            ui.horizontal(|ui| {
                let send_label = if self.continuous_active { "Stop" } else { "Send" };
                if ui
                    .add_enabled(self.connection.is_some(), egui::Button::new(send_label))
                    .clicked()
                {
                    send_requested = true;
                }

                ui.checkbox(&mut self.continuous_enabled, "持续发送");

                if self.continuous_enabled {
                    egui::ComboBox::from_label("时间间隔")
                        .selected_text(format!("{} ms", self.selected_interval_ms))
                        .show_ui(ui, |ui| {
                            for interval in &self.interval_options {
                                ui.selectable_value(
                                    &mut self.selected_interval_ms,
                                    *interval,
                                    format!("{} ms", interval),
                                );
                            }
                        });
                }
            });
        });

        if !self.continuous_enabled && was_continuous_enabled {
            self.continuous_active = false;
            self.next_continuous_send_at = None;
        }

        if add_input_requested {
            self.send_inputs.push(String::new());
        }

        if refresh_requested {
            self.refresh_ports();
        }

        if open_or_close_requested {
            if is_open {
                self.close_port();
            } else {
                self.open_port();
            }
        }

        if send_requested {
            self.handle_send_button();
        }

        if self.connection.is_some() || self.continuous_active {
            ctx.request_repaint_after(Duration::from_millis(50));
        }
    }
}

fn format_receive_entry(bytes: &[u8]) -> String {
    let timestamp = Local::now().format("%H:%M:%S%.3f");
    let hex = bytes
        .iter()
        .map(|byte| format!("{byte:02X}"))
        .collect::<Vec<_>>()
        .join(" ");

    let text = String::from_utf8_lossy(bytes)
        .replace('\r', "\\r")
        .replace('\n', "\\n");

    format!("[{timestamp}] HEX {hex} | TXT {text}")
}
