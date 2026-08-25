# serial-tool 项目全局理解

Rust 编写的图形化串口调试工具，UI 基于 `eframe/egui`（原生，非 Web），串口操作基于 `serialport` crate。支持 Windows / Linux / macOS（macOS 仅在字体与配置路径上有分支代码，README 未提及但代码已覆盖）。所有 UI 文案为中文。

## 架构总览

3 个源文件，职责清晰分离：

| 文件 | 职责 |
|---|---|
| `src/main.rs` | 入口，创建 960x720 窗口（标题 `Rust 串口工具`），启动 eframe 应用 |
| `src/app.rs` | UI 主逻辑 + 状态管理 + 配置持久化 + 字体加载（约 1200 行） |
| `src/serial_worker.rs` | 串口后台线程：读写串口、DTR/RTS 控制，通过 mpsc 通道与 UI 线程通信 |

核心设计：**串口 I/O 在独立后台线程中运行**（每打开一个端口 spawn 一个线程），UI 线程只做非阻塞的 `try_recv` 轮询，避免阻塞 egui 渲染循环。线程名格式 `serial-worker-{port_name}`。

## 数据流

### 发送路径（UI → 串口）
1. UI 收集各输入框内容，经 `SendInput::to_bytes()` 转为字节（文本 → UTF-8 字节；HEX → 按空格分词解析），再按该行 `+0D`/`+0A` 勾选追加末尾字节（勾 +0D 追 0D、勾 +0A 追 0A、都勾追 0D0A）。无全局换行设置，旧的 `NewlineMode` 枚举已移除。
2. `SerialConnection::send_bytes()` 通过 `cmd_tx` 发送 `WorkerCommand::SendBatch(Vec<Vec<u8>>)`。
3. 后台线程按顺序 `write_all` + `flush` 每条 payload，失败则发 `WorkerEvent::Error` 并退出线程。
4. DTR/RTS 通过 `WorkerCommand::SetDtr/SetRts` 下发，worker 执行失败同样发 Error 并退出。

### 接收路径（串口 → UI）
1. 后台线程循环 `port.read()`（100ms 超时），读到数据发 `WorkerEvent::Received(Vec<u8>)`。
2. UI 每帧调用 `drain_worker_events()` 排空事件队列：接收数据带毫秒时间戳（`chrono::Local`，格式 `%H:%M:%S%.3f`）存入日志。
3. 日志上限 `MAX_LOG_ENTRIES = 1000` 条，超出从头部 drain。
4. 日志在追加时即构建 `LogEntry{hex_line, text_line}` 两种格式缓存（`format_receive_hex` 空格分隔大写两位 / `format_receive_text` 转义 `\r`/`\n`），渲染时直接取缓存行，切换 HEX 开关零成本；`send_log` 也受同一个 HEX 开关控制。

### 关闭路径
- `SerialConnection::drop()` 发送 `WorkerCommand::Close` 并 `join` 线程（阻塞式关闭）。
- 后台线程收到 Close 或发送端断开（`TryRecvError::Disconnected`）时发 `WorkerEvent::Closed` 后退出。
- UI 收到 Closed/Error/Disconnected 事件时置 `should_drop_connection`，事件排空后统一 `connection.take()`，防止借用冲突。

## 关键状态与逻辑（app.rs）

