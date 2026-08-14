//! 跨语言冒烟：Rust 客户端连接 Node 侧桥服务端，验证通用桥。
//!
//! 用法（先起服务端）：
//!   node --experimental-strip-types plugins/desktop-host/test/pipe-server.ts <token>
//!   $env:DSH_BRIDGE_ENDPOINT='\\.\pipe\dsh-desktop-<token>'
//!   cargo run --example bridge_smoke
//! （POSIX 端点：<tmpdir>/dsh-desktop-<token>.sock）
//! 端点优先读环境变量 DSH_BRIDGE_ENDPOINT，其次用 bridge::endpoint("cross-smoke")。

use std::io;
use std::time::{Duration, Instant};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let endpoint = std::env::var("DSH_BRIDGE_ENDPOINT")
        .ok()
        .unwrap_or_else(|| dsh_desktop_lib::bridge::endpoint("cross-smoke"));

    let mut client = dsh_desktop_lib::bridge::BridgeClient::connect_with_retry(&endpoint)?;
    eprintln!("[bridge_smoke] connected: {endpoint}");

    client.send_message(&serde_json::json!({ "type": "ping" }))?;
    eprintln!("[bridge_smoke] >> ping");

    // 预期回复：服务端对 ping 回 state + notification 两条
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut got = 0usize;
    while Instant::now() < deadline {
        match client.recv_message() {
            Ok(message) => {
                println!("[bridge_smoke] << {}", message);
                got += 1;
                if got >= 2 {
                    break;
                }
            }
            Err(err) if err.kind() == io::ErrorKind::UnexpectedEof => break,
            Err(err) => {
                eprintln!("[bridge_smoke] recv error: {err}");
                break;
            }
        }
    }
    if got == 0 {
        eprintln!("[bridge_smoke] FAIL: no message received");
        std::process::exit(1);
    }

    // 通知服务端退出；对端可能已关闭（管道 232），忽略发送错误
    let _ = client.send_message(&serde_json::json!({ "type": "quit" }));
    eprintln!("[bridge_smoke] >> quit");
    Ok(())
}
