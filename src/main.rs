// 仅在 Release 模式下隐藏控制台窗口
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

#[cfg(not(windows))]
compile_error!(
    "etw-viewer 仅支持 Windows 目标平台（依赖 ETW 与 Win32 API）。\n\
     支持的 target: x86_64-pc-windows-msvc / aarch64-pc-windows-msvc / i686-pc-windows-msvc\n\
     Windows 上直接构建: cargo build --release\n\
     交叉编译: cargo build --release --target x86_64-pc-windows-msvc"
);

use etw_monitor::{DllEvent, EtwSession, MonitorEvent};
use gpui_kit::component::scroll::Scrollbar;
use gpui_kit::component::theme::{Theme, ThemeMode};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

const MAX_EVENTS: usize = 5000;
/// 北京时间偏移（UTC+8），单位秒
const LOCAL_OFFSET_SECS: u64 = 8 * 3600;

// ==================== 配置 ====================

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AppConfig {
    #[serde(default = "default_true")]
    auto_scroll: bool,
    #[serde(default = "default_true")]
    show_system: bool,
    #[serde(default = "default_true")]
    monitoring: bool,
}

fn default_true() -> bool {
    true
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            auto_scroll: true,
            show_system: true,
            monitoring: true,
        }
    }
}

fn config_path() -> PathBuf {
    if let Ok(dir) = std::env::var("APPDATA") {
        PathBuf::from(dir).join("etw-viewer").join("config.json")
    } else if let Ok(dir) = std::env::var("HOME") {
        PathBuf::from(dir)
            .join(".config")
            .join("etw-viewer")
            .join("config.json")
    } else {
        PathBuf::from("etw-viewer-config.json")
    }
}

fn load_config(path: &Path) -> AppConfig {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_config(path: &Path, cfg: &AppConfig) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(s) = serde_json::to_string_pretty(cfg) {
        let _ = std::fs::write(path, s);
    }
}

// ==================== 数据结构 ====================

struct StoredDll {
    ev: DllEvent,
    is_system: bool,
}

struct PidGroup {
    pid: u32,
    id: u64,
    dlls: VecDeque<StoredDll>,
}

struct NameGroup {
    name: String,
    id: u64,
    pids: Vec<PidGroup>,
    pid_index: HashMap<u32, usize>,
}

#[derive(Clone)]
enum RenderRow {
    Name {
        name_id: u64,
        name: String,
        pid_count: usize,
        total_dlls: usize,
        expanded: bool,
    },
    Pid {
        group_id: u64,
        pid: u32,
        name: String,
        count: usize,
        expanded: bool,
        last_ts: u64,
    },
    Dll {
        image_name: String,
        ts: u64,
        pid: u32,
        process_name: String,
    },
}

#[derive(Clone)]
enum MenuTarget {
    Name {
        name: String,
        pid_count: usize,
        total_dlls: usize,
    },
    Pid {
        pid: u32,
        name: String,
        count: usize,
        last_ts: u64,
    },
    Dll {
        image_name: String,
        ts: u64,
        pid: u32,
        process_name: String,
    },
}

#[derive(Clone, Copy)]
enum MenuAction {
    Detail,
    Reveal,
}

#[derive(Clone)]
struct MenuState {
    x: f32,
    y: f32,
    target: MenuTarget,
}

#[derive(Clone)]
struct DetailInfo {
    title: String,
    lines: Vec<(String, String)>,
}

struct EtwViewer {
    names: Vec<NameGroup>,
    name_index: HashMap<String, usize>,
    expanded_names: HashSet<u64>,
    expanded_pids: HashSet<u64>,
    auto_expanded_names: HashSet<u64>,
    auto_expanded_pids: HashSet<u64>,
    order: VecDeque<(String, u32, u64)>,
    next_id: u64,
    flat: Rc<Vec<RenderRow>>,

    show_system: bool,
    stick_to_bottom: bool,

    menu: Option<MenuState>,
    detail: Option<DetailInfo>,

    config_path: PathBuf,

    scroll_handle: UniformListScrollHandle,
    session: Option<EtwSession>,
    is_running: bool,
    error_msg: Option<String>,
}

// ==================== 工具函数 ====================

fn is_system_dll(path: &str) -> bool {
    let lower = path.to_lowercase();
    lower.starts_with("c:\\windows")
        || lower.contains("\\windows\\system32\\")
        || lower.contains("\\windows\\syswow64\\")
        || lower.contains("\\windows\\winsxs\\")
}

fn fmt_time(ts: u64) -> String {
    let secs = (ts / 1000 + LOCAL_OFFSET_SECS) % 86400;
    format!(
        "{:02}:{:02}:{:02}",
        secs / 3600,
        (secs % 3600) / 60,
        secs % 60
    )
}