- **关键常量**：`MAX_LOG_ENTRIES = 1000`（发送/接收日志**各自独立**上限）、`DEFAULT_BAUD_RATE = 115200`、`DEFAULT_INTERVAL_MS = 1000`、配置目录 `serial_tool`/文件名 `config.toml`；worker read 缓冲 4096 字节。
- **布局**：`ui.columns(2)` 双列——左列发送（日志 + 输入框 + 发送控制），右列接收（日志）。
- **启动流程**：`SerialToolApp::new`：`setup_fonts` → 加载配置 → `apply_config` → `refresh_ports`（自动刷新一次，无设备时提示"未发现可用串口设备"）。
- **连接状态**：`connection: Option<SerialConnection>`，打开后禁用设备列表/波特率/刷新/串口参数控件；DTR/RTS 开关仅在打开时可用（乐观更新本地状态后下发命令，serialport 4.9 无读取 API，无法回读实际电平）。**Send 按钮在未打开串口时禁用**（`add_enabled(connection.is_some(), ...)`）。
- **串口参数**：`selected_baud`（8 档预设 9600~921600 + `DragValue` 自定义，范围 `1..=4_000_000`）、`selected_data_bits`/`selected_stop_bits`/`selected_parity`（serialport 4.9 的 `StopBits` 只有 One/Two，**无 1.5**），打开时传给 `SerialConnection::open`。
- **持续发送**：`continuous_enabled`（勾选意愿）与 `continuous_active`（实际运行中）分离。勾选后显示间隔下拉（100/200/500/1000/2000/5000 ms）。首次点 Send 立即发送一次并进入循环，按钮变为 `Stop`。`tick_continuous_send()` 每帧用 `Instant` 检查是否到点。注意：**取消勾选持续发送会同时停止循环**（update 末尾的 `was_continuous_enabled` 逻辑）；输入非法/打开关闭串口/发送失败都会通过 `stop_continuous()` 重置持续发送状态，避免忙循环。
- **发送校验**：HEX 输入要求每个 token 恰好 2 个十六进制字符（`SendInput::validate()`，错误用 ⚠ 图标 + hover 提示）；发送时跳过空输入框，全空则报"请至少填写一条发送数据"。
- **发送日志时机**：`send_payloads` 仅在 `send_bytes` 成功提交给后台线程后才记录日志（先 clone payloads），未打开串口/发送失败不记录。
- **输入框管理**：默认 1 条，多行输入（`TextEdit::multiline` desired_rows(2)），最后一条旁有 `Add`，多于 1 条时每条旁有 `Del`。每条输入带独立 `HEX` 勾选与 `+0D`/`+0A` 末尾追加勾选（`SendInput::append_cr`/`append_lf`，默认不勾；追加字节在 `collect_payloads` 中按先 0D 后 0A 顺序 push）。按钮点击标记为标志位，**在闭包外**统一执行（`add_input_requested` / `remove_input_index`），避免在 egui 借用中修改 `send_inputs`。清空日志/清空输入按钮因无冲突借用可直接在闭包内执行。
- **其他借用惯例**：`update()` 内所有 UI 操作通过局部布尔标志延迟到闭包结束后执行（refresh/open-close/send/settings-changed），这是本项目刻意保持的模式。
- **状态横幅**：`status: Option<StatusBanner>`，错误红色 `(220,70,70)`、成功绿色 `(90,180,120)`，显示在顶栏下方。
- **重绘调度**：连接打开或持续发送中时 `request_repaint_after(50ms)`，空闲时靠 egui 事件驱动。

## 配置持久化

- 保存位置：Windows `%APPDATA%\serial_tool\config.toml`；Linux `$XDG_CONFIG_HOME/serial_tool/config.toml` 或 `~/.config/...`；macOS `~/Library/Application Support/serial_tool/...`。
- 保存内容（全部 7 个键）：`selected_port`、`selected_baud`、`selected_interval_ms`、`receive_as_hex`、`data_bits`（5-8）、`stop_bits`（one/two）、`parity`（none/even/odd），**手写 TOML 序列化**（`escape_toml_string`/`parse_toml_string` 处理转义，枚举键解析时先取引号再校验合法值，有单元测试覆盖），未引入 toml crate。注：旧版的 `newline_mode` 键已随全局换行移除，解析器对未知键静默忽略（`_ => {}`），旧配置文件向后兼容。
- 保存时机：任何设置变化时（`settings_changed` 标志 → `save_config()`）。

