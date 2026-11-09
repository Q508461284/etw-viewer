#[cfg(not(windows))]
compile_error!(
    "etw-monitor 仅支持 Windows 目标平台（依赖 ETW 与 Win32 API）。\n\
     支持的 target: x86_64-pc-windows-msvc / aarch64-pc-windows-msvc / i686-pc-windows-msvc\n\
     Windows 上直接构建: cargo build --release\n\
     交叉编译: cargo build --release --target x86_64-pc-windows-msvc"
);

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use tokio::sync::mpsc;

use windows::Win32::Foundation::CloseHandle;
use windows::Win32::Storage::FileSystem::{GetLogicalDriveStringsW, QueryDosDeviceW};
use windows::Win32::System::Threading::{
    OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};
use windows::core::{PCWSTR, PWSTR};
use windows_erg::etw::{DecodedEvent, EventTrace, SystemProvider};

// ==================== 公共数据模型 ====================

#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct DllEvent {
    pub pid: u32,
    pub process_name: String,
    pub image_name: String,
    pub ts: u64,
}

#[derive(Debug)]
pub enum MonitorEvent {
    Event(DllEvent),
    Error(EtwError),
}

#[derive(Debug)]
pub enum EtwError {
    StartFailed(String),
    ReadFailed(String),
}

impl std::fmt::Display for EtwError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EtwError::StartFailed(s) => write!(f, "ETW 启动失败: {}", s),
            EtwError::ReadFailed(s) => write!(f, "读取事件失败: {}", s),
        }
    }
}

impl std::error::Error for EtwError {}

// ==================== 工具函数（私有） ====================

fn get_process_name(pid: u32) -> Option<String> {
    if pid == 0 {
        return Some("System Idle Process".to_string());
    }
    if pid == 4 {
        return Some("System".to_string());
    }

    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;

        let mut buf = [0u16; 1024];
        let mut size = buf.len() as u32;
        let res = QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_WIN32,
            PWSTR(buf.as_mut_ptr()),
            &mut size,
        );
        let _ = CloseHandle(handle);

        if res.is_ok() {
            let full_path = String::from_utf16_lossy(&buf[..size as usize]);
            Some(
                full_path
                    .rsplit('\\')
                    .next()
                    .unwrap_or(&full_path)
                    .to_string(),
            )
        } else {
            None
        }
    }
}

fn build_drive_map() -> HashMap<String, String> {
    let mut map = HashMap::new();
    unsafe {
        let mut buf = [0u16; 512];
        let len = GetLogicalDriveStringsW(Some(&mut buf));
        if len > 0 {
            let mut start = 0;
            for i in 0..len as usize {
                if buf[i] == 0 {
                    if i > start {
                        let drive = String::from_utf16_lossy(&buf[start..i]);
                        let drive_stripped = drive.trim_end_matches('\\').to_string();

                        let mut dev_buf = [0u16; 512];
                        let drive_utf16: Vec<u16> = drive_stripped
                            .encode_utf16()
                            .chain(std::iter::once(0))
                            .collect();

                        let dev_len =
                            QueryDosDeviceW(PCWSTR(drive_utf16.as_ptr()), Some(&mut dev_buf));
                        if dev_len > 0 {
                            let dev_raw = String::from_utf16_lossy(&dev_buf[..dev_len as usize]);
                            let dev = dev_raw.trim_end_matches('\0').to_string();
                            map.insert(dev, drive_stripped);
                        }
                    }
                    start = i + 1;
                }
            }
        }
    }
    map
}

fn nt_to_dos_path(nt_path: &str, drive_map: &HashMap<String, String>) -> String {
    for (nt_prefix, drive_letter) in drive_map {
        if nt_path.starts_with(nt_prefix) {
            return format!("{}{}", drive_letter, &nt_path[nt_prefix.len()..]);
        }
    }
    if nt_path.starts_with("\\Device\\HarddiskVolume") {
        if let Some(idx) = nt_path[22..].find('\\') {
            return format!("C:{}", &nt_path[22 + idx..]);
        }
    }
    nt_path.to_string()
}

// ==================== 会话句柄 ====================

pub struct EtwSession {
    stop_flag: Arc<AtomicBool>,
}

impl EtwSession {
    pub fn start() -> Result<(Self, EventStream), EtwError> {
        let (tx, rx) = mpsc::unbounded_channel();
        let drive_map = Arc::new(build_drive_map());
        let proc_cache = Arc::new(Mutex::new(HashMap::new()));
        let stop_flag = Arc::new(AtomicBool::new(false));
        let stop_flag_clone = stop_flag.clone();

        std::thread::Builder::new()
            .name("etw-monitor".into())
            .spawn(move || {
                let trace_result = EventTrace::builder("DllLoadViewerSession")
                    .system_provider(SystemProvider::ImageLoad)
                    .start();

                let mut trace = match trace_result {
                    Ok(t) => t,
                    Err(e) => {
                        let msg = format!("{:?}", e);
                        let _ = tx.send(MonitorEvent::Error(EtwError::StartFailed(msg)));
                        return;
                    }
                };

                let mut events = Vec::with_capacity(64);
                'outer: loop {
                    if stop_flag_clone.load(Ordering::Relaxed) {
                        break;
                    }
                    if let Err(e) = trace.next_batch(&mut events) {
                        let msg = format!("{:?}", e);
                        let _ = tx.send(MonitorEvent::Error(EtwError::ReadFailed(msg)));
                        break;
                    }

                    for event in &events {
                        let DecodedEvent::ImageLoad(decoded) = event.decode() else {
                            continue;
                        };

                        let pid = decoded.process_id.as_u32();
                        let image_name = nt_to_dos_path(&decoded.file_name, &drive_map);

                        if pid == 0 || image_name.is_empty() {
                            continue;
                        }

                        let pname = {
                            let mut cache = proc_cache.lock().unwrap();
                            cache
                                .entry(pid)
                                .or_insert_with(|| get_process_name(pid))
                                .clone()
                        };

                        let Some(pname) = pname else {
                            continue;
                        };

                        let ts = SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .map(|d| d.as_millis() as u64)
                            .unwrap_or(0);

                        let msg = MonitorEvent::Event(DllEvent {
                            pid,
                            process_name: pname,
                            image_name,
                            ts,
                        });

                        if tx.send(msg).is_err() {
                            break 'outer;
                        }
                    }
                }
            })
            .expect("failed to spawn etw-monitor thread");

        Ok((EtwSession { stop_flag }, EventStream { rx }))
    }

    pub fn stop(&self) {
        self.stop_flag.store(true, Ordering::Relaxed);
    }
}

impl Drop for EtwSession {
    fn drop(&mut self) {
        self.stop();
    }
}

// ==================== 事件流 ====================

pub struct EventStream {
    rx: mpsc::UnboundedReceiver<MonitorEvent>,
}

impl EventStream {
    pub async fn next(&mut self) -> Option<MonitorEvent> {
        self.rx.recv().await
    }
}