fn fmt_datetime(ts: u64) -> String {
    let total_secs = ts / 1000 + LOCAL_OFFSET_SECS;
    let days = total_secs / 86400;
    let secs_of_day = total_secs % 86400;
    let (y, m, d) = civil_from_days(days as i64);
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        y,
        m,
        d,
        secs_of_day / 3600,
        (secs_of_day % 3600) / 60,
        secs_of_day % 60
    )
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn build_detail(target: &MenuTarget) -> DetailInfo {
    match target {
        MenuTarget::Name {
            name,
            pid_count,
            total_dlls,
        } => DetailInfo {
            title: format!("程序详情 · {}", name),
            lines: vec![
                ("程序名".into(), name.clone()),
                ("进程数".into(), pid_count.to_string()),
                ("模块数".into(), total_dlls.to_string()),
            ],
        },
        MenuTarget::Pid {
            pid,
            name,
            count,
            last_ts,
        } => DetailInfo {
            title: format!("进程详情 · {} (pid {})", name, pid),
            lines: vec![
                ("进程名".into(), name.clone()),
                ("PID".into(), pid.to_string()),
                ("可见模块数".into(), count.to_string()),
                (
                    "最近加载".into(),
                    if *last_ts == 0 {
                        "—".into()
                    } else {
                        fmt_datetime(*last_ts)
                    },
                ),
            ],
        },
        MenuTarget::Dll {
            image_name,
            ts,
            pid,
            process_name,
        } => DetailInfo {
            title: "模块详情".into(),
            lines: vec![
                ("进程名".into(), process_name.clone()),
                ("PID".into(), pid.to_string()),
                ("模块路径".into(), image_name.clone()),
                ("加载时间".into(), fmt_datetime(*ts)),
            ],
        },
    }
}

fn reveal_in_explorer(path: &str) {
    use std::path::Path;
    let p = Path::new(path);
    if p.exists() {
        #[cfg(target_os = "windows")]
        {
            let _ = std::process::Command::new("explorer")
                .arg(format!("/select,\"{}\"", path))
                .spawn();
        }
        #[cfg(target_os = "macos")]
        {
            let _ = std::process::Command::new("open")
                .arg("-R")
                .arg(path)
                .spawn();
        }
        #[cfg(all(unix, not(target_os = "macos")))]
        {
            if let Some(parent) = p.parent() {
                let _ = std::process::Command::new("xdg-open").arg(parent).spawn();
            }
        }
    } else if let Some(parent) = p.parent() {
        if parent.exists() {
            #[cfg(target_os = "windows")]
            {
                let _ = std::process::Command::new("explorer").arg(parent).spawn();
            }
            #[cfg(target_os = "macos")]
            {
                let _ = std::process::Command::new("open").arg(parent).spawn();
            }
            #[cfg(all(unix, not(target_os = "macos")))]
            {
                let _ = std::process::Command::new("xdg-open").arg(parent).spawn();
            }
        }
    }
}

// ==================== 业务逻辑 ====================

impl EtwViewer {
    fn new(cx: &mut Context<Self>) -> Self {
        let config_path = config_path();
        let cfg = load_config(&config_path);

        let mut this = Self {
            names: Vec::new(),
            name_index: HashMap::new(),
            expanded_names: HashSet::new(),
            expanded_pids: HashSet::new(),
            auto_expanded_names: HashSet::new(),
            auto_expanded_pids: HashSet::new(),
            order: VecDeque::new(),
            next_id: 0,
            flat: Rc::new(Vec::new()),
            show_system: cfg.show_system,
            stick_to_bottom: cfg.auto_scroll,
            menu: None,
            detail: None,
            config_path,
            scroll_handle: UniformListScrollHandle::new(),
            session: None,
            is_running: false,
            error_msg: None,
        };

        if cfg.monitoring {
            this.start_monitor(cx);
        }

        this
    }

    fn persist_config(&self) {
        let cfg = AppConfig {
            auto_scroll: self.stick_to_bottom,
            show_system: self.show_system,
            monitoring: self.is_running,
        };
        save_config(&self.config_path, &cfg);
    }

    fn reset_state(&mut self) {
        self.names.clear();
        self.name_index.clear();
        self.expanded_names.clear();
        self.expanded_pids.clear();
        self.auto_expanded_names.clear();
        self.auto_expanded_pids.clear();
        self.order.clear();
        self.flat = Rc::new(Vec::new());
        self.menu = None;
        self.detail = None;
    }

