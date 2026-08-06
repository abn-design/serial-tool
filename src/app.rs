use std::{
    env, fs,
    path::PathBuf,
    sync::mpsc::TryRecvError,
    time::{Duration, Instant},
};

use chrono::Local;
use eframe::{
    egui::{self, Color32, RichText, ScrollArea, TextEdit},
    CreationContext, Frame,
};

use crate::serial_worker::{SerialConnection, WorkerEvent};
use serialport::{DataBits, Parity, StopBits};

const MAX_LOG_ENTRIES: usize = 1000;
const DEFAULT_BAUD_RATE: u32 = 115200;
const DEFAULT_INTERVAL_MS: u64 = 1000;
const CONFIG_FILE_NAME: &str = "config.toml";
const CONFIG_DIR_NAME: &str = "serial_tool";

struct StatusBanner {
    text: String,
    is_error: bool,
}

/// 一条收发日志，两种显示格式在追加时一次性构建并缓存，
/// 避免每帧对全部日志重新格式化。
struct LogEntry {
    hex_line: String,
    text_line: String,
}

impl LogEntry {
    fn new(timestamp: &str, bytes: &[u8]) -> Self {
        Self {
            hex_line: format_receive_hex(timestamp, bytes),
            text_line: format_receive_text(timestamp, bytes),
        }
    }
}

/// 发送时末尾追加的换行方式
#[derive(Clone, Copy, PartialEq, Eq)]
enum NewlineMode {
    None,
    CrLf,
    Lf,
}

impl NewlineMode {
    fn suffix(self) -> &'static [u8] {
        match self {
            NewlineMode::None => &[],
            NewlineMode::CrLf => b"\r\n",
            NewlineMode::Lf => b"\n",
        }
    }

    fn label(self) -> &'static str {
        match self {
            NewlineMode::None => "无",
            NewlineMode::CrLf => "CRLF",
            NewlineMode::Lf => "LF",
        }
    }

    fn from_str(value: &str) -> Option<Self> {
        match value {
            "none" => Some(Self::None),
            "crlf" => Some(Self::CrLf),
            "lf" => Some(Self::Lf),
            _ => None,
        }
    }

    fn to_str(self) -> &'static str {
        match self {
            NewlineMode::None => "none",
            NewlineMode::CrLf => "crlf",
            NewlineMode::Lf => "lf",
        }
    }
}

#[derive(Default)]
struct AppConfig {
    selected_port: Option<String>,
    selected_baud: Option<u32>,
    selected_interval_ms: Option<u64>,
    receive_as_hex: Option<bool>,
    newline_mode: Option<String>,
    data_bits: Option<String>,
    stop_bits: Option<String>,
    parity: Option<String>,
}

impl AppConfig {
    fn load() -> Self {
        let Some(path) = config_file_path() else {
            return Self::default();
        };

        let Ok(contents) = fs::read_to_string(path) else {
            return Self::default();
        };

        parse_config(&contents)
    }

    fn save(&self) {
        let Some(path) = config_file_path() else {
            return;
        };

        let Some(parent) = path.parent() else {
            return;
        };

        if fs::create_dir_all(parent).is_err() {
            return;
        }

        let _ = fs::write(path, self.to_toml());
    }

    fn to_toml(&self) -> String {
        let mut content = String::new();

        if let Some(port) = &self.selected_port {
            content.push_str("selected_port = \"");
            content.push_str(&escape_toml_string(port));
            content.push_str("\"\n");
        }

        if let Some(baud) = self.selected_baud {
            content.push_str(&format!("selected_baud = {baud}\n"));
        }

        if let Some(interval) = self.selected_interval_ms {
            content.push_str(&format!("selected_interval_ms = {interval}\n"));
        }

        if let Some(hex) = self.receive_as_hex {
            content.push_str(&format!("receive_as_hex = {hex}\n"));
        }

        if let Some(mode) = &self.newline_mode {
            content.push_str(&format!("newline_mode = \"{mode}\"\n"));
        }

        if let Some(bits) = &self.data_bits {
            content.push_str(&format!("data_bits = \"{bits}\"\n"));
        }

        if let Some(bits) = &self.stop_bits {
            content.push_str(&format!("stop_bits = \"{bits}\"\n"));
        }

        if let Some(parity) = &self.parity {
            content.push_str(&format!("parity = \"{parity}\"\n"));
        }

        content
    }
}

