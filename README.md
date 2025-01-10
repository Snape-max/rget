# Rget - 多线程文件下载器

## 简介

Rget 是一个多线程文件下载器，使用 Rust 编写，支持通过命令行参数配置下载任务。它能够并发下载多个文件片段，并将它们合并为一个完整的文件。

## 特性

- 支持多线程下载以提高下载速度。
- 自动处理 HTTP 请求并解析文件大小。
- 提供进度条显示下载进度。
- 支持指定输出文件路径和下载分块数量。
- 错误重试机制，确保下载稳定性。

## 安装

### 从源码编译

1. 确保已安装 Rust 和 Cargo。
2. 克隆仓库：
   ```bash
   git clone https://github.com/Snape-max/rget.git
   cd rget
   ```
3. 编译项目：
   ```bash
   cargo build --release
   ```

4. 将生成的可执行文件 `rget` 添加到系统 PATH 中（位于 `target/release/` 目录下）。

### 使用预编译二进制文件

请访问 [Releases](https://github.com/Snape-max/rget/releases) 页面下载适合您操作系统的预编译二进制文件，并将其添加到系统 PATH 中。

## 使用方法

### 命令行参数

```bash
rget [OPTIONS] <URL>
```

#### 参数说明

- `<URL>`: 必填，要下载的文件 URL。
- `-o, --output <FILE>`: 可选，指定输出文件路径，默认为当前目录下的文件名。
- `-c, --chunks <CHUNKS>`: 可选，指定并行下载的分块数量，默认为 4。

### 示例

1. 下载文件并保存为默认名称：
   ```bash
   rget https://example.com/file.zip
   ```

2. 下载文件并指定输出路径：
   ```bash
   rget -o /path/to/output/file.zip https://example.com/file.zip
   ```

3. 下载文件并指定分块数量：
   ```bash
   rget -c 8 https://example.com/file.zip
   ```

## 日志输出

Rget 在运行过程中会输出带有颜色的日志信息，帮助用户了解下载状态：

- `[INFO]`: 一般信息。
- `[SUCCESS]`: 成功完成的操作。
- `[WARNING]`: 警告信息。
- `[ERROR]`: 错误信息。

## 依赖库

- [clap](https://crates.io/crates/clap): 命令行参数解析。
- [reqwest](https://crates.io/crates/reqwest): HTTP 请求库。
- [tokio](https://crates.io/crates/tokio): 异步运行时。
- [indicatif](https://crates.io/crates/indicatif): 进度条显示。
- [colored](https://crates.io/crates/colored): 控制台彩色输出。


## 许可证

本项目采用 MIT 许可证，详情参见 [LICENSE](LICENSE) 文件。