use image::{imageops::FilterType, RgbaImage};
use minifb::{Key, MouseButton, MouseMode, Window, WindowOptions};
use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::net::UdpSocket;
use std::path::PathBuf;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::{Duration, Instant};
use xcap::Monitor;

extern crate native_windows_gui as nwg;

// ─── 配置结构 ───

#[derive(Serialize, Deserialize, Clone)]
struct Config {
    capture: CaptureArea,
    network: NetworkConfig,
    monitor: MonitorSettings,
}

#[derive(Serialize, Deserialize, Clone)]
struct CaptureArea {
    x: u32,
    y: u32,
    width: u32,
    height: u32,
}

impl CaptureArea {
    fn summary(&self) -> String {
        if self.width == 0 && self.height == 0 {
            "未设置".into()
        } else {
            format!("({}, {}) {}x{}", self.x, self.y, self.width, self.height)
        }
    }
}

#[derive(Serialize, Deserialize, Clone)]
struct NetworkConfig {
    target: String,
    port: u16,
}

#[derive(Serialize, Deserialize, Clone)]
struct MonitorSettings {
    interval_ms: u64,
    threshold: u64,
    cooldown_ms: u64,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            capture: CaptureArea {
                x: 0,
                y: 0,
                width: 0,
                height: 0,
            },
            network: NetworkConfig {
                target: "255.255.255.255".into(),
                port: 13587,
            },
            monitor: MonitorSettings {
                interval_ms: 500,
                threshold: 5000,
                cooldown_ms: 3000,
            },
        }
    }
}

fn config_path() -> PathBuf {
    std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .join("monitor_config.toml")
}

fn load_config() -> Config {
    config_path()
        .exists()
        .then(|| {
            let content = std::fs::read_to_string(config_path()).ok()?;
            toml::from_str(&content).ok()
        })
        .flatten()
        .unwrap_or_default()
}

fn save_config(config: &Config) {
    let content = toml::to_string_pretty(config).unwrap();
    std::fs::write(config_path(), content).unwrap();
}

// ─── 图像工具 ───

fn rgba_to_0rgb(image: &RgbaImage) -> Vec<u32> {
    image
        .pixels()
        .map(|p| {
            let [r, g, b, _] = p.0;
            ((r as u32) << 16) | ((g as u32) << 8) | (b as u32)
        })
        .collect()
}

fn dim_buffer(buf: &[u32]) -> Vec<u32> {
    buf.iter()
        .map(|&p| {
            let r = ((p >> 16) & 0xFF) * 3 / 5;
            let g = ((p >> 8) & 0xFF) * 3 / 5;
            let b = (p & 0xFF) * 3 / 5;
            (r << 16) | (g << 8) | b
        })
        .collect()
}

// ─── 区域选择器 ───

fn draw_overlay(
    display: &mut [u32],
    original: &[u32],
    dimmed: &[u32],
    width: usize,
    sel: Option<(usize, usize, usize, usize)>,
) {
    display.copy_from_slice(dimmed);
    if let Some((x1, y1, x2, y2)) = sel {
        for y in y1..=y2 {
            let start = y * width + x1;
            let end = y * width + x2 + 1;
            display[start..end].copy_from_slice(&original[start..end]);
        }
        let red = 0x00FF0000u32;
        for t in 0..=1usize {
            for x in x1..=x2 {
                display[(y1 + t) * width + x] = red;
                display[y2.saturating_sub(t) * width + x] = red;
            }
            for y in y1..=y2 {
                display[y * width + x1 + t] = red;
                display[y * width + x2.saturating_sub(t)] = red;
            }
        }
    }
}

fn get_primary_monitor() -> Monitor {
    let monitors = Monitor::all().expect("无法获取显示器列表");
    monitors
        .into_iter()
        .find(|m| m.is_primary().unwrap_or(false))
        .or_else(|| Monitor::all().ok()?.into_iter().next())
        .expect("未找到显示器")
}

fn take_screenshot() -> (RgbaImage, Monitor) {
    let monitor = get_primary_monitor();
    let screenshot = monitor.capture_image().expect("截图失败");
    (screenshot, monitor)
}