    fn start_monitor(&mut self, cx: &mut Context<Self>) {
        if self.is_running {
            return;
        }
        let (session, mut stream) = match EtwSession::start() {
            Ok(v) => v,
            Err(e) => {
                self.error_msg = Some(e.to_string());
                cx.notify();
                return;
            }
        };
        self.session = Some(session);
        self.is_running = true;
        self.error_msg = None;
        self.reset_state();

        cx.spawn(async move |this, cx| {
            let mut batch = Vec::with_capacity(100);
            loop {
                tokio::select! {
                    msg = stream.next() => {
                        match msg {
                            Some(MonitorEvent::Event(ev)) => {
                                batch.push(ev);
                                if batch.len() >= 100 {
                                    let _ = this.update(cx, |this, cx| {
                                        this.process_batch(&mut batch, cx);
                                    });
                                }
                            }
                            Some(MonitorEvent::Error(e)) => {
                                let _ = this.update(cx, |this, cx| {
                                    this.error_msg = Some(e.to_string());
                                    this.is_running = false;
                                    this.session = None;
                                    this.persist_config();
                                    cx.notify();
                                });
                                break;
                            }
                            None => break,
                        }
                    }
                    _ = cx.background_executor().timer(Duration::from_millis(50)) => {
                        if !batch.is_empty() {
                            let _ = this.update(cx, |this, cx| {
                                this.process_batch(&mut batch, cx);
                            });
                        }
                    }
                }
            }
        })
        .detach();
    }

    fn process_batch(&mut self, batch: &mut Vec<DllEvent>, cx: &mut Context<Self>) {
        if batch.is_empty() {
            return;
        }
        let events = std::mem::take(batch);
        let mut added = 0usize;

        for ev in events {
            let name = ev.process_name.clone();
            let pid = ev.pid;

            let ni = match self.name_index.get(&name) {
                Some(&i) => i,
                None => {
                    let i = self.names.len();
                    let id = self.next_id;
                    self.next_id += 1;
                    self.names.push(NameGroup {
                        name: name.clone(),
                        id,
                        pids: Vec::new(),
                        pid_index: HashMap::new(),
                    });
                    self.name_index.insert(name.clone(), i);
                    i
                }
            };

            let exists = self.names[ni].pid_index.contains_key(&pid);
            if !exists {
                let id = self.next_id;
                self.next_id += 1;
                let ng = &mut self.names[ni];
                let i = ng.pids.len();
                ng.pids.push(PidGroup {
                    pid,
                    id,
                    dlls: VecDeque::new(),
                });
                ng.pid_index.insert(pid, i);
            }

            let is_system = is_system_dll(&ev.image_name);
            let ng = &mut self.names[ni];
            let pi = *ng.pid_index.get(&pid).unwrap();
            let gid = ng.pids[pi].id;
            ng.pids[pi].dlls.push_back(StoredDll { ev, is_system });

            self.order.push_back((name, pid, gid));
            added += 1;
        }

        if added == 0 {
            return;
        }

        self.trim_overflow();
        self.rebuild_flat();

        if self.stick_to_bottom {
            self.scroll_handle.scroll_to_bottom();
        }

        cx.notify();
    }

    fn trim_overflow(&mut self) {
        while self.order.len() > MAX_EVENTS {
            let Some((name, pid, gid)) = self.order.pop_front() else {
                break;
            };
            let Some(&ni) = self.name_index.get(&name) else {
                continue;
            };

            let mut removed_pid: Option<u64> = None;
            let mut removed_name: Option<u64> = None;

            {
                let ng = &mut self.names[ni];
                let Some(&pi) = ng.pid_index.get(&pid) else {
                    continue;
                };
                if ng.pids[pi].id != gid {
                    continue;
                }

                ng.pids[pi].dlls.pop_front();
                if ng.pids[pi].dlls.is_empty() {
                    ng.pids.remove(pi);
                    ng.pid_index.clear();
                    for (i, p) in ng.pids.iter().enumerate() {
                        ng.pid_index.insert(p.pid, i);
                    }
                    removed_pid = Some(gid);
                }
                if ng.pids.is_empty() {
                    removed_name = Some(ng.id);
                }
            }

            if let Some(g) = removed_pid {
                self.expanded_pids.remove(&g);
                self.auto_expanded_pids.remove(&g);
            }
            if let Some(id) = removed_name {
                self.names.remove(ni);
                self.name_index.remove(&name);
                self.name_index.clear();
                for (i, n) in self.names.iter().enumerate() {
                    self.name_index.insert(n.name.clone(), i);
                }
                self.expanded_names.remove(&id);
                self.auto_expanded_names.remove(&id);
            }
        }
    }

