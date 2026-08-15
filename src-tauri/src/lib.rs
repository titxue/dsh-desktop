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
use tauri::menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::Manager;
use tauri::Url;
use tauri_plugin_autostart::ManagerExt;

/// 桌面壳 ↔ dsh 进程的通用 IPC 桥（Windows 管道 / POSIX unix socket）。
/// 接入点：M1 桥客户端线程消费事件、发送命令（见 docs/design-desktop-host.md）。
pub mod bridge;

/// CREATE_NO_WINDOW: keep the server and npm consoles hidden.
const CREATE_NO_WINDOW: u32 = 0x08000000;

/// dsh 服务进程的重启参数（托盘"重新启动服务"用）。
#[derive(Clone)]
struct ServerSpawn {
    node_exe: PathBuf,
    bin_js: PathBuf,
    deps: PathBuf,
    port: u16,
    desktop_patch: Option<PathBuf>,
    token: Option<String>,
    log_dir: PathBuf,
}

/// 服务子进程 + 重启参数；托盘"退出"时按此清理进程树。
#[derive(Default)]
struct ServerState {
    child: Mutex<Option<Child>>,
    spawn: Mutex<Option<ServerSpawn>>,
}

/// 托盘显示状态（单一事实来源：bridge 事件 → 这里 → 图标/菜单渲染）。
struct TrayState {
    phase: String, // idle | ready | error | off
    detail: String,
}

/// 壳侧本地设置（持久化到 app_config_dir/settings.json）。
#[derive(serde::Serialize, serde::Deserialize)]
struct ShellSettings {
    #[serde(default = "default_true")]
    minimize_to_tray: bool,
    #[serde(default)]
    launch_at_login: bool,
}

fn default_true() -> bool {
    true
}

impl Default for ShellSettings {
    fn default() -> Self {
        Self {
            minimize_to_tray: true,
            launch_at_login: false,
        }
    }
}

fn settings_path(app: &tauri::AppHandle) -> PathBuf {
    app.path()
        .app_config_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join("settings.json")
}

fn load_settings(app: &tauri::AppHandle) -> ShellSettings {
    std::fs::read_to_string(settings_path(app))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_settings(app: &tauri::AppHandle, settings: &ShellSettings) {
    if let Ok(dir) = app.path().app_config_dir() {
        let _ = create_dir_all(&dir);
        if let Ok(json) = serde_json::to_string_pretty(settings) {
            let _ = std::fs::write(dir.join("settings.json"), json);
        }
    }
}

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

/// 托盘状态图标（编译期嵌入，scripts/gen-tray-icons.mjs 生成）。
fn tray_icon_bytes(phase: &str) -> &'static [u8] {
    match phase {
        "ready" => include_bytes!("../icons/tray/tray-ready.png"),
        "error" => include_bytes!("../icons/tray/tray-error.png"),
        "off" => include_bytes!("../icons/tray/tray-off.png"),
        _ => include_bytes!("../icons/tray/tray-idle.png"),
    }
}

/// 重建托盘菜单（状态行由 TrayState 动态生成）。
fn build_tray_menu(app: &tauri::AppHandle, state: &TrayState) -> tauri::Result<Menu<tauri::Wry>> {
    let status_text = if state.detail.is_empty() {
        format!("状态：{}", state.phase)
    } else {
        format!("状态：{} · {}", state.phase, state.detail)
    };
    let show = MenuItem::with_id(app, "show", "显示主窗口", true, None::<&str>)?;
    let status = MenuItem::with_id(app, "status", status_text, false, None::<&str>)?;
    let open = MenuItem::with_id(app, "open", "打开主界面", true, None::<&str>)?;
    let restart = MenuItem::with_id(app, "restart", "重新启动服务", true, None::<&str>)?;
    let data_dir = MenuItem::with_id(app, "data-dir", "打开数据目录", true, None::<&str>)?;
    let log_dir = MenuItem::with_id(app, "log-dir", "打开日志目录", true, None::<&str>)?;
    // M3: 复选设置项（状态来自 autostart 插件 / 本地 settings.json）
    let settings = load_settings(app);
    let autostart_on = app.autolaunch().is_enabled().unwrap_or(false);
    let autostart = CheckMenuItem::with_id(app, "autostart", "开机自启", true, autostart_on, None::<&str>)?;
    let minimize = CheckMenuItem::with_id(app, "minimize", "关闭时最小化到托盘", true, settings.minimize_to_tray, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "退出", true, None::<&str>)?;
    Menu::with_items(
        app,
        &[
            &show,
            &status,
            &PredefinedMenuItem::separator(app)?,
            &open,
            &restart,
            &data_dir,
            &log_dir,
            &PredefinedMenuItem::separator(app)?,
            &autostart,
            &minimize,
            &PredefinedMenuItem::separator(app)?,
            &quit,
        ],
    )
}

