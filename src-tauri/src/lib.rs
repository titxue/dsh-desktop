use std::fs::{create_dir_all, File};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::windows::process::CommandExt;
use std::path::{Component, Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tauri::Manager;
use tauri::Url;

/// 桌面壳 ↔ dsh 进程的通用 IPC 桥（Windows 管道 / POSIX unix socket）。
/// 接入点：M1 桥客户端线程消费事件、发送命令（见 docs/design-desktop-host.md）。
pub mod bridge;

/// CREATE_NO_WINDOW: keep the server and npm consoles hidden.
const CREATE_NO_WINDOW: u32 = 0x08000000;

/// Holds the spawned dsh server child so the app can stop it on exit.
struct ServerState(Mutex<Option<Child>>);

/// Pinned Node.js runtime manifest shipped in the bootstrap resources.
#[derive(Deserialize)]
struct NodeManifest {
    version: String,
    urls: Vec<String>,
    sha256: String,
}

/// Bootstrap progress pushed to the loading page via window.eval.
#[derive(Clone, Serialize)]
struct ProgressState {
    phase: String,
    label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pct: Option<u8>,
    detail: String,
}

impl ProgressState {
    fn new(phase: &str, label: &str, pct: Option<u8>, detail: &str) -> Self {
        Self { phase: phase.into(), label: label.into(), pct, detail: detail.into() }
    }
}

/// Strip the Win32 verbatim (\\?\...) prefix tauri's path resolver returns;
/// Node's module loader cannot handle \\?\-prefixed entry paths.
fn win_clean(path: &Path) -> PathBuf {
    let raw = path.to_string_lossy();
    if let Some(rest) = raw.strip_prefix("\\\\?\\UNC\\") {
        PathBuf::from(format!("\\\\{rest}"))
    } else if let Some(rest) = raw.strip_prefix("\\\\?\\") {
        PathBuf::from(rest)
    } else {
        path.to_path_buf()
    }
}

/// Append one line to the desktop app's diagnostic log under the app data dir.
fn log_line(app: &tauri::AppHandle, line: &str) {
    if let Ok(dir) = app.path().app_log_dir() {
        let _ = create_dir_all(&dir);
        let _ = File::options().create(true).append(true).open(dir.join("desktop.log")).map(|mut f| {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let _ = writeln!(f, "[{now}] {line}");
        });
    }
}

/// Find a bundled resource subdirectory (e.g. "bootstrap" or "runtime") across
/// the layouts tauri produces: next to the exe (raw/dev build) or under an
/// "_up_" staging dir (NSIS installs).
fn find_resource_subdir(resource_dir: &Path, exe_dir: &Path, name: &str) -> Option<PathBuf> {
    for base in [resource_dir, exe_dir] {
        for candidate in [base.join(name), base.join("_up_").join(name)] {
            if candidate.is_dir() {
                return Some(candidate);
            }
        }
    }
    None
}

/// Best-effort current URL of a window, for close-request diagnostics.
fn window_url(app: &tauri::AppHandle, label: &str) -> String {
    app.get_webview_window(label)
        .and_then(|w| w.url().ok())
        .map(|u| u.to_string())
        .unwrap_or_default()
}

/// Ask the OS for a free TCP port, then release it for the server to bind.
fn pick_free_port() -> u16 {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind ephemeral port");
    listener.local_addr().expect("local addr").port()
}

/// Poll the port until the dsh server accepts connections or the timeout hits.
fn wait_for_port(port: u16, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    false
}

/// Kill the server process tree (Windows: taskkill /T covers worker threads
/// and shell children).
fn kill_tree(child: &mut Child) {
    let _ = Command::new("taskkill")
        .args(["/PID", &child.id().to_string(), "/T", "/F"])
        .creation_flags(CREATE_NO_WINDOW)
        .output();
    let _ = child.kill();
}

/// Publish a progress state: store it, push it to the loading page and reflect
/// it in the window title.
fn send_progress(shared: &Arc<Mutex<ProgressState>>, window: &tauri::WebviewWindow, state: ProgressState) {
    if let Ok(mut guard) = shared.lock() {
        *guard = state.clone();
    }
    if let Ok(json) = serde_json::to_string(&state) {
        let _ = window.eval(&format!("window.updateProgress?.({json})"));
    }
    let title = if state.label.is_empty() {
        "DeepSeek Harness".to_string()
    } else {
        match state.pct {
            Some(pct) => format!("DeepSeek Harness — {} {}%", state.label, pct),
            None => format!("DeepSeek Harness — {}", state.label),
        }
    };
    let _ = window.set_title(&title);
}

/// Download a file, verifying its SHA-256 after the transfer completes and
/// reporting byte progress through the callback.
fn download_verified(
    url: &str,
    dest: &Path,
    expected_sha: &str,
    on_progress: &dyn Fn(u64, u64),
) -> Result<(), String> {
    let resp = ureq::get(url)
        .timeout(Duration::from_secs(300))
        .call()
        .map_err(|e| format!("GET {url}: {e}"))?;
    let total: u64 = resp
        .header("Content-Length")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let mut reader = resp.into_reader();
    let mut file = File::create(dest).map_err(|e| format!("create {}: {e}", dest.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 65536];
    let mut written = 0u64;
    let mut last_reported = 0u64;
    loop {
        let n = reader.read(&mut buf).map_err(|e| format!("read {url}: {e}"))?;
        if n == 0 {
            break;
        }
        written += n as u64;
        hasher.update(&buf[..n]);
        file.write_all(&buf[..n]).map_err(|e| format!("write {}: {e}", dest.display()))?;
        if written - last_reported >= 524_288 || written == total {
            last_reported = written;
            on_progress(written, total);
        }
    }
    file.sync_all().map_err(|e| format!("sync {}: {e}", dest.display()))?;
    let actual = format!("{:x}", hasher.finalize());
    if actual != expected_sha {
        return Err(format!("sha256 mismatch for {url}: expected {expected_sha}, got {actual}"));
    }
    Ok(())
}

/// Extract a zip archive into "dest", stripping the single top-level
/// directory node distributions carry, with zip-slip protection.
fn extract_node_zip(zip_path: &Path, dest: &Path) -> Result<(), String> {
    let file = File::open(zip_path).map_err(|e| format!("open {}: {e}", zip_path.display()))?;
    let mut archive = zip::ZipArchive::new(file).map_err(|e| format!("zip open: {e}"))?;
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i).map_err(|e| format!("zip entry {i}: {e}"))?;
        // Entries look like "node-v22.22.3-win-x64/node.exe": drop the top dir.
        let parts: Vec<&str> = entry.name().split('/').filter(|p| !p.is_empty()).collect();
        if parts.len() < 2 {
            continue;
        }
        let rel = parts[1..].join("/");
        let mut out = dest.to_path_buf();
        for comp in Path::new(&rel).components() {
            match comp {
                Component::Normal(c) => out.push(c),
                _ => return Err(format!("unsafe path in archive: {}", entry.name())),
            }
        }
        if entry.is_dir() {
            create_dir_all(&out).map_err(|e| format!("mkdir {}: {e}", out.display()))?;
        } else {
            if let Some(parent) = out.parent() {
                create_dir_all(parent).map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
            }
            let mut f = File::create(&out).map_err(|e| format!("create {}: {e}", out.display()))?;
            std::io::copy(&mut entry, &mut f).map_err(|e| format!("extract {}: {e}", out.display()))?;
        }
    }
    Ok(())
}