    fn rebuild_flat(&mut self) {
        let show_system = self.show_system;
        let mut rows = Vec::new();

        for ng in &self.names {
            let name_expanded = self.expanded_names.contains(&ng.id);
            let total_dlls: usize = ng
                .pids
                .iter()
                .map(|pg| {
                    if show_system {
                        pg.dlls.len()
                    } else {
                        pg.dlls.iter().filter(|d| !d.is_system).count()
                    }
                })
                .sum();

            rows.push(RenderRow::Name {
                name_id: ng.id,
                name: ng.name.clone(),
                pid_count: ng.pids.len(),
                total_dlls,
                expanded: name_expanded,
            });

            if !name_expanded {
                continue;
            }

            for pg in &ng.pids {
                let pid_expanded = self.expanded_pids.contains(&pg.id);
                let (count, last_ts) = if show_system {
                    (pg.dlls.len(), pg.dlls.back().map(|d| d.ev.ts).unwrap_or(0))
                } else {
                    (
                        pg.dlls.iter().filter(|d| !d.is_system).count(),
                        pg.dlls
                            .iter()
                            .rev()
                            .find(|d| !d.is_system)
                            .map(|d| d.ev.ts)
                            .unwrap_or(0),
                    )
                };

                rows.push(RenderRow::Pid {
                    group_id: pg.id,
                    pid: pg.pid,
                    name: ng.name.clone(),
                    count,
                    expanded: pid_expanded,
                    last_ts,
                });

                if pid_expanded {
                    for d in &pg.dlls {
                        if !show_system && d.is_system {
                            continue;
                        }
                        rows.push(RenderRow::Dll {
                            image_name: d.ev.image_name.clone(),
                            ts: d.ev.ts,
                            pid: pg.pid,
                            process_name: ng.name.clone(),
                        });
                    }
                }
            }
        }

        self.flat = Rc::new(rows);
    }

    fn toggle_name(&mut self, name_id: u64, cx: &mut Context<Self>) {
        self.auto_expanded_names.remove(&name_id);
        if !self.expanded_names.remove(&name_id) {
            self.expanded_names.insert(name_id);
        }
        self.stick_to_bottom = false;
        self.persist_config();
        self.rebuild_flat();
        cx.notify();
    }

    fn toggle_pid(&mut self, group_id: u64, cx: &mut Context<Self>) {
        self.auto_expanded_pids.remove(&group_id);
        if !self.expanded_pids.remove(&group_id) {
            self.expanded_pids.insert(group_id);
        }
        self.stick_to_bottom = false;
        self.persist_config();
        self.rebuild_flat();
        cx.notify();
    }

    fn batch_expand_active(&self) -> bool {
        !self.auto_expanded_names.is_empty() || !self.auto_expanded_pids.is_empty()
    }

    fn toggle_all_expansion(&mut self, cx: &mut Context<Self>) {
        if self.names.is_empty() {
            return;
        }

        self.stick_to_bottom = false;
        self.persist_config();

        if self.batch_expand_active() {
            let auto_names: Vec<u64> = self.auto_expanded_names.drain().collect();
            let auto_pids: Vec<u64> = self.auto_expanded_pids.drain().collect();
            for id in auto_names {
                self.expanded_names.remove(&id);
            }
            for id in auto_pids {
                self.expanded_pids.remove(&id);
            }
        } else {
            let mut new_names = Vec::new();
            let mut new_pids = Vec::new();
            for ng in &self.names {
                if !self.expanded_names.contains(&ng.id) {
                    new_names.push(ng.id);
                }
                for pg in &ng.pids {
                    if !self.expanded_pids.contains(&pg.id) {
                        new_pids.push(pg.id);
                    }
                }
            }
            for id in new_names {
                self.expanded_names.insert(id);
                self.auto_expanded_names.insert(id);
            }
            for id in new_pids {
                self.expanded_pids.insert(id);
                self.auto_expanded_pids.insert(id);
            }
        }

        self.rebuild_flat();
        cx.notify();
    }

    fn toggle_show_system(&mut self, cx: &mut Context<Self>) {
        self.show_system = !self.show_system;
        self.persist_config();
        self.rebuild_flat();
        if self.stick_to_bottom {
            self.scroll_handle.scroll_to_bottom();
        }
        cx.notify();
    }

    fn toggle_stick_to_bottom(&mut self, cx: &mut Context<Self>) {
        self.stick_to_bottom = !self.stick_to_bottom;
        self.persist_config();
        if self.stick_to_bottom {
            self.scroll_handle.scroll_to_bottom();
        }
        cx.notify();
    }

    fn stop_monitor(&mut self, cx: &mut Context<Self>) {
        self.is_running = false;
        if let Some(s) = self.session.take() {
            s.stop();
        }
        self.persist_config();
        cx.notify();
    }

    fn toggle_monitor(&mut self, cx: &mut Context<Self>) {
        if self.is_running {
            self.stop_monitor(cx);
        } else {
            self.start_monitor(cx);
            self.persist_config();
        }
    }