struct SendInput {
    text: String,
    is_hex: bool,
    hex_error: Option<String>,
}

impl SendInput {
    fn new() -> Self {
        Self {
            text: String::new(),
            is_hex: false,
            hex_error: None,
        }
    }

    fn validate(&mut self) {
        if !self.is_hex {
            self.hex_error = None;
            return;
        }

        let trimmed = self.text.trim();
        if trimmed.is_empty() {
            self.hex_error = None;
            return;
        }

        for token in trimmed.split_whitespace() {
            if token.len() != 2 || !token.chars().all(|c| c.is_ascii_hexdigit()) {
                self.hex_error = Some(format!(
                    "'{}' 不是有效的 HEX 字节（需要恰好两位十六进制字符，格式示例: 01 A5 FF）",
                    token
                ));
                return;
            }
        }
        self.hex_error = None;
    }

    fn to_bytes(&self) -> Result<Vec<u8>, String> {
        if self.is_hex {
            self.text
                .split_whitespace()
                .map(|token| {
                    u8::from_str_radix(token, 16)
                        .map_err(|_| format!("'{}' 不是有效的十六进制值", token))
                })
                .collect()
        } else {
            Ok(self.text.as_bytes().to_vec())
        }
    }
}

pub struct SerialToolApp {
    ports: Vec<String>,
    selected_port: Option<String>,
    baud_rates: Vec<u32>,
    selected_baud: u32,
    connection: Option<SerialConnection>,
    receive_log: Vec<LogEntry>,
    send_log: Vec<LogEntry>,
    receive_as_hex: bool,
    send_inputs: Vec<SendInput>,
    interval_options: Vec<u64>,
    selected_interval_ms: u64,
    newline_mode: NewlineMode,
    selected_data_bits: DataBits,
    selected_stop_bits: StopBits,
    selected_parity: Parity,
    dtr: bool,
    rts: bool,
    continuous_enabled: bool,
    continuous_active: bool,
    next_continuous_send_at: Option<Instant>,
    status: Option<StatusBanner>,
}

impl SerialToolApp {
    pub fn new(cc: &CreationContext<'_>) -> Self {
        setup_fonts(&cc.egui_ctx);
        let config = AppConfig::load();

        let mut app = Self {
            ports: Vec::new(),
            selected_port: config.selected_port.clone(),
            baud_rates: vec![9600, 19200, 38400, 57600, 115200, 230400, 460800, 921600],
            selected_baud: config.selected_baud.unwrap_or(DEFAULT_BAUD_RATE),
            connection: None,
            receive_log: Vec::new(),
            send_log: Vec::new(),
            receive_as_hex: config.receive_as_hex.unwrap_or(false),
            send_inputs: vec![SendInput::new()],
            interval_options: vec![100, 200, 500, 1000, 2000, 5000],
            selected_interval_ms: DEFAULT_INTERVAL_MS,
            continuous_enabled: false,
            continuous_active: false,
            next_continuous_send_at: None,
            status: None,
            newline_mode: NewlineMode::None,
            selected_data_bits: DataBits::Eight,
            selected_stop_bits: StopBits::One,
            selected_parity: Parity::None,
            dtr: false,
            rts: false,
        };

        app.apply_config(&config);
        app.refresh_ports();
        app
    }

    fn apply_config(&mut self, config: &AppConfig) {
        if let Some(interval) = config.selected_interval_ms {
            self.selected_interval_ms = interval;
        }

        if let Some(hex) = config.receive_as_hex {
            self.receive_as_hex = hex;
        }

        if let Some(mode) = config
            .newline_mode
            .as_deref()
            .and_then(NewlineMode::from_str)
        {
            self.newline_mode = mode;
        }

        self.selected_data_bits = match config.data_bits.as_deref() {
            Some("5") => DataBits::Five,
            Some("6") => DataBits::Six,
            Some("7") => DataBits::Seven,
            _ => DataBits::Eight,
        };
        self.selected_stop_bits = match config.stop_bits.as_deref() {
            Some("two") => StopBits::Two,
            _ => StopBits::One,
        };
        self.selected_parity = match config.parity.as_deref() {
            Some("even") => Parity::Even,
            Some("odd") => Parity::Odd,
            _ => Parity::None,
        };
    }