/// Ensure a usable Node.js exists under "deps/node"; download it from the
/// manifest mirrors on first use, reporting progress while doing so.
fn ensure_node(
    deps: &Path,
    manifest: &NodeManifest,
    send: &dyn Fn(ProgressState),
) -> Result<PathBuf, String> {
    let node_exe = deps.join("node").join("node.exe");
    if node_exe.exists() {
        return Ok(node_exe);
    }
    create_dir_all(deps).map_err(|e| format!("mkdir {}: {e}", deps.display()))?;

    let zip_path = deps.join(format!("node-{}.zip", manifest.version));
    let mut last_error = String::new();
    for (i, url) in manifest.urls.iter().enumerate() {
        let label = format!("正在下载 Node.js 运行时 (镜像 {}/{})…", i + 1, manifest.urls.len());
        send(ProgressState::new("download-node", &label, None, ""));
        let outcome = download_verified(url, &zip_path, &manifest.sha256, &|written, total| {
            let pct = if total > 0 { Some(((written * 100) / total) as u8) } else { None };
            let detail = format!(
                "{:.1} MB / {:.1} MB",
                written as f64 / 1048576.0,
                total as f64 / 1048576.0
            );
            send(ProgressState::new("download-node", "正在下载 Node.js 运行时…", pct, &detail));
        });
        match outcome {
            Ok(()) => {
                last_error.clear();
                break;
            }
            Err(e) => {
                last_error = format!("{e}");
                let _ = std::fs::remove_file(&zip_path);
            }
        }
    }
    if !last_error.is_empty() {
        return Err(format!("Node.js 下载失败: {last_error}"));
    }

    send(ProgressState::new("extract-node", "正在解压 Node.js 运行时…", None, ""));
    let node_dir = deps.join("node");
    let tmp_dir = deps.join("node.tmp");
    let _ = std::fs::remove_dir_all(&tmp_dir);
    create_dir_all(&tmp_dir).map_err(|e| format!("mkdir {}: {e}", tmp_dir.display()))?;
    extract_node_zip(&zip_path, &tmp_dir)?;
    let _ = std::fs::remove_dir_all(&node_dir);
    std::fs::rename(&tmp_dir, &node_dir).map_err(|e| format!("rename node dir: {e}"))?;
    let _ = std::fs::remove_file(&zip_path);

    if !node_exe.exists() {
        return Err("Node.js 解压后 node.exe 缺失".into());
    }
    Ok(node_exe)
}

