//! 通用桥客户端（壳侧）— Windows 命名管道 / POSIX unix socket。
//!
//! 与插件侧（Node）的协议一致：新行分隔 JSON，双向同构——
//!   插件 → 壳：progress / state / menu / notification / log
//!   壳 → 插件：menu-click / nav-result / window-event / shutdown-request
//!
//! 传输抽象为单一 trait（BridgeTransport），平台实现各约 40 行（下方 cfg 块），
//! 上层（托盘状态、导航、生命周期）只依赖 BridgeClient，与平台无关。
//! 两边均零第三方依赖：Windows 用 std::fs（映射 CreateFileW），
//! POSIX 用 std::os::unix::net::UnixStream。

use std::io::{self, BufRead, BufReader, Read, Write};
use std::time::{Duration, Instant};

/// 与插件侧 bridgeEndpoint() 对应的端点构造（协议一致性）。
/// Windows: \\.\pipe\dsh-desktop-<token>；POSIX: <tmpdir>/dsh-desktop-<token>.sock。
pub fn endpoint(token: &str) -> String {
    #[cfg(windows)]
    {
        format!("\\\\.\\pipe\\dsh-desktop-{token}")
    }
    #[cfg(unix)]
    {
        std::env::temp_dir()
            .join(format!("dsh-desktop-{token}.sock"))
            .to_string_lossy()
            .into_owned()
    }
}

/// 传输句柄：既能读又能写。
pub trait BridgeTransport: Read + Write + Send {}

// ---------------------------------------------------------------------------
// 平台传输实现
// ---------------------------------------------------------------------------

#[cfg(windows)]
mod imp {
    use super::*;
    use std::fs::OpenOptions;

    const ERROR_FILE_NOT_FOUND: i32 = 2;
    const ERROR_PIPE_BUSY: i32 = 231;

    /// Windows 命名管道客户端：std::fs 直接映射 CreateFileW，
    /// 以 GENERIC_READ|GENERIC_WRITE 打开（管道需要双向句柄）。
    pub struct PipeTransport {
        file: std::fs::File,
    }

    impl PipeTransport {
        pub fn open(path: &str) -> io::Result<Self> {
            let file = OpenOptions::new().read(true).write(true).open(path)?;
            Ok(Self { file })
        }
    }

    impl Read for PipeTransport {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            self.file.read(buf)
        }
    }

    impl Write for PipeTransport {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.file.write(buf)
        }
        fn flush(&mut self) -> io::Result<()> {
            self.file.flush()
        }
    }

    pub fn open_transport(path: &str) -> io::Result<PipeTransport> {
        PipeTransport::open(path)
    }

    /// 管道服务端尚未就绪（端点不存在 / 所有实例忙）时重试。
    pub fn is_retryable(err: &io::Error) -> bool {
        matches!(err.raw_os_error(), Some(ERROR_FILE_NOT_FOUND | ERROR_PIPE_BUSY))
    }
}

#[cfg(unix)]
mod imp {
    use super::*;
    use std::os::unix::net::UnixStream;

    /// POSIX unix socket 客户端：std::os::unix 原生支持。
    pub struct UnixTransport {
        stream: UnixStream,
    }

    impl UnixTransport {
        pub fn open(path: &str) -> io::Result<Self> {
            Ok(Self {
                stream: UnixStream::connect(path)?,
            })
        }
    }

    impl Read for UnixTransport {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            self.stream.read(buf)
        }
    }

    impl Write for UnixTransport {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.stream.write(buf)
        }
        fn flush(&mut self) -> io::Result<()> {
            self.stream.flush()
        }
    }

    pub fn open_transport(path: &str) -> io::Result<UnixTransport> {
        UnixTransport::open(path)
    }

    /// 监听端未就绪（连接被拒 / 文件不存在）时重试。
    pub fn is_retryable(err: &io::Error) -> bool {
        matches!(
            err.kind(),
            io::ErrorKind::ConnectionRefused | io::ErrorKind::NotFound
        )
    }
}

#[cfg(windows)]
impl BridgeTransport for imp::PipeTransport {}
#[cfg(unix)]
impl BridgeTransport for imp::UnixTransport {}

// ---------------------------------------------------------------------------
// 通用客户端：指数退避连接 + 行式 JSON 收发（平台无关）
// ---------------------------------------------------------------------------

const INITIAL_DELAY: Duration = Duration::from_millis(250);
const MAX_DELAY: Duration = Duration::from_secs(2);
const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// 平台无关的桥客户端。内部句柄是 Box<dyn BridgeTransport>，
/// 上层代码无需关心当前跑在哪个平台上。
pub struct BridgeClient {
    path: String,
    reader: BufReader<Box<dyn BridgeTransport>>,
}

impl BridgeClient {
    /// 带指数退避的连接：子进程刚 spawn 时监听端尚未就绪属正常时序。
    pub fn connect_with_retry(path: &str) -> io::Result<Self> {
        Self::connect_with_retry_and_timeout(path, DEFAULT_CONNECT_TIMEOUT)
    }

    pub fn connect_with_retry_and_timeout(path: &str, timeout: Duration) -> io::Result<Self> {
        let deadline = Instant::now() + timeout;
        let mut delay = INITIAL_DELAY;
        loop {
            match imp::open_transport(path) {
                Ok(transport) => {
                    return Ok(Self {
                        path: path.to_owned(),
                        reader: BufReader::new(Box::new(transport)),
                    });
                }
                Err(err) if imp::is_retryable(&err) && Instant::now() < deadline => {
                    std::thread::sleep(delay);
                    delay = (delay * 2).min(MAX_DELAY);
                }
                Err(err) => {
                    return Err(io::Error::new(
                        err.kind(),
                        format!("bridge connect {path} failed: {err}"),
                    ));
                }
            }
        }
    }

    /// 发送一行原始 JSON（壳 → 插件）。
    pub fn send_line(&mut self, line: &str) -> io::Result<()> {
        self.reader.get_mut().write_all(line.as_bytes())?;
        self.reader.get_mut().flush()
    }

    /// 发送一条 JSON 消息（自动补 \n）。
    pub fn send_message(&mut self, message: &serde_json::Value) -> io::Result<()> {
        let mut line = serde_json::to_string(message)
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
        line.push('\n');
        self.send_line(&line)
    }

    /// 阻塞读取一行原始 JSON（插件 → 壳）。EOF 表示对端关闭。
    pub fn recv_line(&mut self) -> io::Result<String> {
        let mut line = String::new();
        let n = self.reader.read_line(&mut line)?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "bridge closed",
            ));
        }
        if line.ends_with('\n') {
            line.pop();
        }
        if line.ends_with('\r') {
            line.pop();
        }
        Ok(line)
    }

    /// 读取并解析一条消息。
    pub fn recv_message(&mut self) -> io::Result<serde_json::Value> {
        let line = self.recv_line()?;
        serde_json::from_str(&line)
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))
    }

    /// 断线后重连（事件流线程消费到 EOF 时调用）。
    pub fn reconnect(&mut self) -> io::Result<()> {
        let path = self.path.clone();
        let deadline = Instant::now() + DEFAULT_CONNECT_TIMEOUT;
        let mut delay = INITIAL_DELAY;
        loop {
            match imp::open_transport(&path) {
                Ok(transport) => {
                    self.reader = BufReader::new(Box::new(transport));
                    return Ok(());
                }
                Err(err) if imp::is_retryable(&err) && Instant::now() < deadline => {
                    std::thread::sleep(delay);
                    delay = (delay * 2).min(MAX_DELAY);
                }
                Err(err) => return Err(err),
            }
        }
    }
}