fn run_selector(screenshot: RgbaImage, monitor: &Monitor) -> Option<CaptureArea> {
    let phys_w = screenshot.width();
    let phys_h = screenshot.height();

    let win_w = monitor.width().unwrap_or(phys_w) as usize;
    let win_h = monitor.height().unwrap_or(phys_h) as usize;
    let scale_x = phys_w as f64 / win_w as f64;
    let scale_y = phys_h as f64 / win_h as f64;

    let display_image = if win_w != phys_w as usize || win_h != phys_h as usize {
        image::imageops::resize(
            &screenshot,
            win_w as u32,
            win_h as u32,
            FilterType::Triangle,
        )
    } else {
        screenshot
    };

    let base_buffer = rgba_to_0rgb(&display_image);
    let dim_buf = dim_buffer(&base_buffer);
    let mut display_buffer = vec![0u32; win_w * win_h];

    let mut window = Window::new(
        "按住左键框选监控区域 | ESC取消",
        win_w,
        win_h,
        WindowOptions {
            borderless: true,
            topmost: true,
            ..WindowOptions::default()
        },
    )
    .expect("无法创建选区窗口");

    window.set_position(0, 0);
    window.set_target_fps(60);

    let mut drag_start: Option<(usize, usize)> = None;
    let mut selection: Option<(usize, usize, usize, usize)> = None;

    loop {
        if !window.is_open() || window.is_key_down(Key::Escape) {
            return None;
        }

        if let Some((mx, my)) = window.get_mouse_pos(MouseMode::Clamp) {
            let mx = (mx as usize).min(win_w.saturating_sub(1));
            let my = (my as usize).min(win_h.saturating_sub(1));

            if window.get_mouse_down(MouseButton::Left) {
                if drag_start.is_none() {
                    drag_start = Some((mx, my));
                }
                if let Some((sx, sy)) = drag_start {
                    selection = Some((sx.min(mx), sy.min(my), sx.max(mx), sy.max(my)));
                }
            } else if drag_start.take().is_some() {
                if let Some((x1, y1, x2, y2)) = selection {
                    if x2 > x1 + 10 && y2 > y1 + 10 {
                        return Some(CaptureArea {
                            x: (x1 as f64 * scale_x) as u32,
                            y: (y1 as f64 * scale_y) as u32,
                            width: ((x2 - x1) as f64 * scale_x) as u32,
                            height: ((y2 - y1) as f64 * scale_y) as u32,
                        });
                    }
                }
                selection = None;
            }
        }

        draw_overlay(
            &mut display_buffer,
            &base_buffer,
            &dim_buf,
            win_w,
            selection,
        );
        window
            .update_with_buffer(&display_buffer, win_w, win_h)
            .unwrap();
    }
}

// ─── 监控循环（后台线程）───

fn capture_region(monitor: &Monitor, area: &CaptureArea) -> Option<Vec<u8>> {
    let region = monitor
        .capture_region(area.x, area.y, area.width, area.height)
        .ok()?;
    let small = image::imageops::resize(
        &region,
        (area.width / 4).max(1),
        (area.height / 4).max(1),
        FilterType::Triangle,
    );
    Some(small.into_raw())
}

fn pixel_diff(a: &[u8], b: &[u8]) -> u64 {
    a.iter()
        .zip(b.iter())
        .map(|(&x, &y)| (x as i64 - y as i64).unsigned_abs())
        .sum()
}

