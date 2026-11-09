# ETW Viewer

> **⚠️ 仅支持 Windows**
>
> 本项目依赖 ETW (Event Tracing for Windows) 与 Win32 API，
> 只能在 Windows 目标平台上编译运行。非 Windows 目标会触发
> `compile_error!`，依赖也不会被拉入依赖图。
>
> 支持的目标：`x86_64-pc-windows-msvc`、`aarch64-pc-windows-msvc`、`i686-pc-windows-msvc`

## 构建

\`\`\`powershell
cargo build --release
\`\`\`

首次启动会弹 UAC 请求管理员权限（运行时提权，非 manifest）。

## License

本项目采用双许可，可任选其一：

- [MIT License](LICENSE-MIT)
- [Apache License 2.0](LICENSE-APACHE)

### 贡献

除非你明确声明，否则任何有意提交以包含在本项目中的贡献，
均按上述双许可条款授权，不附加任何额外条款或条件。
<!-- test: ˫����֤ -->