    fn refresh_ports(&mut self) {
        match serialport::available_ports() {
            Ok(ports) => {
                let mut names = ports
                    .into_iter()
                    .map(|port| port.port_name)
                    .collect::<Vec<_>>();
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

    fn stop_continuous(&mut self) {
        self.continuous_active = false;
        self.next_continuous_send_at = None;
    }

    fn save_config(&self) {
        AppConfig {
            selected_port: self.selected_port.clone(),
            selected_baud: Some(self.selected_baud),
            selected_interval_ms: Some(self.selected_interval_ms),
            receive_as_hex: Some(self.receive_as_hex),
            newline_mode: Some(self.newline_mode.to_str().to_owned()),
            data_bits: Some(
                match self.selected_data_bits {
                    DataBits::Five => "5",
                    DataBits::Six => "6",
                    DataBits::Seven => "7",
                    DataBits::Eight => "8",
                }
                .to_owned(),
            ),
            stop_bits: Some(
                match self.selected_stop_bits {
                    StopBits::One => "one",
                    StopBits::Two => "two",
                }
                .to_owned(),
            ),
            parity: Some(
                match self.selected_parity {
                    Parity::None => "none",
                    Parity::Even => "even",
                    Parity::Odd => "odd",
                }
                .to_owned(),
            ),
        }
        .save();
    }

    fn open_port(&mut self) {
        let Some(port_name) = self.selected_port.clone() else {
            self.set_status("请先选择串口设备".to_owned(), true);
            return;
        };

        match SerialConnection::open(
            &port_name,
            self.selected_baud,
            self.selected_data_bits,
            self.selected_stop_bits,
            self.selected_parity,
        ) {
            Ok(connection) => {
                self.connection = Some(connection);
                self.dtr = false;
                self.rts = false;
                self.stop_continuous();
                self.set_status(
                    format!("已打开串口：{port_name} @ {} baud", self.selected_baud),
                    false,
                );
            }
            Err(err) => self.set_status(err, true),
        }
    }

    fn close_port(&mut self) {
        self.stop_continuous();
        self.connection.take();
        self.dtr = false;
        self.rts = false;
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
                    self.stop_continuous();
                    self.set_status(message, false);
                    should_drop_connection = true;
                }
                Some(WorkerEvent::Error(message)) => {
                    self.stop_continuous();
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
        let timestamp = Local::now().format("%H:%M:%S%.3f").to_string();
        self.receive_log.push(LogEntry::new(&timestamp, &bytes));

        if self.receive_log.len() > MAX_LOG_ENTRIES {
            let overflow = self.receive_log.len() - MAX_LOG_ENTRIES;
            self.receive_log.drain(0..overflow);
        }
    }

    fn push_send_log(&mut self, bytes: Vec<u8>) {
        let timestamp = Local::now().format("%H:%M:%S%.3f").to_string();
        self.send_log.push(LogEntry::new(&timestamp, &bytes));

        if self.send_log.len() > MAX_LOG_ENTRIES {
            let overflow = self.send_log.len() - MAX_LOG_ENTRIES;
            self.send_log.drain(0..overflow);
        }
    }

    fn collect_payloads(&self) -> Result<Vec<Vec<u8>>, String> {
        let mut payloads = Vec::new();
        let mut found_non_empty = false;

        for (index, input) in self.send_inputs.iter().enumerate() {
            if input.text.trim().is_empty() {
                continue;
            }
            found_non_empty = true;

            if input.is_hex && input.hex_error.is_some() {
                return Err(format!(
                    "数据 {} 包含无效的 HEX 格式，请修正后再发送",
                    index + 1
                ));
            }

            let mut bytes = input.to_bytes()?;
            bytes.extend_from_slice(self.newline_mode.suffix());
            payloads.push(bytes);
        }

        if !found_non_empty {
            return Err("请至少填写一条发送数据".to_owned());
        }

        Ok(payloads)
    }

    fn send_payloads(&mut self) -> bool {
        let payloads = match self.collect_payloads() {
            Ok(payloads) => payloads,
            Err(err) => {
                // 输入非法（如 HEX 格式错误、全空）时停止持续发送，避免每帧重试的忙循环
                self.stop_continuous();
                self.set_status(err, true);
                return false;
            }
        };

        let result = match self.connection.as_ref() {
            Some(conn) => conn.send_bytes(payloads.clone()),
            None => {
                self.stop_continuous();
                self.set_status("请先打开串口".to_owned(), true);
                return false;
            }
        };

        match result {
            Ok(()) => {
                // 仅在成功提交给串口后台线程后才记录发送日志
                for bytes in &payloads {
                    self.push_send_log(bytes.clone());
                }
                true
            }
            Err(err) => {
                self.stop_continuous();
                self.set_status(format!("发送失败：{err}"), true);
                false
            }
        }
    }

    fn handle_send_button(&mut self) {
        if self.continuous_enabled {
            if self.continuous_active {
                self.stop_continuous();
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
        let mut settings_changed = false;
        let mut remove_input_index: Option<usize> = None;

        egui::CentralPanel::default().show(ctx, |ui| {
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
                                if ui
                                    .selectable_value(
                                        &mut self.selected_port,
                                        Some(port.clone()),
                                        port,
                                    )
                                    .changed()
                                {
                                    settings_changed = true;
                                }
                            }
                        });

                    egui::ComboBox::from_label("波特率")
                        .selected_text(self.selected_baud.to_string())
                        .show_ui(ui, |ui| {
                            for baud in &self.baud_rates {
                                if ui
                                    .selectable_value(
                                        &mut self.selected_baud,
                                        *baud,
                                        baud.to_string(),
                                    )
                                    .changed()
                                {
                                    settings_changed = true;
                                }
                            }
                        });

                    // 自定义波特率：可直接输入任意值
                    if ui
                        .add(
                            egui::DragValue::new(&mut self.selected_baud)
                                .range(1..=4_000_000)
                                .speed(100),
                        )
                        .changed()
                    {
                        settings_changed = true;
                    }

                    if ui.button("刷新").clicked() {
                        refresh_requested = true;
                    }
                });

                let button_text = if is_open { "关闭" } else { "打开" };
                if ui.button(button_text).clicked() {
                    open_or_close_requested = true;
                }

                if ui.checkbox(&mut self.receive_as_hex, "HEX 显示").changed() {
                    settings_changed = true;
                }
            });

            // -- 串口参数 --
            ui.horizontal(|ui| {
                ui.add_enabled_ui(!is_open, |ui| {
                    egui::ComboBox::from_label("数据位")
                        .selected_text(match self.selected_data_bits {
                            DataBits::Five => "5",
                            DataBits::Six => "6",
                            DataBits::Seven => "7",
                            DataBits::Eight => "8",
                        })
                        .show_ui(ui, |ui| {
                            for (bits, label) in [
                                (DataBits::Five, "5"),
                                (DataBits::Six, "6"),
                                (DataBits::Seven, "7"),
                                (DataBits::Eight, "8"),
                            ] {
                                if ui
                                    .selectable_value(&mut self.selected_data_bits, bits, label)
                                    .changed()
                                {
                                    settings_changed = true;
                                }
                            }
                        });

                    egui::ComboBox::from_label("停止位")
                        .selected_text(match self.selected_stop_bits {
                            StopBits::One => "1",
                            StopBits::Two => "2",
                        })
                        .show_ui(ui, |ui| {
                            for (bits, label) in [(StopBits::One, "1"), (StopBits::Two, "2")] {
                                if ui
                                    .selectable_value(&mut self.selected_stop_bits, bits, label)
                                    .changed()
                                {
                                    settings_changed = true;
                                }
                            }
                        });

                    egui::ComboBox::from_label("校验位")
                        .selected_text(match self.selected_parity {
                            Parity::None => "无",
                            Parity::Even => "偶校验",
                            Parity::Odd => "奇校验",
                        })
                        .show_ui(ui, |ui| {
                            for (parity, label) in [
                                (Parity::None, "无"),
                                (Parity::Even, "偶校验"),
                                (Parity::Odd, "奇校验"),
                            ] {
                                if ui
                                    .selectable_value(&mut self.selected_parity, parity, label)
                                    .changed()
                                {
                                    settings_changed = true;
                                }
                            }
                        });
                });

                ui.add_enabled_ui(is_open, |ui| {
                    if ui.checkbox(&mut self.dtr, "DTR").changed() {
                        if let Some(conn) = self.connection.as_ref() {
                            let _ = conn.set_dtr(self.dtr);
                        }
                    }
                    if ui.checkbox(&mut self.rts, "RTS").changed() {
                        if let Some(conn) = self.connection.as_ref() {
                            let _ = conn.set_rts(self.rts);
                        }
                    }
                });
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

            ui.columns(2, |columns| {
                // ═══════════ Left column: Send ═══════════
                columns[0].vertical(|ui| {
                    // -- Send log --
                    ui.horizontal(|ui| {
                        ui.label("发送数据");
                        if ui.small_button("清空日志").clicked() {
                            self.send_log.clear();
                        }
                        if ui.small_button("清空输入").clicked() {
                            for input in &mut self.send_inputs {
                                input.text.clear();
                                input.validate();
                            }
                        }
                    });

                    let input_count = self.send_inputs.len() as f32;
                    let controls_height = 120.0 + input_count * 54.0;
                    let scroll_height = (ui.available_height() - controls_height).max(80.0);

                    egui::Frame::group(ui.style())
                        .inner_margin(egui::Margin::symmetric(4, 4))
                        .show(ui, |ui| {
                            ui.set_min_width(ui.available_width());
                            ui.set_min_height(scroll_height);
                            ScrollArea::vertical()
                                .id_salt("send_data")
                                .max_height(scroll_height)
                                .stick_to_bottom(true)
                                .auto_shrink([false, false])
                                .show(ui, |ui| {
                                    if self.send_log.is_empty() {
                                        ui.weak("暂无发送数据");
                                    } else {
                                        let as_hex = self.receive_as_hex;
                                        for entry in &self.send_log {
                                            let line = if as_hex {
                                                &entry.hex_line
                                            } else {
                                                &entry.text_line
                                            };
                                            // 替换 ui.monospace(line)
                                            ui.add(
                                                egui::Label::new(
                                                    egui::RichText::new(line.as_str()).monospace(),
                                                )
                                                .wrap(),
                                            );
                                        }
                                    }
                                });
                        });

                    ui.separator();

                    // -- Send controls --
                    let input_count = self.send_inputs.len();
                    let last_input_index = input_count.saturating_sub(1);
                    let can_remove = input_count > 1;

                    for (index, input) in self.send_inputs.iter_mut().enumerate() {
                        ui.horizontal(|ui| {
                            ui.set_min_width(ui.available_width());
                            ui.label(format!("数据 {}", index + 1));

                            let hint = if input.is_hex {
                                "例如: 01 02 0A FF"
                            } else {
                                "输入要发送的文本"
                            };
                            let response = ui.add(
                                TextEdit::multiline(&mut input.text)
                                    .desired_rows(2)
                                    .desired_width(220.0)
                                    .hint_text(hint),
                            );

                            if response.changed() {
                                input.validate();
                            }

                            if ui.checkbox(&mut input.is_hex, "HEX").changed() {
                                input.validate();
                            }

                            if let Some(ref err) = input.hex_error {
                                ui.label(
                                    RichText::new("\u{26a0}").color(Color32::from_rgb(220, 70, 70)),
                                )
                                .on_hover_text(err.as_str());
                            }

                            if index == last_input_index && ui.button("Add").clicked() {
                                add_input_requested = true;
                            }

                            if can_remove && ui.button("Del").clicked() {
                                remove_input_index = Some(index);
                            }
                        });
                    }

                    ui.separator();
                    ui.horizontal(|ui| {
                        let send_label = if self.continuous_active {
                            "Stop"
                        } else {
                            "Send"
                        };
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
                                        if ui
                                            .selectable_value(
                                                &mut self.selected_interval_ms,
                                                *interval,
                                                format!("{} ms", interval),
                                            )
                                            .changed()
                                        {
                                            settings_changed = true;
                                        }
                                    }
                                });
                        }

                        egui::ComboBox::from_label("换行")
                            .selected_text(self.newline_mode.label())
                            .show_ui(ui, |ui| {
                                for mode in [NewlineMode::None, NewlineMode::CrLf, NewlineMode::Lf]
                                {
                                    if ui
                                        .selectable_value(
                                            &mut self.newline_mode,
                                            mode,
                                            mode.label(),
                                        )
                                        .changed()
                                    {
                                        settings_changed = true;
                                    }
                                }
                            });
                    });
                });

                // ═══════════ Right column: Receive ═══════════
                columns[1].vertical(|ui| {
                    ui.horizontal(|ui| {
                        ui.label("接收数据");
                        if ui.small_button("清空日志").clicked() {
                            self.receive_log.clear();
                        }
                    });

                    let scroll_height = ui.available_height() - 32.0;

                    egui::Frame::group(ui.style())
                        .inner_margin(egui::Margin::symmetric(4, 4))
                        .show(ui, |ui| {
                            ui.set_min_width(ui.available_width());
                            ui.set_min_height(scroll_height);
                            ScrollArea::vertical()
                                .id_salt("recv_data")
                                .max_height(scroll_height)
                                .stick_to_bottom(true)
                                .auto_shrink([false, false])
                                .show(ui, |ui| {
                                    if self.receive_log.is_empty() {
                                        ui.weak("暂无接收数据");
                                    } else {
                                        let as_hex = self.receive_as_hex;
                                        for entry in &self.receive_log {
                                            let line = if as_hex {
                                                &entry.hex_line
                                            } else {
                                                &entry.text_line
                                            };
                                            ui.add(
                                                egui::Label::new(
                                                    egui::RichText::new(line.as_str()).monospace(),
                                                )
                                                .wrap(),
                                            );
                                        }
                                    }
                                });
                        });
                });
            });
        });

        if !self.continuous_enabled && was_continuous_enabled {
            self.stop_continuous();
        }

        if let Some(idx) = remove_input_index {
            self.send_inputs.remove(idx);
        }

        if add_input_requested {
            self.send_inputs.push(SendInput::new());
        }

        if settings_changed {
            self.save_config();
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

fn setup_fonts(ctx: &egui::Context) {
    #[cfg(target_os = "windows")]
    let font_paths: &[&str] = &[
        "C:/Windows/Fonts/msyh.ttc",
        "C:/Windows/Fonts/simsun.ttc",
        "C:/Windows/Fonts/simhei.ttf",
    ];

    #[cfg(target_os = "macos")]
    let font_paths: &[&str] = &[
        "/System/Library/Fonts/PingFang.ttc",
        "/System/Library/Fonts/STHeiti Light.ttc",
    ];

    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    let font_paths: &[&str] = &[
        "/usr/share/fonts/truetype/wqy/wqy-microhei.ttc",
        "/usr/share/fonts/wqy-microhei/wqy-microhei.ttc",
        "/usr/share/fonts/truetype/noto/NotoSansCJK-Regular.ttc",
        "/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc",
        "/usr/share/fonts/google-noto-cjk/NotoSansCJK-Regular.ttc",
        "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
        "/usr/share/fonts/noto/NotoSansCJK-Regular.ttc",
    ];

    for path in font_paths {
        if let Ok(data) = std::fs::read(path) {
            let mut fonts = egui::FontDefinitions::default();
            fonts.font_data.insert(
                "cjk_font".to_owned(),
                egui::FontData::from_owned(data).into(),
            );
            if let Some(family) = fonts.families.get_mut(&egui::FontFamily::Proportional) {
                family.push("cjk_font".to_owned());
            }
            if let Some(family) = fonts.families.get_mut(&egui::FontFamily::Monospace) {
                family.push("cjk_font".to_owned());
            }
            ctx.set_fonts(fonts);
            return;
        }
    }
}

fn format_receive_hex(timestamp: &str, bytes: &[u8]) -> String {
    let hex = bytes
        .iter()
        .map(|byte| format!("{byte:02X}"))
        .collect::<Vec<_>>()
        .join(" ");
    format!("[{timestamp}] {hex}")
}

fn format_receive_text(timestamp: &str, bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes)
        .replace('\r', "\\r")
        .replace('\n', "\\n");
    format!("[{timestamp}] {text}")
}