fn start_monitoring(config: Config, running: Arc<AtomicBool>) {
    std::thread::spawn(move || {
        let socket = UdpSocket::bind("0.0.0.0:0").expect("无法绑定 UDP");
        socket.set_broadcast(true).ok();
        let target = format!("{}:{}", config.network.target, config.network.port);

        let monitor = get_primary_monitor();
        eprintln!(
            "[调试] 监控区域: ({}, {}) {}x{}",
            config.capture.x, config.capture.y, config.capture.width, config.capture.height
        );
        eprintln!("[调试] 目标地址: {}", target);

        // 保存初始截图用于调试
        match monitor.capture_region(
            config.capture.x,
            config.capture.y,
            config.capture.width,
            config.capture.height,
        ) {
            Ok(img) => {
                let debug_path = std::env::current_exe()
                    .unwrap()
                    .parent()
                    .unwrap()
                    .join("debug_capture.png");
                let _ = img.save(&debug_path);
                eprintln!("[调试] 初始截图已保存: {}", debug_path.display());
            }
            Err(e) => {
                eprintln!("[调试] capture_region 失败: {}", e);
            }
        }

        let mut last_capture = match capture_region(&monitor, &config.capture) {
            Some(c) => {
                eprintln!("[调试] 初始截图成功, 数据长度: {}", c.len());
                c
            }
            None => {
                eprintln!("[调试] 初始截图失败!");
                running.store(false, Ordering::SeqCst);
                return;
            }
        };
        let mut last_notify = Instant::now() - Duration::from_secs(60);
        let mut last_hb = Instant::now();
        let mut tick = 0u64;

        while running.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(config.monitor.interval_ms));

            if last_hb.elapsed() >= Duration::from_secs(5) {
                let _ = socket.send_to(b"HB", &target);
                last_hb = Instant::now();
            }

            let Some(current) = capture_region(&monitor, &config.capture) else {
                eprintln!("[调试] 截图失败 tick={}", tick);
                continue;
            };
            let d = pixel_diff(&last_capture, &current);
            tick += 1;

            // 前 10 次每次都打印 diff 值，之后每 20 次打印一次
            if tick <= 10 || tick % 20 == 0 {
                eprintln!(
                    "[调试] tick={} diff={} threshold={}",
                    tick, d, config.monitor.threshold
                );
            }

            last_capture = current;

            if d > config.monitor.threshold
                && last_notify.elapsed() >= Duration::from_millis(config.monitor.cooldown_ms)
            {
                eprintln!("[通知] diff={} 超过阈值, 发送 MSG 到 {}", d, target);
                let _ = socket.send_to(b"MSG", &target);
                last_notify = Instant::now();
            }
        }
        eprintln!("[调试] 监控循环已停止");
    });
}

// ─── GUI ───