/// Ensure the pinned dependency closure is installed under "deps/node_modules"
/// via the bundled npm; a no-op once present. While npm runs, the tail of its
/// output log is surfaced as progress detail.
fn ensure_deps(
    deps: &Path,
    node_exe: &Path,
    bootstrap_dir: &Path,
    log_dir: &Path,
    send: &dyn Fn(ProgressState),
) -> Result<(), String> {
    let marker = deps.join("node_modules").join("@deepseek-ai").join("dsh").join("package.json");
    if marker.exists() {
        return Ok(());
    }
    let npm_cli = deps.join("node").join("node_modules").join("npm").join("bin").join("npm-cli.js");
    if !npm_cli.exists() {
        return Err(format!("npm CLI missing at {}", npm_cli.display()));
    }

    // npm treats its working directory as the project root, so the pinned
    // manifests must live in the deps dir and npm must run from there.
    // The .npmrc defaults to the npmmirror registry (fast in CN); an explicit
    // DSH_NPM_REGISTRY below overrides it via --registry.
    for manifest in ["package.json", "package-lock.json", ".npmrc"] {
        let src = bootstrap_dir.join(manifest);
        let dst = deps.join(manifest);
        if !dst.exists() && src.exists() {
            std::fs::copy(&src, &dst).map_err(|e| format!("copy {manifest}: {e}"))?;
        }
    }

    // Total package count from the lockfile, for per-package progress.
    let total_pkgs: usize = std::fs::read_to_string(deps.join("package-lock.json"))
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .and_then(|v| v.get("packages").and_then(|p| p.as_object()).map(|o| o.len().saturating_sub(1)))
        .unwrap_or(0);
    // npm 10 logs one "npm http ..." line per package: "npm http fetch GET
    // 200 <url> (cache miss)" on first download and "npm http cache <pkg>@<url>
    // 0ms (cache hit)" when the tarball is already in the local npm cache.
    let fetch_count = |path: &Path| -> usize {
        std::fs::read_to_string(path)
            .map(|s| s.lines().filter(|l| l.contains("npm http")).count())
            .unwrap_or(0)
    };
    let last_fetched_pkg = |path: &Path| -> String {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|s| {
                s.lines()
                    .rev()
                    .find(|l| l.contains("npm http"))
                    .and_then(|l| l.split_whitespace().find(|w| w.ends_with(".tgz")))
                    .map(|w| w.rsplit('/').next().unwrap_or(w).trim_end_matches(".tgz").to_string())
            })
            .unwrap_or_default()
    };

    send(ProgressState::new("install-deps", "正在安装 DeepSeek Harness 依赖…", None, "首次安装约 190 MB，请稍候"));
    let mut cmd = Command::new(win_clean(node_exe));
    cmd.arg(win_clean(&npm_cli))
        .arg("install")
        .arg("--omit=dev")
        .arg("--no-audit")
        .arg("--no-fund")
        .arg("--loglevel=http")
        .creation_flags(CREATE_NO_WINDOW)
        .current_dir(win_clean(deps))
        .stdout(Stdio::from(File::create(log_dir.join("npm.out.log")).map_err(|e| e.to_string())?))
        .stderr(Stdio::from(File::create(log_dir.join("npm.err.log")).map_err(|e| e.to_string())?));
    if let Ok(registry) = std::env::var("DSH_NPM_REGISTRY") {
        if !registry.is_empty() {
            cmd.arg("--registry").arg(registry);
        }
    }
    let mut child = cmd.spawn().map_err(|e| format!("npm install 启动失败: {e}"))?;
    let http_path = log_dir.join("npm.err.log");
    // Progress is time-driven: a first install has no history and package
    // counts are unreliable (the lockfile includes dev-only packages), so the
    // bar advances with real elapsed time against an expected total. Whenever
    // the expectation is exceeded the timeline extends, keeping the bar moving
    // on slow networks; the detail line names the package npm is working on.
    let started = Instant::now();
    let mut expected_ms: u64 = 90_000;
    loop {
        match child.try_wait().map_err(|e| format!("npm wait: {e}"))? {
            Some(status) => {
                if !status.success() {
                    return Err(format!(
                        "npm install 失败 (exit {:?})，详见 {}",
                        status.code(),
                        log_dir.join("npm.err.log").display()
                    ));
                }
                break;
            }
            None => {
                let elapsed = started.elapsed().as_millis() as u64;
                if elapsed > expected_ms {
                    expected_ms = elapsed + 45_000;
                }
                let pct = ((elapsed * 100) / expected_ms).min(95) as u8;
                let remain_s = expected_ms.saturating_sub(elapsed) / 1000;
                let pkg = last_fetched_pkg(&http_path);
                let detail = if pkg.is_empty() {
                    format!("已用 {}s · 预计还需 {}s", elapsed / 1000, remain_s)
                } else {
                    format!("已用 {}s · 预计还需 {}s · {}", elapsed / 1000, remain_s, pkg)
                };
                send(ProgressState::new(
                    "install-deps",
                    &format!("正在安装依赖 (预计还需 {}s)…", remain_s),
                    Some(pct),
                    &detail,
                ));
                std::thread::sleep(Duration::from_millis(1000));
            }
        }
    }
    if !marker.exists() {
        return Err("npm install 完成后缺少 @deepseek-ai/dsh".into());
    }
    Ok(())
}

