# Rust 串口工具

一个使用 Rust 原生 UI（`eframe/egui`）实现的简单图形化串口工具，支持 Windows 和 Linux。

## 功能

- 串口设备列表下拉选择
- 波特率下拉选择
- 打开 / 关闭串口，打开后禁用设备和波特率选择
- 打开失败时显示明确错误原因
- 上方滚动显示接收数据（HEX + 文本）
- 下方支持多条发送输入，默认 1 条，最后一条后提供 `Add`
- `Send` 支持单次发送
- 勾选“持续发送”后显示发送周期下拉，首次点击 `Send` 开始持续发送，按钮切换为 `Stop`

## 本地运行

```bash
cargo run
```

## 编译 Windows 可执行文件

在 Windows 主机上：

```bash
cargo build --release --target x86_64-pc-windows-msvc
```

输出文件：

```text
target\x86_64-pc-windows-msvc\release\serial_tool.exe
```

也可以使用别名：

```bash
cargo build-win
```

## 编译 Linux 可执行文件

在 Linux 主机上：

```bash
cargo build --release --target x86_64-unknown-linux-gnu
```

输出文件：

```text
target/x86_64-unknown-linux-gnu/release/serial_tool
```

也可以使用别名：

```bash
cargo build-linux
```

## Linux 构建依赖

以 Debian / Ubuntu 为例，需要先安装常见桌面与串口依赖：

```bash
sudo apt update
sudo apt install -y pkg-config libasound2-dev libudev-dev libxkbcommon-dev libwayland-dev libx11-dev
```

## 说明

- 当前发送逻辑按“每个输入框一条消息”处理，并按输入顺序逐条写入串口。
- 持续发送只会发送非空输入框内容。