fn main() {
    nwg::init().expect("无法初始化 NWG");

    let mut font = nwg::Font::default();
    nwg::Font::builder()
        .family("Microsoft YaHei")
        .size(16)
        .build(&mut font)
        .ok();
    nwg::Font::set_global_default(Some(font));

    // ─── 主窗口 ───
    let mut window = nwg::Window::default();
    nwg::Window::builder()
        .size((420, 380))
        .center(true)
        .title("消息监控端")
        .flags(
            nwg::WindowFlags::WINDOW | nwg::WindowFlags::VISIBLE | nwg::WindowFlags::MINIMIZE_BOX,
        )
        .build(&mut window)
        .unwrap();

    // ─── 托盘图标 ───
    let mut tray_icon = nwg::TrayNotification::default();
    let mut icon = nwg::Icon::default();
    // 使用嵌入的默认图标 (蓝色方块)
    nwg::Icon::builder()
        .source_embed(None)
        .source_system(Some(nwg::OemIcon::Information))
        .build(&mut icon)
        .unwrap();
    nwg::TrayNotification::builder()
        .parent(&window)
        .icon(Some(&icon))
        .tip(Some("消息监控端"))
        .build(&mut tray_icon)
        .unwrap();

    // ─── 区域选择 ───
    let mut lbl_area_title = nwg::Label::default();
    nwg::Label::builder()
        .text("监控区域")
        .parent(&window)
        .position((20, 20))
        .size((80, 25))
        .build(&mut lbl_area_title)
        .unwrap();

    let mut lbl_area = nwg::Label::default();
    nwg::Label::builder()
        .text("未设置")
        .parent(&window)
        .position((100, 20))
        .size((200, 25))
        .build(&mut lbl_area)
        .unwrap();

    let mut btn_select = nwg::Button::default();
    nwg::Button::builder()
        .text("选择区域")
        .parent(&window)
        .position((310, 15))
        .size((90, 30))
        .build(&mut btn_select)
        .unwrap();

    // ─── 目标 IP ───
    let mut lbl_target = nwg::Label::default();
    nwg::Label::builder()
        .text("接收端 IP")
        .parent(&window)
        .position((20, 65))
        .size((80, 25))
        .build(&mut lbl_target)
        .unwrap();

    let mut txt_target = nwg::TextInput::default();
    nwg::TextInput::builder()
        .text("255.255.255.255")
        .parent(&window)
        .position((100, 60))
        .size((300, 28))
        .build(&mut txt_target)
        .unwrap();

    // ─── 端口 ───
    let mut lbl_port = nwg::Label::default();
    nwg::Label::builder()
        .text("端口")
        .parent(&window)
        .position((20, 105))
        .size((80, 25))
        .build(&mut lbl_port)
        .unwrap();

    let mut txt_port = nwg::TextInput::default();
    nwg::TextInput::builder()
        .text("13587")
        .parent(&window)
        .position((100, 100))
        .size((100, 28))
        .build(&mut txt_port)
        .unwrap();

    // ─── 检测间隔 ───
    let mut lbl_interval = nwg::Label::default();
    nwg::Label::builder()
        .text("间隔(ms)")
        .parent(&window)
        .position((20, 145))
        .size((80, 25))
        .build(&mut lbl_interval)
        .unwrap();

    let mut txt_interval = nwg::TextInput::default();
    nwg::TextInput::builder()
        .text("500")
        .parent(&window)
        .position((100, 140))
        .size((100, 28))
        .build(&mut txt_interval)
        .unwrap();

    // ─── 阈值 ───
    let mut lbl_thresh = nwg::Label::default();
    nwg::Label::builder()
        .text("阈值")
        .parent(&window)
        .position((220, 145))
        .size((60, 25))
        .build(&mut lbl_thresh)
        .unwrap();

    let mut txt_threshold = nwg::TextInput::default();
    nwg::TextInput::builder()
        .text("5000")
        .parent(&window)
        .position((280, 140))
        .size((120, 28))
        .build(&mut txt_threshold)
        .unwrap();

    // ─── 冷却时间 ───
    let mut lbl_cooldown = nwg::Label::default();
    nwg::Label::builder()
        .text("冷却(ms)")
        .parent(&window)
        .position((20, 185))
        .size((80, 25))
        .build(&mut lbl_cooldown)
        .unwrap();

    let mut txt_cooldown = nwg::TextInput::default();
    nwg::TextInput::builder()
        .text("3000")
        .parent(&window)
        .position((100, 180))
        .size((100, 28))
        .build(&mut txt_cooldown)
        .unwrap();

    // ─── 操作按钮 ───
    let mut btn_start = nwg::Button::default();
    nwg::Button::builder()
        .text("开始监控")
        .parent(&window)
        .position((60, 230))
        .size((130, 40))
        .build(&mut btn_start)
        .unwrap();

    let mut btn_stop = nwg::Button::default();
    nwg::Button::builder()
        .text("停止监控")
        .enabled(false)
        .parent(&window)
        .position((230, 230))
        .size((130, 40))
        .build(&mut btn_stop)
        .unwrap();

    // ─── 状态栏 ───
    let mut lbl_status = nwg::Label::default();
    nwg::Label::builder()
        .text("状态: 就绪")
        .parent(&window)
        .position((20, 290))
        .size((380, 25))
        .build(&mut lbl_status)
        .unwrap();

    let mut lbl_count = nwg::Label::default();
    nwg::Label::builder()
        .text("")
        .parent(&window)
        .position((20, 320))
        .size((380, 25))
        .build(&mut lbl_count)
        .unwrap();

    // ─── 定时器（用于刷新状态）───
    let mut timer = nwg::AnimationTimer::default();
    nwg::AnimationTimer::builder()
        .interval(Duration::from_secs(1))
        .max_tick(None)
        .parent(&window)
        .build(&mut timer)
        .unwrap();
    timer.stop();

    // ─── 加载已有配置 ───
    let config = load_config();
    lbl_area.set_text(&config.capture.summary());
    txt_target.set_text(&config.network.target);
    txt_port.set_text(&config.network.port.to_string());
    txt_interval.set_text(&config.monitor.interval_ms.to_string());
    txt_threshold.set_text(&config.monitor.threshold.to_string());
    txt_cooldown.set_text(&config.monitor.cooldown_ms.to_string());

    // ─── 状态 ───
    let config = RefCell::new(config);
    let running = Arc::new(AtomicBool::new(false));

    // ─── 事件绑定 ───
    let window_handle = window.handle;
    let evt_handler =
        nwg::full_bind_event_handler(&window_handle, move |evt, _evt_data, handle| {
            use nwg::Event as E;
            match evt {
                E::OnButtonClick => {
                    if handle == btn_select.handle {
                        // 先截图（此时任务栏完全可见），再隐藏窗口
                        let (screenshot, mon) = take_screenshot();
                        window.set_visible(false);
                        std::thread::sleep(Duration::from_millis(200));
                        if let Some(area) = run_selector(screenshot, &mon) {
                            lbl_area.set_text(&area.summary());
                            config.borrow_mut().capture = area;
                        }
                        window.set_visible(true);
                        window.set_focus();
                    }
                    if handle == btn_start.handle {
                        // 读取界面值构建配置
                        let port: u16 = txt_port.text().parse().unwrap_or(13587);
                        let interval: u64 = txt_interval.text().parse().unwrap_or(500);
                        let threshold: u64 = txt_threshold.text().parse().unwrap_or(5000);
                        let cooldown: u64 = txt_cooldown.text().parse().unwrap_or(3000);

                        let mut cfg = config.borrow_mut();
                        cfg.network.target = txt_target.text();
                        cfg.network.port = port;
                        cfg.monitor.interval_ms = interval;
                        cfg.monitor.threshold = threshold;
                        cfg.monitor.cooldown_ms = cooldown;

                        if cfg.capture.width == 0 || cfg.capture.height == 0 {
                            nwg::modal_info_message(&window, "提示", "请先选择监控区域");
                            return;
                        }

                        save_config(&cfg);

                        let cfg_clone = cfg.clone();
                        running.store(true, Ordering::SeqCst);
                        start_monitoring(cfg_clone, running.clone());

                        btn_start.set_enabled(false);
                        btn_stop.set_enabled(true);
                        btn_select.set_enabled(false);
                        txt_target.set_readonly(true);
                        txt_port.set_readonly(true);
                        txt_interval.set_readonly(true);
                        txt_threshold.set_readonly(true);
                        txt_cooldown.set_readonly(true);
                        lbl_status.set_text("状态: 监控中...");
                        timer.start();
                    }
                    if handle == btn_stop.handle {
                        running.store(false, Ordering::SeqCst);
                        btn_start.set_enabled(true);
                        btn_stop.set_enabled(false);
                        btn_select.set_enabled(true);
                        txt_target.set_readonly(false);
                        txt_port.set_readonly(false);
                        txt_interval.set_readonly(false);
                        txt_threshold.set_readonly(false);
                        txt_cooldown.set_readonly(false);
                        lbl_status.set_text("状态: 已停止");
                        lbl_count.set_text("");
                        timer.stop();
                    }
                }
                E::OnTimerTick => {
                    if running.load(Ordering::SeqCst) {
                        lbl_status.set_text("状态: 监控中...");
                    } else {
                        lbl_status.set_text("状态: 已停止（异常）");
                        btn_start.set_enabled(true);
                        btn_stop.set_enabled(false);
                        btn_select.set_enabled(true);
                        timer.stop();
                    }
                }
                E::OnWindowMinimize => {
                    window.set_visible(false);
                }
                E::OnContextMenu => {
                    if handle == tray_icon.handle {
                        // 双击或右键托盘图标 → 恢复窗口
                        window.set_visible(true);
                        window.set_focus();
                    }
                }
                E::OnWindowClose => {
                    running.store(false, Ordering::SeqCst);
                    nwg::stop_thread_dispatch();
                }
                _ => {}
            }
        });

    nwg::dispatch_thread_events();
    nwg::unbind_event_handler(&evt_handler);
}