    fn clear_events(&mut self, cx: &mut Context<Self>) {
        self.reset_state();
        self.error_msg = None;
        cx.notify();
    }

    fn open_menu(&mut self, x: f32, y: f32, target: MenuTarget, cx: &mut Context<Self>) {
        self.menu = Some(MenuState { x, y, target });
        cx.notify();
    }

    fn close_menu(&mut self, cx: &mut Context<Self>) {
        self.menu = None;
        cx.notify();
    }

    fn handle_menu_action(
        &mut self,
        action: MenuAction,
        target: MenuTarget,
        cx: &mut Context<Self>,
    ) {
        self.menu = None;
        match action {
            MenuAction::Detail => {
                self.detail = Some(build_detail(&target));
            }
            MenuAction::Reveal => {
                if let MenuTarget::Dll { image_name, .. } = &target {
                    reveal_in_explorer(image_name);
                }
            }
        }
        cx.notify();
    }

    fn stats(&self) -> (usize, usize, usize) {
        let show_system = self.show_system;
        let names = self.names.len();
        let pids: usize = self.names.iter().map(|n| n.pids.len()).sum();
        let dlls: usize = self
            .names
            .iter()
            .flat_map(|n| n.pids.iter())
            .map(|pg| {
                if show_system {
                    pg.dlls.len()
                } else {
                    pg.dlls.iter().filter(|d| !d.is_system).count()
                }
            })
            .sum();
        (names, pids, dlls)
    }
}

// ==================== 渲染 ====================

fn menu_entry(
    label: &'static str,
    entity: Entity<EtwViewer>,
    action: MenuAction,
    target: MenuTarget,
) -> AnyElement {
    div()
        .id(label)
        .h(px(30.))
        .px(px(12.))
        .flex()
        .items_center()
        .text_sm()
        .cursor_pointer()
        .hover(|s| s.bg(rgb(0x3a3a3d)))
        .on_mouse_down(MouseButton::Left, move |_, _, cx| {
            let target = target.clone();
            let _ = entity.update(cx, |this, cx| {
                this.handle_menu_action(action, target, cx);
            });
        })
        .child(label)
        .into_any_element()
}

impl Render for EtwViewer {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let entity = cx.entity();

        let (name_count, pid_count, dll_count) = self.stats();

        let has_items = !self.names.is_empty();
        let expand_enabled = has_items;
        let batch_active = self.batch_expand_active();
        let expand_bg = if !expand_enabled {
            rgb(0x2a2a2c)
        } else if batch_active {
            rgb(0x007AFF)
        } else {
            rgb(0x333333)
        };
        let expand_fg = if !expand_enabled {
            rgb(0x666666)
        } else {
            rgb(0xffffff)
        };

        let stick_bg = if self.stick_to_bottom {
            rgb(0x007AFF)
        } else {
            rgb(0x333333)
        };
        let sys_bg = if self.show_system {
            rgb(0x007AFF)
        } else {
            rgb(0x333333)
        };

        let monitor_label = if self.is_running {
            "停止监控"
        } else {
            "开始监控"
        };
        let monitor_bg = if self.is_running {
            rgb(0x8a2020)
        } else {
            rgb(0x007AFF)
        };
        let monitor_hover = if self.is_running {
            rgb(0xa02828)
        } else {
            rgb(0x2b8fff)
        };

        let vw = f32::from(window.viewport_size().width);
        let vh = f32::from(window.viewport_size().height);