/// 按当前 TrayState 重建菜单（设置项切换后刷新复选状态）。
fn refresh_tray_menu(app: &tauri::AppHandle) {
    if let Some(state) = app.try_state::<Mutex<TrayState>>() {
        if let Ok(guard) = state.lock() {
            if let Ok(menu) = build_tray_menu(app, &guard) {
                if let Some(tray) = app.tray_by_id("main-tray") {
                    let _ = tray.set_menu(Some(menu));
                }
            }
        }
    }
}

/// 更新托盘：图标状态 + 状态行（bridge 事件与本地事件共用）。
fn set_tray_phase(app: &tauri::AppHandle, phase: &str, detail: &str) {
    if let Some(state) = app.try_state::<Mutex<TrayState>>() {
        if let Ok(mut guard) = state.lock() {
            guard.phase = phase.to_string();
            guard.detail = detail.to_string();
        }
    }
    let Some(tray) = app.tray_by_id("main-tray") else {
        return;
    };
    if let Ok(image) = tauri::image::Image::from_bytes(tray_icon_bytes(phase)) {
        let _ = tray.set_icon(Some(image));
    }
    if let Some(state) = app.try_state::<Mutex<TrayState>>() {
        if let Ok(guard) = state.lock() {
            if let Ok(menu) = build_tray_menu(app, &guard) {
                let _ = tray.set_menu(Some(menu));
            }
        }
    }
}

/// 系统通知（桥 notification 事件 / 本地提示）。
fn notify(app: &tauri::AppHandle, title: &str, body: &str) {
    use tauri_plugin_notification::NotificationExt;
    log_line(app, &format!("notify: {title} — {body}"));
    let _ = app.notification().builder().title(title).body(body).show();
}

/// 用系统文件管理器打开目录。
fn open_path(path: &Path) {
    #[cfg(target_os = "windows")]
    let _ = Command::new("explorer").arg(path).spawn();
    #[cfg(target_os = "macos")]
    let _ = Command::new("open").arg(path).spawn();
    #[cfg(target_os = "linux")]
    let _ = Command::new("xdg-open").arg(path).spawn();
}

/// 显示并聚焦主窗口（托盘左键/菜单"显示"）。
fn show_main_window(app: &tauri::AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
    }
}

/// spawn dsh 服务进程（首次启动与托盘"重启"共用）。
fn spawn_server(spawn: &ServerSpawn) -> std::io::Result<Child> {
    let mut cmd = Command::new(win_clean(&spawn.node_exe));
    cmd.arg(win_clean(&spawn.bin_js));
    match &spawn.token {
        Some(token) => {
            cmd.arg("--profile")
                .arg("web")
                .arg("--patch")
                .arg(win_clean(spawn.desktop_patch.as_ref().unwrap()))
                .arg("--port")
                .arg(spawn.port.to_string())
                .env("DSH_DESKTOP_TOKEN", token);
        }
        None => {
            cmd.arg("web").arg("--port").arg(spawn.port.to_string());
        }
    }
    cmd.current_dir(win_clean(&spawn.deps))
        .creation_flags(CREATE_NO_WINDOW)
        .stdout(
            File::create(spawn.log_dir.join("server.out.log"))
                .map(Stdio::from)
                .unwrap_or_else(|_| Stdio::null()),
        )
        .stderr(
            File::create(spawn.log_dir.join("server.err.log"))
                .map(Stdio::from)
                .unwrap_or_else(|_| Stdio::null()),
        );
    cmd.spawn()
}