fn config_file_path() -> Option<PathBuf> {
    #[cfg(target_os = "windows")]
    let base_dir = env::var_os("APPDATA").map(PathBuf::from);

    #[cfg(target_os = "macos")]
    let base_dir = env::var_os("HOME")
        .map(PathBuf::from)
        .map(|home| home.join("Library").join("Application Support"));

    #[cfg(all(not(target_os = "windows"), not(target_os = "macos")))]
    let base_dir = env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")));

    base_dir.map(|dir| dir.join(CONFIG_DIR_NAME).join(CONFIG_FILE_NAME))
}

fn parse_config(contents: &str) -> AppConfig {
    let mut config = AppConfig::default();

    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        let Some((key, value)) = line.split_once('=') else {
            continue;
        };

        let key = key.trim();
        let value = value.trim();

        match key {
            "selected_port" => {
                config.selected_port = parse_toml_string(value);
            }
            "selected_baud" => {
                config.selected_baud = value.parse().ok();
            }
            "selected_interval_ms" => {
                config.selected_interval_ms = value.parse().ok();
            }
            "receive_as_hex" => {
                config.receive_as_hex = value.parse().ok();
            }
            "newline_mode" => {
                if let Some(mode) = parse_toml_string(value) {
                    if matches!(mode.as_str(), "none" | "crlf" | "lf") {
                        config.newline_mode = Some(mode);
                    }
                }
            }
            "data_bits" => {
                if let Some(bits) = parse_toml_string(value) {
                    if matches!(bits.as_str(), "5" | "6" | "7" | "8") {
                        config.data_bits = Some(bits);
                    }
                }
            }
            "stop_bits" => {
                if let Some(bits) = parse_toml_string(value) {
                    if matches!(bits.as_str(), "one" | "two") {
                        config.stop_bits = Some(bits);
                    }
                }
            }
            "parity" => {
                if let Some(parity) = parse_toml_string(value) {
                    if matches!(parity.as_str(), "none" | "even" | "odd") {
                        config.parity = Some(parity);
                    }
                }
            }
            _ => {}
        }
    }

    config
}