        div()
            .relative()
            .flex()
            .flex_col()
            .size_full()
            .bg(rgb(0x1e1e1e))
            .text_color(rgb(0xffffff))
            .child(
                div()
                    .flex()
                    .items_center()
                    .h(px(48.))
                    .px(px(16.))
                    .gap(px(8.))
                    .border_b_1()
                    .border_color(rgb(0x333333))
                    .child(
                        div()
                            .text_sm()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("ETW 镜像加载监控"),
                    )
                    .child(div().flex_1())
                    .child(div().text_xs().text_color(rgb(0x888888)).child(format!(
                        "{} 程序 · {} 进程 · {} 模块",
                        name_count, pid_count, dll_count
                    )))
                    .child(div().w(px(8.)))
                    .child(
                        div()
                            .id("toggle-expand")
                            .h(px(28.))
                            .px(px(12.))
                            .rounded(px(6.))
                            .bg(expand_bg)
                            .text_color(expand_fg)
                            .text_sm()
                            .flex()
                            .items_center()
                            .when(expand_enabled, |d| d.cursor_pointer())
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, _, _, cx| {
                                    this.toggle_all_expansion(cx);
                                }),
                            )
                            .child("展开"),
                    )
                    .child(
                        div()
                            .id("toggle-auto-scroll")
                            .h(px(28.))
                            .px(px(12.))
                            .rounded(px(6.))
                            .bg(stick_bg)
                            .text_sm()
                            .flex()
                            .items_center()
                            .cursor_pointer()
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, _, _, cx| {
                                    this.toggle_stick_to_bottom(cx);
                                }),
                            )
                            .child("自动滚动"),
                    )
                    .child(
                        div()
                            .id("toggle-show-system")
                            .h(px(28.))
                            .px(px(12.))
                            .rounded(px(6.))
                            .bg(sys_bg)
                            .text_sm()
                            .flex()
                            .items_center()
                            .cursor_pointer()
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, _, _, cx| {
                                    this.toggle_show_system(cx);
                                }),
                            )
                            .child("显示系统DLL"),
                    )
                    .child(
                        div()
                            .id("toggle-monitor")
                            .h(px(28.))
                            .px(px(12.))
                            .rounded(px(6.))
                            .bg(monitor_bg)
                            .text_sm()
                            .flex()
                            .items_center()
                            .cursor_pointer()
                            .hover(move |s| s.bg(monitor_hover))
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, _, _, cx| {
                                    this.toggle_monitor(cx);
                                }),
                            )
                            .child(monitor_label),
                    )
                    .child(
                        div()
                            .id("clear-btn")
                            .h(px(28.))
                            .px(px(12.))
                            .rounded(px(6.))
                            .bg(rgb(0x333333))
                            .text_sm()
                            .flex()
                            .items_center()
                            .cursor_pointer()
                            .hover(|s| s.bg(rgb(0x444444)))
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, _, _, cx| {
                                    this.clear_events(cx);
                                }),
                            )
                            .child("清空"),
                    ),
            )
            .when_some(self.error_msg.clone(), |d, err| {
                d.child(
                    div()
                        .w_full()
                        .bg(rgb(0x4a1010))
                        .border_b_1()
                        .border_color(rgb(0x8a2020))
                        .px(px(16.))
                        .py(px(8.))
                        .text_sm()
                        .text_color(rgb(0xff6666))
                        .child(err),
                )
            })
            .child(
                div()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .child({
                        let flat = self.flat.clone();
                        let list_entity = entity.clone();
                        uniform_list("etw-list", flat.len(), move |range, _window, _cx| {
                            range
                                .clone()
                                .map(|i| build_row(&flat[i], i, list_entity.clone()))
                                .collect::<Vec<_>>()
                        })
                        .track_scroll(&self.scroll_handle)
                        .size_full()
                    })
                    .child(
                        div()
                            .absolute()
                            .top(px(0.))
                            .bottom(px(0.))
                            .right(px(2.))
                            .w(px(6.))
                            .child(Scrollbar::new(&self.scroll_handle)),
                    ),
            )
            .when_some(self.menu.clone(), |root, menu| {
                let is_dll = matches!(menu.target, MenuTarget::Dll { .. });
                let mx = menu.x.min((vw - 210.0).max(0.0)).max(0.0);
                let my = menu.y.min((vh - 90.0).max(0.0)).max(0.0);
                let close_entity = entity.clone();
                let menu_entity = entity.clone();
                let target = menu.target.clone();
                root.child(
                    div()
                        .absolute()
                        .top(px(0.))
                        .left(px(0.))
                        .right(px(0.))
                        .bottom(px(0.))
                        .on_mouse_down(MouseButton::Left, move |_, _, cx| {
                            let _ = close_entity.update(cx, |this, cx| this.close_menu(cx));
                        }),
                )
                .child(
                    div()
                        .absolute()
                        .left(px(mx))
                        .top(px(my))
                        .w(px(200.))
                        .bg(rgb(0x2d2d30))
                        .border_1()
                        .border_color(rgb(0x454545))
                        .rounded(px(6.))
                        .flex()
                        .flex_col()
                        .py(px(4.))
                        .child(menu_entry(
                            "查看详细信息",
                            menu_entity.clone(),
                            MenuAction::Detail,
                            target.clone(),
                        ))
                        .when(is_dll, |d| {
                            d.child(menu_entry(
                                "在资源管理器打开",
                                menu_entity.clone(),
                                MenuAction::Reveal,
                                target.clone(),
                            ))
                        }),
                )
            })
            .when_some(self.detail.clone(), |root, detail| {
                let close_entity = entity.clone();
                let button_entity = entity.clone();
                root.child(
                    div()
                        .absolute()
                        .top(px(0.))
                        .left(px(0.))
                        .right(px(0.))
                        .bottom(px(0.))
                        .bg(rgba(0x000000cc))
                        .flex()
                        .items_center()
                        .justify_center()
                        .on_mouse_down(MouseButton::Left, move |_, _, cx| {
                            let _ = close_entity.update(cx, |this, cx| {
                                this.detail = None;
                                cx.notify();
                            });
                        })
                        .child(
                            div()
                                .w(px(480.))
                                .bg(rgb(0x252526))
                                .border_1()
                                .border_color(rgb(0x454545))
                                .rounded(px(8.))
                                .p(px(16.))
                                .flex()
                                .flex_col()
                                .gap(px(10.))
                                .on_mouse_down(MouseButton::Left, |_, _, _| {})
                                .child(
                                    div()
                                        .text_sm()
                                        .font_weight(FontWeight::SEMIBOLD)
                                        .child(detail.title.clone()),
                                )
                                .child(div().flex().flex_col().gap(px(6.)).children(
                                    detail.lines.iter().map(|(k, v)| {
                                        div()
                                            .flex()
                                            .items_start()
                                            .gap(px(8.))
                                            .child(
                                                div()
                                                    .w(px(80.))
                                                    .flex_shrink_0()
                                                    .text_xs()
                                                    .text_color(rgb(0x999999))
                                                    .child(k.clone()),
                                            )
                                            .child(
                                                div()
                                                    .flex_1()
                                                    .min_w_0()
                                                    .text_xs()
                                                    .font_family("SF Mono")
                                                    .text_color(rgb(0xD4D4D4))
                                                    .child(v.clone()),
                                            )
                                    }),
                                ))
                                .child(
                                    div().flex().justify_end().child(
                                        div()
                                            .id("detail-close")
                                            .h(px(28.))
                                            .px(px(14.))
                                            .rounded(px(6.))
                                            .bg(rgb(0x007AFF))
                                            .text_sm()
                                            .flex()
                                            .items_center()
                                            .cursor_pointer()
                                            .hover(|s| s.bg(rgb(0x2b8fff)))
                                            .on_mouse_down(MouseButton::Left, move |_, _, cx| {
                                                let _ = button_entity.update(cx, |this, cx| {
                                                    this.detail = None;
                                                    cx.notify();
                                                });
                                            })
                                            .child("关闭"),
                                    ),
                                ),
                        ),
                )
            })
    }
}