/// 托盘"重新启动服务"：杀旧进程树 → 重新 spawn（bootstrap 缓存命中，秒级恢复）。
fn restart_server(app: &tauri::AppHandle) {
    let Some(state) = app.try_state::<ServerState>() else {
        return;
    };
    let spawn = state.spawn.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let Some(spawn) = spawn else {
        log_line(app, "restart: no spawn args yet");
        return;
    };
    {
        let mut guard = state.child.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(child) = guard.as_mut() {
            kill_tree(child);
        }
        *guard = None;
    }
    match spawn_server(&spawn) {
        Ok(child) => {
            if let Ok(mut guard) = state.child.lock() {
                *guard = Some(child);
            }
            set_tray_phase(app, "idle", "重启中…");
            log_line(app, "server restarted");
        }
        Err(e) => {
            set_tray_phase(app, "error", &format!("重启失败: {e}"));
            log_line(app, &format!("restart failed: {e}"));
        }
    }
}

/// 托盘菜单点击分发（id 见 build_tray_menu）。
fn handle_tray_menu(app: &tauri::AppHandle, id: &str) {
    log_line(app, &format!("tray menu: {id}"));
    match id {
        "show" | "open" => show_main_window(app),
        "restart" => restart_server(app),
        "data-dir" => {
            if let Ok(dir) = app.path().app_local_data_dir() {
                open_path(&dir);
            }
        }
        "log-dir" => {
            if let Ok(dir) = app.path().app_log_dir() {
                open_path(&dir);
            }
        }
        "autostart" => {
            // 开机自启：切换 autostart 插件状态并刷新菜单复选
            let enabled = app.autolaunch().is_enabled().unwrap_or(false);
            let result = if enabled {
                app.autolaunch().disable()
            } else {
                app.autolaunch().enable()
            };
            if let Err(e) = result {
                log_line(app, &format!("autostart toggle failed: {e}"));
            }
            refresh_tray_menu(app);
        }
        "minimize" => {
            // 关闭时最小化到托盘：切换本地设置
            let mut settings = load_settings(app);
            settings.minimize_to_tray = !settings.minimize_to_tray;
            save_settings(app, &settings);
            refresh_tray_menu(app);
        }
        "quit" => {
            // M3 优雅关闭：先通知插件侧清理，等 2s，再强杀兜底退出
            log_line(app, "tray quit requested (graceful)");
            if let Some(state) = app.try_state::<ServerState>() {
                if let Ok(guard) = state.spawn.lock() {
                    if let Some(spawn) = guard.as_ref() {
                        if let Some(token) = &spawn.token {
                            if let Ok(mut client) = bridge::BridgeClient::connect_with_retry(&bridge::endpoint(token)) {
                                let _ = client.send_message(
                                    &serde_json::json!({ "type": "shutdown-request", "graceful": true }),
                                );
                                log_line(app, "shutdown-request sent to plugin");
                            }
                        }
                    }
                }
            }
            std::thread::sleep(Duration::from_secs(2));
            app.exit(0);
        }
        _ => {}
    }
}

/// 生成桥 token：sha256(时间戳 + pid + 计数器)。端点名即认证，仅防本机
/// 其他进程猜测；非密码学级随机但足够（管道名不可枚举）。
fn make_token() -> String {
    use sha2::{Digest, Sha256};
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let mut hasher = Sha256::new();
    hasher.update(now.to_le_bytes());
    hasher.update(std::process::id().to_le_bytes());
    hasher.update(COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed).to_le_bytes());
    format!("{:x}", hasher.finalize())
}