fn parse_toml_string(value: &str) -> Option<String> {
    let trimmed = value.trim();
    let inner = trimmed.strip_prefix('"')?.strip_suffix('"')?;
    let mut result = String::new();
    let mut chars = inner.chars();

    while let Some(ch) = chars.next() {
        if ch != '\\' {
            result.push(ch);
            continue;
        }

        match chars.next() {
            Some('\\') => result.push('\\'),
            Some('"') => result.push('"'),
            Some('n') => result.push('\n'),
            Some('r') => result.push('\r'),
            Some('t') => result.push('\t'),
            Some(other) => {
                result.push('\\');
                result.push(other);
            }
            None => result.push('\\'),
        }
    }

    Some(result)
}

fn escape_toml_string(value: &str) -> String {
    let mut escaped = String::new();

    for ch in value.chars() {
        match ch {
            '\\' => escaped.push_str("\\\\"),
            '"' => escaped.push_str("\\\""),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            other => escaped.push(other),
        }
    }

    escaped
}

#[cfg(test)]
mod tests {
    use super::{escape_toml_string, parse_config};

    #[test]
    fn parse_config_reads_saved_values() {
        let config = parse_config("selected_port = \"COM3\"\nselected_baud = 57600\n");

        assert_eq!(config.selected_port.as_deref(), Some("COM3"));
        assert_eq!(config.selected_baud, Some(57600));
    }

