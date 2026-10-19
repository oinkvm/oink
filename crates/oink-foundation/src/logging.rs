//! 日志初始化。级别由环境变量 `OINK_LOG`（`tracing` 过滤语法）控制，默认 `info`。

use tracing_subscriber::EnvFilter;

/// 初始化全局日志，输出到 stderr。重复调用无副作用。
pub fn init() {
    let filter = EnvFilter::try_from_env("OINK_LOG").unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .try_init();
}