/// 桥事件循环：消费插件侧事件（state/log/notification…），驱动窗口导航。
/// 断线自动重连；M1 阶段 notification 仅记日志（M2 接系统通知）。
fn bridge_loop(
    mut client: bridge::BridgeClient,
    window: tauri::WebviewWindow,
    handle: tauri::AppHandle,
) {
    loop {
        match client.recv_message() {
            Ok(message) => {
                let kind = message.get("type").and_then(|v| v.as_str()).unwrap_or("");
                match kind {
                    "state" => {
                        let phase = message.get("phase").and_then(|v| v.as_str()).unwrap_or("");
                        if phase == "ready" {
                            let host = message
                                .get("host")
                                .and_then(|v| v.as_str())
                                .unwrap_or("127.0.0.1");
                            let port = message.get("port").and_then(|v| v.as_u64()).unwrap_or(0);
                            if port > 0 {
                                let url = format!("http://{host}:{port}");
                                log_line(&handle, &format!("bridge: ready, navigating to {url}"));
                                set_tray_phase(&handle, "ready", &format!("端口 {port}"));
                                if let Ok(url) = Url::parse(&url) {
                                    let _ = window.navigate(url);
                                    let _ = client.send_message(
                                        &serde_json::json!({ "type": "nav-result", "ok": true }),
                                    );
                                }
                            }
                        } else if phase == "error" {
                            let detail = message
                                .get("detail")
                                .and_then(|v| v.as_str())
                                .unwrap_or("未知错误");
                            log_line(&handle, &format!("bridge: server error: {detail}"));
                            set_tray_phase(&handle, "error", detail);
                            show_error(&window, &format!("本地服务启动失败：{detail}"));
                        }
                    }
                    "log" => {
                        if let Some(line) = message.get("line").and_then(|v| v.as_str()) {
                            log_line(&handle, &format!("bridge: {line}"));
                        }
                    }
                    "notification" => {
                        let title = message
                            .get("title")
                            .and_then(|v| v.as_str())
                            .unwrap_or("");
                        let body = message
                            .get("body")
                            .and_then(|v| v.as_str())
                            .unwrap_or("");
                        notify(&handle, title, body);
                    }
                    _ => {
                        log_line(&handle, &format!("bridge: event {}", kind));
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                log_line(&handle, "bridge: connection closed, reconnecting…");
                set_tray_phase(&handle, "off", "桥连接断开");
                match client.reconnect() {
                    Ok(_) => {
                        log_line(&handle, "bridge: reconnected");
                        set_tray_phase(&handle, "idle", "已重连");
                    }
                    Err(e) => {
                        log_line(&handle, &format!("bridge: reconnect failed: {e}"));
                        break;
                    }
                }
            }
            Err(e) => {
                log_line(&handle, &format!("bridge: read error: {e}"));
                set_tray_phase(&handle, "off", "桥异常");
                break;
            }
        }
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

    // M1: 发行版组合包叠加层存在时启用插件化启动（--profile web --patch）。
    // 桥 token 经环境变量注入，插件侧同名环境变量检测到才挂桥（纯 web 模式降级）。
    let desktop_patch = bootstrap_dir.join("desktop.yml");
    let bridge_token = desktop_patch.exists().then(make_token);

    let spawn = ServerSpawn {
        node_exe: node_exe.clone(),
        bin_js,
        deps: deps.clone(),
        port,
        desktop_patch: desktop_patch.exists().then(|| desktop_patch.clone()),
        token: bridge_token.clone(),
        log_dir: log_dir.clone(),
    };
    let child = match spawn_server(&spawn) {
        Ok(c) => c,
        Err(e) => return fail(&window, format!("server 启动失败: {e}")),
    };
    handle.manage(ServerState {
        child: Mutex::new(Some(child)),
        spawn: Mutex::new(Some(spawn)),
    });
    set_tray_phase(&handle, "idle", "服务启动中…");

    // 桥线程：连接插件侧桥服务，ready 事件驱动导航（wait_for_port 保留为兜底）
    let bridge_window = window.clone();
    if let Some(token) = bridge_token {
        let bridge_handle = handle.clone();
        std::thread::spawn(move || {
            let endpoint = bridge::endpoint(&token);
            std::thread::sleep(Duration::from_secs(1)); // 等子进程起来
            log_line(&bridge_handle, &format!("bridge: connecting to {endpoint}"));
            match bridge::BridgeClient::connect_with_retry(&endpoint) {
                Ok(client) => bridge_loop(client, bridge_window, bridge_handle),
                Err(e) => {
                    log_line(&bridge_handle, &format!("bridge: connect failed: {e}"));
                    set_tray_phase(&bridge_handle, "off", "桥连接失败");
                }
            }
        });
    }

    std::thread::spawn(move || {
        if wait_for_port(port, Duration::from_secs(120)) {
            let url = format!("http://127.0.0.1:{port}");
            log_line(&handle, &format!("server ready, navigating to {url}"));
            send_progress(&shared, &window, ProgressState::new("ready", "", None, ""));
            set_tray_phase(&handle, "ready", &format!("端口 {port}"));
            if let Ok(url) = Url::parse(&url) {
                let _ = window.navigate(url);
            }
        } else {
            log_line(&handle, "server did not become ready in 120s");
            set_tray_phase(&handle, "error", "服务 120s 未就绪");
            show_error(&window, "本地服务在 120 秒内未就绪，请查看 logs\\server.err.log。");
        }
    });
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            // M3: 二次启动 → 聚焦已有窗口并提示
            log_line(app, "second instance detected, focusing main window");
            show_main_window(app);
            notify(app, "DeepSeek Harness 已在运行", "已切换到现有窗口。");
        }))
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_autostart::Builder::new().build())
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

            // 系统托盘（M2）：图标状态 + 菜单模型；左键单击显示窗口
            app.manage(Mutex::new(TrayState {
                phase: "idle".to_string(),
                detail: "启动中…".to_string(),
            }));
            let initial_tray = TrayState {
                phase: "idle".to_string(),
                detail: "启动中…".to_string(),
            };
            let tray = TrayIconBuilder::with_id("main-tray")
                .icon(tauri::image::Image::from_bytes(tray_icon_bytes("idle"))?)
                .menu(&build_tray_menu(app.handle(), &initial_tray)?)
                .show_menu_on_left_click(false)
                .on_menu_event(|app, event| {
                    handle_tray_menu(app, event.id.as_ref());
                })
                .on_tray_icon_event(|tray, event| {
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } = event
                    {
                        show_main_window(tray.app_handle());
                    }
                })
                .build(app)?;
            app.manage(tray);

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
                    if let Ok(mut guard) = state.child.lock() {
                        if let Some(child) = guard.as_mut() {
                            kill_tree(child);
                        }
                    }
                }
            }
            tauri::RunEvent::WindowEvent { label, event, .. } => match event {
                tauri::WindowEvent::CloseRequested { api, .. } => {
                    let url = window_url(&app_handle, &label);
                    log_line(&app_handle, &format!("window {label}: CloseRequested url={url}"));
                    // M2/M3: 关闭按钮行为由"最小化到托盘"设置决定
                    let settings = load_settings(&app_handle);
                    if settings.minimize_to_tray {
                        api.prevent_close();
                        if let Some(window) = app_handle.get_webview_window(&label) {
                            let _ = window.hide();
                        }
                        notify(
                            &app_handle,
                            "DeepSeek Harness 仍在运行",
                            "已最小化到系统托盘，点击托盘图标可重新打开。",
                        );
                    }
                }
                tauri::WindowEvent::Destroyed => {
                    log_line(&app_handle, &format!("window {label}: Destroyed"));
                }
                _ => {}
            },
            _ => {}
        });
}