    #[test]
    fn parse_config_unescapes_port_name() {
        let config = parse_config("selected_port = \"tty\\\"USB0\\\\A\"\n");

        assert_eq!(config.selected_port.as_deref(), Some("tty\"USB0\\A"));
    }

    #[test]
    fn escape_toml_string_escapes_reserved_characters() {
        assert_eq!(
            escape_toml_string("tty\"USB0\\A"),
            "tty\\\"USB0\\\\A".to_owned()
        );
    }

    #[test]
    fn parse_config_reads_extended_settings() {
        let config = parse_config(
            "selected_interval_ms = 500\nreceive_as_hex = true\nnewline_mode = \"crlf\"\ndata_bits = \"7\"\nstop_bits = \"two\"\nparity = \"even\"\n",
        );

        assert_eq!(config.selected_interval_ms, Some(500));
        assert_eq!(config.receive_as_hex, Some(true));
        assert_eq!(config.newline_mode.as_deref(), Some("crlf"));
        assert_eq!(config.data_bits.as_deref(), Some("7"));
        assert_eq!(config.stop_bits.as_deref(), Some("two"));
        assert_eq!(config.parity.as_deref(), Some("even"));
    }

    #[test]
    fn parse_config_rejects_invalid_enum_values() {
        let config =
            parse_config("newline_mode = \"bogus\"\ndata_bits = \"9\"\nparity = \"weird\"\n");

        assert_eq!(config.newline_mode, None);
        assert_eq!(config.data_bits, None);
        assert_eq!(config.parity, None);
    }
}
