use notify_rust::Notification;
use serde::{Deserialize, Serialize};
use std::sync::mpsc;
use std::time::{Duration, Instant};
use tray_icon::menu::{Menu, MenuEvent, MenuItem};
use tray_icon::{Icon, TrayIconBuilder, TrayIconEvent};

// ─── Win32 FFI（消息泵 + 提示音）───

#[repr(C)]
struct POINT {
    x: i32,
    y: i32,
}

#[repr(C)]
struct MSG {
    hwnd: *mut core::ffi::c_void,
    message: u32,
    wparam: usize,
    lparam: isize,
    time: u32,
    pt: POINT,
}

#[link(name = "user32")]
unsafe extern "system" {
    unsafe fn PeekMessageW(
        msg: *mut MSG,
        hwnd: *mut core::ffi::c_void,
        filter_min: u32,
        filter_max: u32,
        remove_msg: u32,
    ) -> i32;
    unsafe fn TranslateMessage(msg: *const MSG) -> i32;
    unsafe fn DispatchMessageW(msg: *const MSG) -> i32;
    unsafe fn MessageBeep(utype: u32) -> i32;
    unsafe fn ShowWindow(hwnd: *mut core::ffi::c_void, cmd: i32) -> i32;
    unsafe fn IsWindowVisible(hwnd: *mut core::ffi::c_void) -> i32;
}

#[link(name = "kernel32")]
unsafe extern "system" {
    unsafe fn GetConsoleWindow() -> *mut core::ffi::c_void;
}

const PM_REMOVE: u32 = 0x0001;
const MB_ICONINFORMATION: u32 = 0x00000040;
const SW_HIDE: i32 = 0;
const SW_SHOW: i32 = 5;

fn toggle_console() {
    unsafe {
        let hwnd = GetConsoleWindow();
        if !hwnd.is_null() {
            if IsWindowVisible(hwnd) != 0 {
                ShowWindow(hwnd, SW_HIDE);
            } else {
                ShowWindow(hwnd, SW_SHOW);
            }
        }
    }
}

fn hide_console() {
    unsafe {
        let hwnd = GetConsoleWindow();
        if !hwnd.is_null() {
            ShowWindow(hwnd, SW_HIDE);
        }
    }
}

// ─── 配置 ───

#[derive(Serialize, Deserialize)]
struct Config {
    network: NetworkConfig,
}

#[derive(Serialize, Deserialize)]
struct NetworkConfig {
    port: u16,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            network: NetworkConfig { port: 13587 },
        }
    }
}

fn config_path() -> std::path::PathBuf {
    std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .join("receiver_config.toml")
}

fn load_config() -> Config {
    let path = config_path();
    if path.exists() {
        let content = std::fs::read_to_string(&path).unwrap_or_default();
        toml::from_str(&content).unwrap_or_default()
    } else {
        let config = Config::default();
        let content = toml::to_string_pretty(&config).unwrap();
        let _ = std::fs::write(&path, content);
        println!("已生成默认配置: {}", path.display());
        config
    }
}

// ─── 图标 ───

fn create_icon(r: u8, g: u8, b: u8) -> Icon {
    let size = 32u32;
    let rgba: Vec<u8> = [r, g, b, 255u8].repeat((size * size) as usize);
    Icon::from_rgba(rgba, size, size).unwrap()
}

// ─── 消息类型 ───

enum UdpMsg {
    Notification,
    Heartbeat,
}

fn main() {
    println!("=== 消息接收端 ===\n");

    let config = load_config();
    let port = config.network.port;
    println!("监听端口: {}", port);

    // 启动后隐藏控制台窗口
    hide_console();

    // 创建托盘菜单和图标
    let menu = Menu::new();
    let show_item = MenuItem::new("显示日志", true, None);
    let show_id = show_item.id().clone();
    let quit_item = MenuItem::new("退出", true, None);
    let quit_id = quit_item.id().clone();
    menu.append(&show_item).unwrap();
    menu.append(&quit_item).unwrap();

    let tray = TrayIconBuilder::new()
        .with_icon(create_icon(0, 180, 80))
        .with_tooltip("消息监控 - 等待连接")
        .with_menu(Box::new(menu))
        .build()
        .expect("无法创建托盘图标");

    println!("托盘图标已创建");

    // 启动 UDP 监听线程
    let (tx, rx) = mpsc::channel::<UdpMsg>();
    std::thread::spawn(move || {
        let socket = std::net::UdpSocket::bind(format!("0.0.0.0:{}", port)).unwrap_or_else(|e| {
            eprintln!("无法绑定端口 {}: {}", port, e);
            std::process::exit(1);
        });
        println!("UDP 监听已启动: 0.0.0.0:{}\n", port);

        let mut buf = [0u8; 64];
        loop {
            match socket.recv_from(&mut buf) {
                Ok((len, addr)) => {
                    let msg = std::str::from_utf8(&buf[..len]).unwrap_or("");
                    match msg {
                        "MSG" => {
                            println!("收到通知 (来自 {})", addr);
                            let _ = tx.send(UdpMsg::Notification);
                        }
                        "HB" => {
                            let _ = tx.send(UdpMsg::Heartbeat);
                        }
                        other => {
                            println!("未知消息: {} (来自 {})", other, addr);
                        }
                    }
                }
                Err(e) => eprintln!("接收错误: {}", e),
            }
        }
    });

    // 状态追踪
    let mut last_heartbeat = Instant::now();
    let mut connected = false;
    let mut msg_count = 0u64;

    // Win32 消息泵
    let mut win_msg: MSG = unsafe { std::mem::zeroed() };
    println!("等待监控端消息...\n");

    loop {
        // 处理 Windows 消息（托盘图标需要）
        unsafe {
            while PeekMessageW(&mut win_msg, std::ptr::null_mut(), 0, 0, PM_REMOVE) != 0 {
                TranslateMessage(&win_msg);
                DispatchMessageW(&win_msg);
            }
        }

        // 检查菜单事件
        if let Ok(event) = MenuEvent::receiver().try_recv() {
            if event.id == quit_id {
                println!("退出");
                break;
            }
            if event.id == show_id {
                toggle_console();
            }
        }

        // 双击托盘图标 → 切换控制台显示
        if let Ok(event) = TrayIconEvent::receiver().try_recv() {
            if matches!(event, TrayIconEvent::DoubleClick { .. }) {
                toggle_console();
            }
        }

        // 处理 UDP 消息
        while let Ok(msg) = rx.try_recv() {
            match msg {
                UdpMsg::Notification => {
                    msg_count += 1;
                    // 播放系统提示音
                    unsafe {
                        MessageBeep(MB_ICONINFORMATION);
                    }
                    // 显示 Toast 通知
                    let _ = Notification::new()
                        .summary("新消息提醒")
                        .body(&format!("检测到屏幕通知区域变化 (第 {} 次)", msg_count))
                        .show();
                }
                UdpMsg::Heartbeat => {
                    if !connected {
                        connected = true;
                        println!("监控端已连接");
                        let _ = tray.set_tooltip(Some("消息监控 - 已连接"));
                        let _ = tray.set_icon(Some(create_icon(0, 220, 80)));
                    }
                    last_heartbeat = Instant::now();
                }
            }
        }

        // 15 秒无心跳 → 断开
        if connected && last_heartbeat.elapsed() > Duration::from_secs(15) {
            connected = false;
            println!("监控端已断开");
            let _ = tray.set_tooltip(Some("消息监控 - 已断开"));
            let _ = tray.set_icon(Some(create_icon(128, 128, 128)));
        }

        std::thread::sleep(Duration::from_millis(50));
    }
}
