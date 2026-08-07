# serial-tool 项目全局理解

Rust 编写的图形化串口调试工具，UI 基于 `eframe/egui`（原生，非 Web），串口操作基于 `serialport` crate。支持 Windows / Linux / macOS（macOS 仅在字体与配置路径上有分支代码，README 未提及但代码已覆盖）。所有 UI 文案为中文。

## 架构总览

3 个源文件，职责清晰分离：

| 文件 | 职责 |
|---|---|
| `src/main.rs` | 入口，创建 960x720 窗口，启动 eframe 应用 |
| `src/app.rs` | UI 主逻辑 + 状态管理 + 配置持久化 + 字体加载（约 1200 行） |
| `src/serial_worker.rs` | 串口后台线程：读写串口、DTR/RTS 控制，通过 mpsc 通道与 UI 线程通信 |

核心设计：**串口 I/O 在独立后台线程中运行**（每打开一个端口 spawn 一个线程），UI 线程只做非阻塞的 `try_recv` 轮询，避免阻塞 egui 渲染循环。线程名格式 `serial-worker-{port_name}`。

## 数据流

### 发送路径（UI → 串口）
1. UI 收集各输入框内容，经 `SendInput::to_bytes()` 转为字节（文本 → UTF-8 字节；HEX → 按空格分词解析），再按 `NewlineMode`（无/CRLF/LF）追加末尾换行。
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

- **连接状态**：`connection: Option<SerialConnection>`，打开后禁用设备列表/波特率/刷新/串口参数控件；DTR/RTS 开关仅在打开时可用（乐观更新本地状态后下发命令，serialport 4.9 无读取 API，无法回读实际电平）。
- **串口参数**：`selected_baud`（8 档预设 + `DragValue` 自定义）、`selected_data_bits`/`selected_stop_bits`/`selected_parity`（serialport 4.9 的 `StopBits` 只有 One/Two，**无 1.5**），打开时传给 `SerialConnection::open`。
- **持续发送**：`continuous_enabled`（勾选意愿）与 `continuous_active`（实际运行中）分离。勾选后显示间隔下拉（100/200/500/1000/2000/5000 ms）。首次点 Send 立即发送一次并进入循环，按钮变为 `Stop`。`tick_continuous_send()` 每帧用 `Instant` 检查是否到点。注意：**取消勾选持续发送会同时停止循环**（update 末尾的 `was_continuous_enabled` 逻辑）；输入非法/打开关闭串口/发送失败都会通过 `stop_continuous()` 重置持续发送状态，避免忙循环。
- **发送校验**：HEX 输入要求每个 token 恰好 2 个十六进制字符（`SendInput::validate()`，错误用 ⚠ 图标 + hover 提示）；发送时跳过空输入框，全空则报"请至少填写一条发送数据"。
- **发送日志时机**：`send_payloads` 仅在 `send_bytes` 成功提交给后台线程后才记录日志（先 clone payloads），未打开串口/发送失败不记录。
- **输入框管理**：默认 1 条，多行输入（`TextEdit::multiline` desired_rows(2)），最后一条旁有 `Add`，多于 1 条时每条旁有 `Del`。按钮点击标记为标志位，**在闭包外**统一执行（`add_input_requested` / `remove_input_index`），避免在 egui 借用中修改 `send_inputs`。清空日志/清空输入按钮因无冲突借用可直接在闭包内执行。
- **其他借用惯例**：`update()` 内所有 UI 操作通过局部布尔标志延迟到闭包结束后执行（refresh/open-close/send/settings-changed），这是本项目刻意保持的模式。
- **状态横幅**：`status: Option<StatusBanner>`，错误红色 `(220,70,70)`、成功绿色 `(90,180,120)`，显示在顶栏下方。
- **重绘调度**：连接打开或持续发送中时 `request_repaint_after(50ms)`，空闲时靠 egui 事件驱动。

## 配置持久化

- 保存位置：Windows `%APPDATA%\serial_tool\config.toml`；Linux `$XDG_CONFIG_HOME/serial_tool/config.toml` 或 `~/.config/...`；macOS `~/Library/Application Support/serial_tool/...`。
- 保存内容（全部 8 个键）：`selected_port`、`selected_baud`、`selected_interval_ms`、`receive_as_hex`、`newline_mode`（none/crlf/lf）、`data_bits`（5-8）、`stop_bits`（one/two）、`parity`（none/even/odd），**手写 TOML 序列化**（`escape_toml_string`/`parse_toml_string` 处理转义，枚举键解析时先取引号再校验合法值，有单元测试覆盖），未引入 toml crate。
- 保存时机：任何设置变化时（`settings_changed` 标志 → `save_config()`）。

## 字体（中文显示）

`setup_fonts()` 按平台探测中文字体路径，找到第一个可读的即注入 egui 字体定义（追加到 Proportional 和 Monospace 家族末尾）：
- Windows：msyh.ttc / simsun.ttc / simhei.ttf
- macOS：PingFang.ttc / STHeiti
- Linux：wqy-microhei / NotoSansCJK 多个候选路径

若全部失败则回退默认字体（中文可能显示为方块）。修改字体时注意保持这套平台分支结构。

## 构建与运行

- `cargo run` 本地运行；`.cargo/config.toml` 定义了别名：`build-win`（msvc 交叉目标）、`build-linux`、`run-dev`。
- 依赖按目标平台条件编译：Linux 额外启用 eframe 的 `wayland`/`x11` 特性，Linux 构建需 `pkg-config libasound2-dev libudev-dev libxkbcommon-dev libwayland-dev libx11-dev`。
- release profile：LTO thin、codegen-units=1、strip。
- `main.rs` 中 `#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]`：release 版 Windows 下不弹控制台窗口。

## 注意点 / 陷阱

- 后台线程 read 超时设为 100ms，配合 UI 的 50ms 轮询，收发延迟可接受但不适合高吞吐。
- 发送是"每条输入框一条消息"，按 `NewlineMode` 统一追加换行后缀（HEX 模式同样追加）；持续发送只发非空输入框。
- `serial_worker.rs` 的命令（SendBatch/SetDtr/SetRts/Close）在同一 `try_recv` 分支处理，先发后收，Close 后立即退出。
- DTR/RTS 为乐观更新：serialport 4.9 只能写不能读（无 `read_data_terminal_ready`/`read_request_to_send`），UI 勾选即假设生效，失败由 Error 事件提示。
- 接收/发送日志共用 `receive_as_hex` 开关，无独立控制。
- 串口打开失败的错误信息由 `serialport` 错误透传（中文前缀包装），直接展示给用户。
- 发送日志区高度为硬编码估算（`120.0 + input_count * 54.0`），输入框数量很多时布局可能失真。

## 测试

`app.rs` 底部有 5 个单元测试（config 解析、TOML 转义、非法枚举值拒绝），无集成测试。运行：`cargo test`。