fn build_row(row: &RenderRow, index: usize, entity: Entity<EtwViewer>) -> AnyElement {
    match row {
        RenderRow::Name {
            name_id,
            name,
            pid_count,
            total_dlls,
            expanded,
        } => {
            let name_id = *name_id;
            let arrow = if *expanded { "▾" } else { "▸" };
            let count_text = format!("{} 进程 · {} 模块", pid_count, total_dlls);

            let left_entity = entity.clone();
            let right_entity = entity.clone();
            let right_target = MenuTarget::Name {
                name: name.clone(),
                pid_count: *pid_count,
                total_dlls: *total_dlls,
            };

            div()
                .id(("name-row", index as u64))
                .flex()
                .items_center()
                .h(px(30.))
                .px(px(16.))
                .gap(px(8.))
                .bg(rgb(0x252526))
                .border_b_1()
                .border_color(rgb(0x2a2a2a))
                .cursor_pointer()
                .hover(|s| s.bg(rgb(0x2e2e30)))
                .on_mouse_down(MouseButton::Left, move |_, _, cx| {
                    let _ = left_entity.update(cx, |this, cx| this.toggle_name(name_id, cx));
                })
                .on_mouse_down(MouseButton::Right, move |ev, _, cx| {
                    let x = f32::from(ev.position.x);
                    let y = f32::from(ev.position.y);
                    let target = right_target.clone();
                    let _ = right_entity.update(cx, |this, cx| {
                        this.open_menu(x, y, target, cx);
                    });
                })
                .child(
                    div()
                        .w(px(16.))
                        .text_xs()
                        .text_color(rgb(0x888888))
                        .child(arrow),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_sm()
                        .font_weight(FontWeight::SEMIBOLD)
                        .whitespace_nowrap()
                        .text_ellipsis()
                        .overflow_hidden()
                        .child(name.clone()),
                )
                .child(div().text_xs().text_color(rgb(0x888888)).child(count_text))
                .into_any_element()
        }
        RenderRow::Pid {
            group_id,
            pid,
            name,
            count,
            expanded,
            last_ts,
        } => {
            let group_id = *group_id;
            let arrow = if *expanded { "▾" } else { "▸" };
            let time_text = if *last_ts == 0 {
                String::new()
            } else {
                fmt_time(*last_ts)
            };

            let left_entity = entity.clone();
            let right_entity = entity.clone();
            let right_target = MenuTarget::Pid {
                pid: *pid,
                name: name.clone(),
                count: *count,
                last_ts: *last_ts,
            };

            div()
                .id(("pid-row", index as u64))
                .flex()
                .items_center()
                .h(px(26.))
                .pl(px(40.))
                .pr(px(16.))
                .gap(px(10.))
                .bg(rgb(0x222224))
                .border_b_1()
                .border_color(rgb(0x2a2a2a))
                .cursor_pointer()
                .hover(|s| s.bg(rgb(0x2a2a2c)))
                .on_mouse_down(MouseButton::Left, move |_, _, cx| {
                    let _ = left_entity.update(cx, |this, cx| this.toggle_pid(group_id, cx));
                })
                .on_mouse_down(MouseButton::Right, move |ev, _, cx| {
                    let x = f32::from(ev.position.x);
                    let y = f32::from(ev.position.y);
                    let target = right_target.clone();
                    let _ = right_entity.update(cx, |this, cx| {
                        this.open_menu(x, y, target, cx);
                    });
                })
                .child(
                    div()
                        .w(px(16.))
                        .text_xs()
                        .text_color(rgb(0x777777))
                        .child(arrow),
                )
                .child(
                    div()
                        .w(px(120.))
                        .text_xs()
                        .font_family("SF Mono")
                        .text_color(rgb(0x007AFF))
                        .child(format!("pid {}", pid)),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(if *count == 0 {
                            rgb(0x666666)
                        } else {
                            rgb(0x888888)
                        })
                        .child(format!("{} 个模块", count)),
                )
                .child(div().flex_1())
                .child(
                    div()
                        .text_xs()
                        .font_family("SF Mono")
                        .text_color(rgb(0x777777))
                        .child(time_text),
                )
                .into_any_element()
        }
        RenderRow::Dll {
            image_name,
            ts,
            pid,
            process_name,
        } => {
            let right_entity = entity.clone();
            let right_target = MenuTarget::Dll {
                image_name: image_name.clone(),
                ts: *ts,
                pid: *pid,
                process_name: process_name.clone(),
            };

            div()
                .id(("dll-row", index as u64))
                .flex()
                .items_center()
                .h(px(26.))
                .pl(px(72.))
                .pr(px(16.))
                .gap(px(12.))
                .border_b_1()
                .border_color(rgb(0x2a2a2a))
                .hover(|s| s.bg(rgb(0x2c2c2c)))
                .on_mouse_down(MouseButton::Right, move |ev, _, cx| {
                    let x = f32::from(ev.position.x);
                    let y = f32::from(ev.position.y);
                    let target = right_target.clone();
                    let _ = right_entity.update(cx, |this, cx| {
                        this.open_menu(x, y, target, cx);
                    });
                })
                .child(
                    div()
                        .w(px(70.))
                        .text_xs()
                        .font_family("SF Mono")
                        .text_color(rgb(0x888888))
                        .child(fmt_time(*ts)),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_xs()
                        .font_family("SF Mono")
                        .text_color(rgb(0xD4D4D4))
                        .whitespace_nowrap()
                        .text_ellipsis()
                        .overflow_hidden()
                        .child(image_name.clone()),
                )
                .into_any_element()
        }
    }
}