/// Navigate the window to an inline error page.
fn show_error(window: &tauri::WebviewWindow, message: &str) {
    let escaped = message.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");
    let html = format!(
        "<!doctype html><meta charset=\"utf-8\"><style>body{{background:#12060a;color:#ffd9e0;font:14px system-ui;padding:32px;max-width:720px;margin:auto}}h1{{font-size:18px}}code{{display:block;white-space:pre-wrap;background:#00000055;padding:12px;border-radius:8px;margin-top:12px}}</style><h1>DeepSeek Harness 启动失败</h1><p>启动过程中发生错误：</p><code>{escaped}</code><p style=\"opacity:.7\">完整日志见应用数据目录 logs\\desktop.log。</p>"
    );
    let encoded: String = html
        .bytes()
        .flat_map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => vec![b],
            _ => format!("%{b:02X}").into_bytes(),
        })
        .map(|b| b as char)
        .collect();
    if let Ok(url) = Url::parse(&format!("data:text/html;charset=utf-8,{encoded}")) {
        let _ = window.navigate(url);
    }
}

/// The full first-run bootstrap: node runtime, dependency closure, server.
fn bootstrap_and_run(
    handle: tauri::AppHandle,
    window: tauri::WebviewWindow,
    resource_dir: PathBuf,
    exe_dir: PathBuf,
    shared: Arc<Mutex<ProgressState>>,
) {
    let fail = |window: &tauri::WebviewWindow, what: String| {
        log_line(&handle, &format!("FATAL: {what}"));
        let state = ProgressState::new("error", "启动失败", None, &what);
        send_progress(&shared, window, state);
        show_error(window, &what);
    };
    let send = |state: ProgressState| send_progress(&shared, &window, state);

    let Some(bootstrap_dir) = find_resource_subdir(&resource_dir, &exe_dir, "bootstrap") else {
        return fail(&window, format!("未找到 bootstrap 资源 (resource_dir={}, exe_dir={})", resource_dir.display(), exe_dir.display()));
    };
    log_line(&handle, &format!("bootstrap dir: {}", bootstrap_dir.display()));

    let manifest_path = bootstrap_dir.join("node-manifest.json");
    let manifest: NodeManifest = match serde_json::from_slice(&std::fs::read(&manifest_path).unwrap_or_default()) {
        Ok(m) => m,
        Err(e) => return fail(&window, format!("读取 {} 失败: {e}", manifest_path.display())),
    };

    let data_dir = match handle.path().app_local_data_dir() {
        Ok(d) => d,
        Err(e) => return fail(&window, format!("app data dir: {e}")),
    };
    let deps = data_dir.join("deps");
    log_line(&handle, &format!("deps dir: {}", deps.display()));

    let node_exe = match ensure_node(&deps, &manifest, &send) {
        Ok(n) => n,
        Err(e) => return fail(&window, e),
    };
    send(ProgressState::new("starting", "准备就绪，正在启动…", None, ""));

    let log_dir = handle.path().app_log_dir().unwrap_or_else(|_| data_dir.clone());
    if let Err(e) = ensure_deps(&deps, &node_exe, &bootstrap_dir, &log_dir, &send) {
        return fail(&window, e);
    }

    let bin_js = deps.join("node_modules").join("@deepseek-ai").join("dsh").join("lib").join("bin.js");
    let port = pick_free_port();
    let child = match Command::new(win_clean(&node_exe))
        .arg(win_clean(&bin_js))
        .arg("web")
        .arg("--port")
        .arg(port.to_string())
        .current_dir(win_clean(&deps))
        .creation_flags(CREATE_NO_WINDOW)
        .stdout(File::create(log_dir.join("server.out.log")).map(Stdio::from).unwrap_or_else(|_| Stdio::null()))
        .stderr(File::create(log_dir.join("server.err.log")).map(Stdio::from).unwrap_or_else(|_| Stdio::null()))
        .spawn()
    {
        Ok(c) => c,
        Err(e) => return fail(&window, format!("server 启动失败: {e}")),
    };
    handle.manage(ServerState(Mutex::new(Some(child))));

    std::thread::spawn(move || {
        if wait_for_port(port, Duration::from_secs(120)) {
            let url = format!("http://127.0.0.1:{port}");
            log_line(&handle, &format!("server ready, navigating to {url}"));
            send_progress(&shared, &window, ProgressState::new("ready", "", None, ""));
            if let Ok(url) = Url::parse(&url) {
                let _ = window.navigate(url);
            }
        } else {
            log_line(&handle, "server did not become ready in 120s");
            show_error(&window, "本地服务在 120 秒内未就绪，请查看 logs\\server.err.log。");
        }
    });
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .setup(|app| {
            let resource_dir = app.path().resource_dir()?;
            let exe_dir = app
                .path()
                .executable_dir()
                .unwrap_or_else(|_| std::env::current_exe().map(|p| p.to_path_buf()).unwrap_or_default());

            let shared = Arc::new(Mutex::new(ProgressState::new("starting", "准备中…", None, "")));
            let window = tauri::WebviewWindowBuilder::new(
                app,
                "main",
                tauri::WebviewUrl::App("index.html".into()),
            )
            .title("DeepSeek Harness")
            .inner_size(1440.0, 900.0)
            .min_inner_size(960.0, 640.0)
            .build()?;

            let handle = app.handle().clone();
            std::thread::spawn(move || {
                bootstrap_and_run(handle, window, resource_dir, exe_dir, shared);
            });
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|app_handle, event| match event {
            tauri::RunEvent::ExitRequested { .. } => {
                log_line(&app_handle, "event: ExitRequested");
            }
            tauri::RunEvent::Exit => {
                log_line(&app_handle, "event: Exit");
                if let Some(state) = app_handle.try_state::<ServerState>() {
                    if let Ok(mut guard) = state.0.lock() {
                        if let Some(child) = guard.as_mut() {
                            kill_tree(child);
                        }
                    }
                }
            }
            tauri::RunEvent::WindowEvent { label, event, .. } => match event {
                tauri::WindowEvent::CloseRequested { .. } => {
                    let url = window_url(&app_handle, &label);
                    log_line(&app_handle, &format!("window {label}: CloseRequested url={url}"));
                }
                tauri::WindowEvent::Destroyed => {
                    log_line(&app_handle, &format!("window {label}: Destroyed"));
                }
                _ => {}
            },
            _ => {}
        });
}