## 字体（中文显示）

`setup_fonts()` 按平台探测中文字体路径，找到第一个可读的即注入 egui 字体定义（追加到 Proportional 和 Monospace 家族末尾）：
- Windows：msyh.ttc / simsun.ttc / simhei.ttf
- macOS：PingFang.ttc / STHeiti
- Linux：wqy-microhei / NotoSansCJK 多个候选路径

若全部失败则回退默认字体（中文可能显示为方块）。修改字体时注意保持这套平台分支结构。

## 构建与运行

- 依赖版本（以 Cargo.lock 为准）：eframe/egui 0.31.1、serialport **4.9.0**（Cargo.toml 声明 `"4.7.0"` 宽松约束）、chrono 0.4.44（`default-features = false`，仅 clock/std）、edition 2021。
- `cargo run` 本地运行；`.cargo/config.toml` 定义了别名：`build-win`（msvc 交叉目标）、`build-linux`、`run-dev`，并设 `[build] target-dir = "target"`。
- 依赖按目标平台条件编译：Linux 额外启用 eframe 的 `wayland`/`x11` 特性，Linux 构建需 `pkg-config libasound2-dev libudev-dev libxkbcommon-dev libwayland-dev libx11-dev`。
- release profile：LTO thin、codegen-units=1、strip。
- `main.rs` 中 `#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]`：release 版 Windows 下不弹控制台窗口。

## 注意点 / 陷阱

- 后台线程 read 超时设为 100ms，配合 UI 的 50ms 轮询，收发延迟可接受但不适合高吞吐。
- 发送是"每条输入框一条消息"，按该行 `+0D`/`+0A` 勾选追加末尾字节（先 0D 后 0A，两个都勾 = 0D0A；HEX 模式同样追加）；持续发送只发非空输入框。
- `serial_worker.rs` 的命令（SendBatch/SetDtr/SetRts/Close）在同一 `try_recv` 分支处理，先发后收，Close 后立即退出。
- DTR/RTS 为乐观更新：serialport 4.9 只能写不能读（无 `read_data_terminal_ready`/`read_request_to_send`），UI 勾选即假设生效，失败由 Error 事件提示。
- 接收/发送日志共用 `receive_as_hex` 开关，无独立控制。
- 串口打开失败的错误信息由 `serialport` 错误透传（中文前缀包装），直接展示给用户。
- 发送日志区高度为硬编码估算（`120.0 + input_count * 54.0`），输入框数量很多时布局可能失真。
- egui 0.31 API 注意点：日志行不用 `ui.monospace`，改用 `egui::Label::new(RichText::new(line).monospace()).wrap()`；`ScrollArea` 用 `id_salt(...)`（0.31 命名）配合 `stick_to_bottom(true)`。
- 状态消息有固定文案：`发送完成` / `已开始持续发送，周期 {n} ms` / `已停止持续发送` / `请先打开串口`；HEX 校验错误消息带数据序号（`数据 {n} 包含无效的 HEX 格式，请修正后再发送`）。

## 测试

`app.rs` 底部 `mod tests` 有 5 个单元测试，无集成测试、无 CI 配置。运行：`cargo test`。
测试名：`parse_config_reads_saved_values`、`parse_config_unescapes_port_name`、`escape_toml_string_escapes_reserved_characters`、`parse_config_reads_extended_settings`、`parse_config_rejects_invalid_enum_values`。

## 待确认问题

- macOS：代码已有字体与配置路径分支，但 README 只声明 Windows/Linux，macOS 构建未验证。
- Cargo.toml 声明 `serialport = "4.7.0"`，Cargo.lock 实际解析为 4.9.0——是否为刻意宽松约束待确认（代码按 4.9 API 编写）。
- 发送日志与接收日志共用 `receive_as_hex` 一个开关，无独立控制（当前设计如此，非缺陷；如需独立控制需改 `push_send_log` 渲染分支）。