// ==================== 入口 ====================

fn main() {
    #[cfg(windows)]
    ensure_admin();

    gpui_kit::application()
        .with_assets(gpui_kit::assets::Assets)
        .run(move |cx| {
            gpui_kit::init(cx);

            let theme = Theme::global_mut(cx);
            theme.mode = ThemeMode::Dark;
            theme.font_family = "SF Pro Display".into();
            theme.mono_font_family = "SF Mono".into();

            let bounds = Bounds::centered(None, size(px(1000.), px(600.)), cx);
            gpui_kit::open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    titlebar: Some(TitlebarOptions {
                        title: Some("ETW 镜像加载监控".into()),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                cx,
                |_window, cx| cx.new(|cx| EtwViewer::new(cx)),
            )
            .expect("Failed to open window");
        });
}

/// 检测并申请管理员权限。如果不是管理员，会拉起 UAC 并结束当前进程。
#[cfg(windows)]
fn ensure_admin() {
    use std::os::windows::ffi::OsStrExt;
    use windows::Win32::UI::Shell::{IsUserAnAdmin, ShellExecuteW};
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    use windows::core::{PCWSTR, w};

    unsafe {
        if !IsUserAnAdmin().as_bool() {
            let exe = std::env::current_exe().expect("无法获取当前程序路径");
            let exe_wide: Vec<u16> = exe
                .as_os_str()
                .encode_wide()
                .chain(std::iter::once(0))
                .collect();

            let _ = ShellExecuteW(
                None,
                w!("runas"),
                PCWSTR(exe_wide.as_ptr()),
                None,
                None,
                SW_SHOWNORMAL,
            );

            std::process::exit(0);
        }
    }
}
