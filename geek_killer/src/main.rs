#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use eframe::egui;
use rust_core_lib::{device, meta::STAR_TAP_BRAND, security, ui};
use std::collections::HashMap;
use std::sync::{mpsc, Arc, RwLock};
use std::time::{Duration, Instant};
use sysinfo::{Disks, Networks, ProcessRefreshKind, System};

use windows_sys::Win32::Devices::DeviceAndDriverInstallation::{
    CM_Get_Parent, CM_Request_Device_EjectW, CR_SUCCESS, DIGCF_DEVICEINTERFACE, DIGCF_PRESENT,
    SP_DEVICE_INTERFACE_DATA, SP_DEVICE_INTERFACE_DETAIL_DATA_W, SP_DEVINFO_DATA,
    SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInterfaces, SetupDiGetClassDevsW,
    SetupDiGetDeviceInterfaceDetailW,
};
use windows_sys::Win32::System::Ioctl::{
    IOCTL_STORAGE_GET_DEVICE_NUMBER, STORAGE_DEVICE_NUMBER,
};
use windows_sys::Win32::UI::Shell::SHChangeNotify;

const GUID_DEVINTERFACE_DISK: windows_sys::core::GUID = windows_sys::core::GUID {
    data1: 0x53f56307,
    data2: 0xb6bf,
    data3: 0x11d0,
    data4: [0x94, 0xf2, 0x00, 0xa0, 0xc9, 0x1e, 0xfb, 0x8b],
};

// ═══════════════════════════════════════════════════════════════
//  核心数据结构与状态定义
// ═══════════════════════════════════════════════════════════════

#[derive(Clone, Debug, PartialEq)]
struct Occupant {
    pid: u32,
    name: String,
    desc: String,
}

#[derive(Clone, Debug, PartialEq)]
enum UsbState {
    Idle,
    Scanning(String), // 正在扫描的盘符
    Occupied { drive: String, list: Vec<Occupant> },
    Ejecting(String), // 正在弹出的盘符
    Done(String),     // 成功/失败消息
}

enum UsbMsg {
    State(UsbState),
}

enum UsbCmd {
    Scan(String),                    // 扫描占用并弹出
    ForceEject(String, Vec<u32>),    // 强制弹出
    FsutilDismount(String),          // 极客命令：fsutil
    KillOne(u32, String),            // 终止单个
}

#[derive(Clone, Debug)]
struct ProcessInfo {
    chinese_name: String,
    category: String,
}

impl ProcessInfo {
    fn new(name: &str, cat: &str) -> Self {
        Self {
            chinese_name: name.to_string(),
            category: cat.to_string(),
        }
    }
}

#[derive(Clone, Debug)]
struct ProcessGroup {
    name: String,
    friendly_name: String,
    category: String,
    total_memory: u64,
    total_cpu: f32,
    pids: Vec<u32>,
    is_system: bool,
    is_not_responding: bool,
}

#[derive(Clone, Debug, Default)]
struct DiskData {
    mount_point: String,
    name: String,
    available_space: u64,
    total_space: u64,
    is_removable: bool,
}

/// 共享给 UI 的数据快照（解决 UI 卡顿的核心）
#[derive(Clone, Default)]
struct AppSnapshot {
    high_resource: Vec<ProcessGroup>,
    other_groups: Vec<ProcessGroup>,
    system_groups: Vec<ProcessGroup>,

    global_cpu: f32,
    used_memory: u64,
    total_memory: u64,

    network_in: u64,
    network_out: u64,

    disks: Vec<DiskData>,

    is_resource_tight: bool,
}

// ═══════════════════════════════════════════════════════════════
//  Win32 API 封装 (FileDescription & RestartManager)
// ═══════════════════════════════════════════════════════════════

#[link(name = "version")]
extern "system" {
    fn GetFileVersionInfoSizeW(lptstrfilename: *const u16, lpdwhandle: *mut u32) -> u32;
    fn GetFileVersionInfoW(
        lptstrfilename: *const u16,
        dwhandle: u32,
        dwlen: u32,
        lpdata: *mut std::ffi::c_void,
    ) -> i32;
    fn VerQueryValueW(
        pblock: *const std::ffi::c_void,
        lpsubblock: *const u16,
        lplpbuffer: *mut *mut std::ffi::c_void,
        puptrlen: *mut u32,
    ) -> i32;
}

fn get_exe_file_description(exe_path: &std::path::Path) -> Option<String> {
    use std::os::windows::ffi::OsStrExt;
    let path_wide: Vec<u16> = exe_path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();

    unsafe {
        let mut _handle = 0;
        let size = GetFileVersionInfoSizeW(path_wide.as_ptr(), &mut _handle);
        if size == 0 {
            return None;
        }

        let mut buffer = vec![0u8; size as usize];
        if GetFileVersionInfoW(path_wide.as_ptr(), 0, size, buffer.as_mut_ptr() as _) == 0 {
            return None;
        }

        let mut lang_ptr = std::ptr::null_mut();
        let mut lang_len = 0;
        let var_info_path: Vec<u16> = "\\VarFileInfo\\Translation\0".encode_utf16().collect();

        let mut description = None;

        if VerQueryValueW(
            buffer.as_ptr() as _,
            var_info_path.as_ptr(),
            &mut lang_ptr,
            &mut lang_len,
        ) != 0
            && lang_len >= 4
        {
            let langs = std::slice::from_raw_parts(lang_ptr as *const u16, (lang_len / 2) as usize);
            for i in (0..langs.len()).step_by(2) {
                let lang_id = langs[i];
                let charset_id = langs[i + 1];
                let sub_block = format!(
                    "\\StringFileInfo\\{:04x}{:04x}\\FileDescription",
                    lang_id, charset_id
                );
                if let Some(desc) = query_string_value(&buffer, &sub_block) {
                    description = Some(desc);
                    break;
                }
            }
        }

        if description.is_none() {
            let fallbacks = [
                "\\StringFileInfo\\080404b0\\FileDescription",
                "\\StringFileInfo\\040904b0\\FileDescription",
                "\\StringFileInfo\\000004b0\\FileDescription",
            ];
            for fb in fallbacks {
                if let Some(desc) = query_string_value(&buffer, fb) {
                    description = Some(desc);
                    break;
                }
            }
        }
        description
    }
}

fn query_string_value(buffer: &[u8], sub_block: &str) -> Option<String> {
    unsafe {
        let sub_block_wide: Vec<u16> = sub_block.encode_utf16().chain(std::iter::once(0)).collect();
        let mut value_ptr = std::ptr::null_mut();
        let mut value_len = 0;

        if VerQueryValueW(
            buffer.as_ptr() as _,
            sub_block_wide.as_ptr(),
            &mut value_ptr,
            &mut value_len,
        ) != 0
            && value_len > 0
        {
            let slice = std::slice::from_raw_parts(value_ptr as *const u16, value_len as usize);
            let s = String::from_utf16_lossy(slice)
                .trim_matches(char::from(0))
                .to_string();
            if !s.is_empty() {
                return Some(s);
            }
        }
    }
    None
}

/// Restart Manager 模块 - 解决 U 盘占用检测的关键
mod rm {
    use super::Occupant;
    use windows_sys::Win32::Foundation::ERROR_MORE_DATA;
    use windows_sys::Win32::Storage::FileSystem::GetVolumeNameForVolumeMountPointW;
    use windows_sys::Win32::System::RestartManager::*;

    fn w(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }
    fn from_wide(buf: &[u16]) -> String {
        let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        String::from_utf16_lossy(&buf[..end])
    }

    fn volume_guid_root(drive_letter: &str) -> Option<String> {
        let letter = drive_letter.trim_end_matches(':').to_uppercase();
        let mount = format!("{}:\\", letter);
        let mut out = [0u16; 128];
        let ok = unsafe {
            GetVolumeNameForVolumeMountPointW(
                w(&mount).as_ptr(),
                out.as_mut_ptr(),
                out.len() as u32,
            )
        };
        if ok == 0 {
            None
        } else {
            let vol = from_wide(&out);
            if vol.ends_with('\\') {
                Some(vol)
            } else {
                Some(format!("{}\\", vol))
            }
        }
    }

    struct Session(u32);
    impl Drop for Session {
        fn drop(&mut self) {
            unsafe {
                let _ = RmEndSession(self.0);
            }
        }
    }

    fn start_session() -> Result<Session, String> {
        unsafe {
            let mut h: u32 = 0;
            let mut key = [0u16; (CCH_RM_SESSION_KEY as usize) + 1];
            let rc = RmStartSession(&mut h, 0, key.as_mut_ptr());
            if rc != 0 {
                return Err(format!("RmStartSession rc={}", rc));
            }
            Ok(Session(h))
        }
    }

    fn register_drive(session: &Session, drive_letter: &str) -> Result<(), String> {
        let letter = drive_letter.trim_end_matches(':').to_uppercase();
        let root = format!("{}:\\", letter);
        let vol = volume_guid_root(&letter);

        let mut paths: Vec<Vec<u16>> = vec![w(&root)];
        if let Some(v) = vol {
            paths.push(w(&v));
        }

        let ptrs: Vec<*const u16> = paths.iter().map(|p| p.as_ptr()).collect();
        unsafe {
            let rc = RmRegisterResources(
                session.0,
                ptrs.len() as u32,
                ptrs.as_ptr(),
                0,
                std::ptr::null(),
                0,
                std::ptr::null(),
            );
            if rc != 0 {
                return Err(format!("RmRegisterResources rc={}", rc));
            }
        }
        Ok(())
    }

    pub fn list_occupants(drive_letter: &str) -> Result<Vec<Occupant>, String> {
        let s = start_session()?;
        register_drive(&s, drive_letter)?;

        unsafe {
            let mut needed: u32 = 0;
            let mut count: u32 = 0;
            let mut reboot: u32 = 0;

            let rc1 = RmGetList(
                s.0,
                &mut needed,
                &mut count,
                std::ptr::null_mut(),
                &mut reboot,
            );
            if rc1 != 0 && rc1 != ERROR_MORE_DATA {
                return Err(format!("RmGetList rc={}", rc1));
            }
            if needed == 0 {
                return Ok(vec![]);
            }

            let mut infos: Vec<RM_PROCESS_INFO> = vec![std::mem::zeroed(); needed as usize];
            count = needed;

            let rc2 = RmGetList(
                s.0,
                &mut needed,
                &mut count,
                infos.as_mut_ptr(),
                &mut reboot,
            );
            if rc2 != 0 {
                return Err(format!("RmGetList#2 rc={}", rc2));
            }

            let mut out = Vec::with_capacity(count as usize);
            for p in infos.into_iter().take(count as usize) {
                let pid = p.Process.dwProcessId;
                let app = from_wide(&p.strAppName);
                let svc = from_wide(&p.strServiceShortName);

                let name = if !app.is_empty() {
                    app.clone()
                } else {
                    "Unknown".into()
                };
                let desc = if !svc.is_empty() {
                    format!("RestartManager：{} (服务:{})", app, svc)
                } else {
                    format!("RestartManager：{}", app)
                };

                out.push(Occupant { pid, name, desc });
            }
            Ok(out)
        }
    }

    pub fn shutdown_occupants(drive_letter: &str, force: bool) -> Result<(), String> {
        let s = start_session()?;
        register_drive(&s, drive_letter)?;

        let flags = if force { 1 } else { 0 }; // RmForceShutdown
        unsafe {
            let rc = RmShutdown(s.0, flags, None);
            if rc != 0 {
                return Err(format!("RmShutdown rc={}", rc));
            }
        }
        Ok(())
    }
}

// ═══════════════════════════════════════════════════════════════
//  极客命令封装 (Geek Commands) - 调用系统原生工具
// ═══════════════════════════════════════════════════════════════
mod geek_commands {
    use std::process::Command;
    use std::os::windows::process::CommandExt;

    const CREATE_NO_WINDOW: u32 = 0x08000000;

    /// 辅助函数：尝试刷新卷缓冲区（最大限度保护数据）
    pub fn try_flush(drive: &str) {
        use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
        use windows_sys::Win32::Storage::FileSystem::{
            CreateFileW, FlushFileBuffers, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, FILE_SHARE_WRITE,
            OPEN_EXISTING,
        };
        
        let drive_path = format!("\\\\.\\{}:", drive);
        let path_wide: Vec<u16> = drive_path.encode_utf16().chain(std::iter::once(0)).collect();
        
        unsafe {
            let handle = CreateFileW(
                path_wide.as_ptr(),
                0x80000000 | 0x40000000, // GENERIC_READ | GENERIC_WRITE
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                std::ptr::null(),
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL,
                0,
            );
            if handle != INVALID_HANDLE_VALUE {
                let _ = FlushFileBuffers(handle);
                CloseHandle(handle);
            }
        }
    }

    /// 方法 1: fsutil dismount (推荐！最干净)
    /// 相当于 FSCTL_DISMOUNT_VOLUME，但由系统工具执行，更稳定
    pub fn eject_by_fsutil(drive_letter: &str) -> Result<(), String> {
        let drive = drive_letter.trim_end_matches([':', '\\', '/']);
        
        // 1. 先尝试刷盘，保护数据
        try_flush(drive);

        // fsutil volume dismount E:
        let output = Command::new("fsutil")
            .args(["volume", "dismount", &format!("{}:", drive)])
            .creation_flags(CREATE_NO_WINDOW)
            .output()
            .map_err(|e| format!("无法启动 fsutil: {}", e))?;

        if output.status.success() {
            Ok(())
        } else {
            let err = String::from_utf8_lossy(&output.stderr).to_string();
            // 即使报错，有时候也可能生效，或者是 "没有装载卷" 之类的错误
            if err.contains("没有装载") || err.contains("not mounted") {
                Ok(())
            } else {
                Err(err)
            }
        }
    }
}

// ═══════════════════════════════════════════════════════════════
//  主应用逻辑
// ═══════════════════════════════════════════════════════════════

struct GeekKillerApp {
    // UI 状态
    search_query: String,
    is_admin: bool,
    show_performance: bool,
    show_diagnostics: bool,
    show_usb_manager: bool,

    // USB 状态
    usb_state: UsbState,
    usb_tx: mpsc::Sender<UsbCmd>,
    usb_rx: mpsc::Receiver<UsbMsg>,
    usb_status_msg: String,
    usb_msg_time: Option<Instant>,

    // 数据快照（从后台线程获取）
    snapshot: Arc<RwLock<AppSnapshot>>,

    // 配置
    #[allow(dead_code)]
    auto_low_power: bool,
    #[allow(dead_code)]
    enhanced_mode: bool,

    // 视图控制
    paused: bool,
    cached_snapshot: Arc<AppSnapshot>,
    last_tight_state: bool, // 记录上一次的负载状态，用于边缘触发
}

fn norm_drive(d: &str) -> String {
    d.trim_end_matches([':', '\\', '/']).to_uppercase()
}

/// 智能弹出：尝试刷新驱动器文件缓冲 (Sync) 并强制卸载卷 (Dismount)
/// 并尝试弹出物理设备（解决 VetoType 6）
fn smart_eject(drive: &str) -> Result<(), String> {
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FlushFileBuffers, FILE_SHARE_READ, FILE_SHARE_WRITE,
        OPEN_EXISTING,
    };
    use windows_sys::Win32::System::Ioctl::{FSCTL_DISMOUNT_VOLUME, FSCTL_LOCK_VOLUME};
    use windows_sys::Win32::System::IO::DeviceIoControl;

    let drive_letter = drive.trim_end_matches([':', '\\', '/']);
    let drive_path = format!("\\\\.\\{}:", drive_letter);
    let path_wide: Vec<u16> = drive_path.encode_utf16().chain(std::iter::once(0)).collect();

    // 1. 打开设备句柄
    let (handle, sdn) = unsafe {
        let h = CreateFileW(
            path_wide.as_ptr(),
            0x80000000 | 0x40000000, // GENERIC_READ | GENERIC_WRITE
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            std::ptr::null(),
            OPEN_EXISTING,
            0,
            0,
        );
        if h == INVALID_HANDLE_VALUE {
            return Err("无法打开驱动器 (权限不足或不存在)".to_string());
        }
        
        // 获取设备号以便后续 PnP 弹出
        let mut sdn: STORAGE_DEVICE_NUMBER = std::mem::zeroed();
        let mut bytes_returned = 0u32;
        let mut has_sdn = false;
        if DeviceIoControl(
            h,
            IOCTL_STORAGE_GET_DEVICE_NUMBER,
            std::ptr::null(),
            0,
            &mut sdn as *mut _ as _,
            std::mem::size_of::<STORAGE_DEVICE_NUMBER>() as u32,
            &mut bytes_returned,
            std::ptr::null_mut(),
        ) != 0 {
            has_sdn = true;
        }
        
        (h, if has_sdn { Some(sdn) } else { None })
    };

    unsafe {
        // 2. 尝试 Flush
        let _ = FlushFileBuffers(handle);

        // 3. 尝试 Lock (多次)
        let mut bytes_returned = 0u32;
        let mut _locked = false;
        for _ in 0..5 {
             if DeviceIoControl(handle, FSCTL_LOCK_VOLUME, std::ptr::null(), 0, std::ptr::null_mut(), 0, &mut bytes_returned, std::ptr::null_mut()) != 0 {
                 _locked = true;
                 break;
             }
             std::thread::sleep(std::time::Duration::from_millis(100));
        }
        
        // 4. 强制 Dismount (即使 Lock 失败也尝试)
        DeviceIoControl(handle, FSCTL_DISMOUNT_VOLUME, std::ptr::null(), 0, std::ptr::null_mut(), 0, &mut bytes_returned, std::ptr::null_mut());
        
        // 必须确保关闭句柄
        CloseHandle(handle);
    }
    
    // 给系统一点时间反应 Dismount
    std::thread::sleep(std::time::Duration::from_millis(500));
    
    // 5. 尝试 PnP 弹出 (如果有 SDN)
    if let Some(sdn) = sdn {
        // 重试机制：PnP 弹出有时候需要等句柄彻底释放
        for _ in 0..3 {
            if find_and_eject_device(sdn.DeviceNumber, sdn.DeviceType).is_ok() {
                return Ok(());
            }
            std::thread::sleep(std::time::Duration::from_millis(500));
        }
        // 如果3次都失败，再报最后一次的错
        find_and_eject_device(sdn.DeviceNumber, sdn.DeviceType)
    } else {
        // 降级方案：普通弹出
        device::eject(drive_letter).map_err(|e| e.to_string())
    }
}

fn find_and_eject_device(
    target_device_number: u32,
    target_device_type: u32,
) -> Result<(), String> {
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };
    use windows_sys::Win32::System::IO::DeviceIoControl;

    unsafe {
        let dev_info_set = SetupDiGetClassDevsW(
            &GUID_DEVINTERFACE_DISK,
            std::ptr::null(),
            0,
            DIGCF_PRESENT | DIGCF_DEVICEINTERFACE,
        );
        if dev_info_set == -1isize as _ {
            return Err("无法枚举磁盘设备列表".to_string());
        }

        let mut member_index = 0u32;
        let mut found = false;

        loop {
            let mut iface_data: SP_DEVICE_INTERFACE_DATA = std::mem::zeroed();
            iface_data.cbSize = std::mem::size_of::<SP_DEVICE_INTERFACE_DATA>() as u32;

            if SetupDiEnumDeviceInterfaces(
                dev_info_set,
                std::ptr::null(),
                &GUID_DEVINTERFACE_DISK,
                member_index,
                &mut iface_data,
            ) == 0
            {
                break;
            }

            let mut required_size = 0u32;
            SetupDiGetDeviceInterfaceDetailW(
                dev_info_set,
                &iface_data,
                std::ptr::null_mut(),
                0,
                &mut required_size,
                std::ptr::null_mut(),
            );

            if required_size > 0 {
                let mut buffer = vec![0u8; required_size as usize];
                let detail = buffer.as_mut_ptr() as *mut SP_DEVICE_INTERFACE_DETAIL_DATA_W;
                (*detail).cbSize =
                    std::mem::size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>() as u32;

                let mut devinfo: SP_DEVINFO_DATA = std::mem::zeroed();
                devinfo.cbSize = std::mem::size_of::<SP_DEVINFO_DATA>() as u32;

                if SetupDiGetDeviceInterfaceDetailW(
                    dev_info_set,
                    &iface_data,
                    detail,
                    required_size,
                    std::ptr::null_mut(),
                    &mut devinfo,
                ) != 0
                {
                    let path_ptr = &(*detail).DevicePath as *const u16;
                    let mut len = 0;
                    while *path_ptr.add(len) != 0 {
                        len += 1;
                    }
                    let device_path =
                        String::from_utf16_lossy(std::slice::from_raw_parts(path_ptr, len));

                    let dp_w: Vec<u16> =
                        device_path.encode_utf16().chain(std::iter::once(0)).collect();
                    let disk_handle = CreateFileW(
                        dp_w.as_ptr(),
                        0,
                        FILE_SHARE_READ | FILE_SHARE_WRITE,
                        std::ptr::null(),
                        OPEN_EXISTING,
                        0,
                        0,
                    );

                    if disk_handle != INVALID_HANDLE_VALUE {
                        // 获取设备号比对
                        let mut sdn: STORAGE_DEVICE_NUMBER = std::mem::zeroed();
                        let mut bytes = 0u32;
                        let ok = DeviceIoControl(
                            disk_handle,
                            IOCTL_STORAGE_GET_DEVICE_NUMBER,
                            std::ptr::null(), 0,
                            &mut sdn as *mut _ as _,
                            std::mem::size_of::<STORAGE_DEVICE_NUMBER>() as u32,
                            &mut bytes,
                            std::ptr::null_mut()
                        );
                        CloseHandle(disk_handle);

                        if ok != 0 && sdn.DeviceNumber == target_device_number
                            && sdn.DeviceType == target_device_type
                        {
                            // 尝试弹出父设备 (关键修复：解决 VetoType 6)
                            let mut parent_inst = 0u32;
                            if CM_Get_Parent(&mut parent_inst, devinfo.DevInst, 0)
                                == CR_SUCCESS
                            {
                                let mut veto_type = 0i32;
                                let mut veto_name = [0u16; 260];
                                if CM_Request_Device_EjectW(
                                    parent_inst,
                                    &mut veto_type,
                                    veto_name.as_mut_ptr(),
                                    260,
                                    0,
                                ) == CR_SUCCESS
                                {
                                    found = true;
                                }
                            }
                            // 如果父设备弹出失败，尝试弹出当前设备
                            if !found {
                                let mut veto_type = 0i32;
                                if CM_Request_Device_EjectW(
                                    devinfo.DevInst,
                                    &mut veto_type,
                                    std::ptr::null_mut(),
                                    0,
                                    0,
                                ) == CR_SUCCESS
                                {
                                    found = true;
                                }
                            }
                            if found {
                                break;
                            }
                        }
                    }
                }
            }
            member_index += 1;
        }

        SetupDiDestroyDeviceInfoList(dev_info_set);

        if found {
            SHChangeNotify(0x00002000, 0x0005, std::ptr::null(), std::ptr::null());
            Ok(())
        } else {
            Err("硬件拒绝弹出 (VetoType 6)。请尝试关闭所有窗口后重试。".to_string())
        }
    }
}

/// 后台 USB 工作线程
fn usb_worker(cmd_rx: mpsc::Receiver<UsbCmd>, msg_tx: mpsc::Sender<UsbMsg>, ctx: egui::Context) {
    let send = |s: UsbState| {
        let _ = msg_tx.send(UsbMsg::State(s));
        ctx.request_repaint();
    };

    // 辅助函数：手动扫描进程占用 (fallback)
    // 当 RM 失败时，尝试通过 sysinfo 扫描进程的 exe/cwd 是否在目标驱动器上
    let scan_processes_fallback = |drive: &str| -> Vec<Occupant> {
        let drive_upper = drive.trim_end_matches([':', '\\', '/']).to_uppercase();
        let drive_prefix = format!("{}:", drive_upper); // "I:"

        let mut list = Vec::new();
        let mut sys = System::new();
        // 只需要 EXE 和 CWD 信息
        sys.refresh_processes_specifics(
            sysinfo::ProcessesToUpdate::All,
            true,
            ProcessRefreshKind::new()
                .with_exe(sysinfo::UpdateKind::Always)
                .with_cwd(sysinfo::UpdateKind::Always),
        );

        for (pid, proc) in sys.processes() {
            let mut is_occupying = false;
            let mut reason = String::new();

            // Check EXE path
            if let Some(exe) = proc.exe() {
                if let Some(exe_str) = exe.to_str() {
                    if exe_str.to_uppercase().starts_with(&drive_prefix) {
                        is_occupying = true;
                        reason = "正在运行".to_string();
                    }
                }
            }

            // Check CWD
            if !is_occupying {
                if let Some(cwd) = proc.cwd() {
                    if let Some(cwd_str) = cwd.to_str() {
                        if cwd_str.to_uppercase().starts_with(&drive_prefix) {
                            is_occupying = true;
                            reason = "工作目录".to_string();
                        }
                    }
                }
            }

            if is_occupying {
                let name = proc.name().to_string_lossy().to_string();
                // 尝试获取中文描述
                let desc = if let Some(exe) = proc.exe() {
                    if let Some(d) = get_exe_file_description(exe) {
                        format!("{} ({})", d, reason)
                    } else {
                        format!("{} ({})", name, reason)
                    }
                } else {
                    format!("{} ({})", name, reason)
                };

                list.push(Occupant {
                    pid: pid.as_u32(),
                    name,
                    desc,
                });
            }
        }
        list
    };

    while let Ok(cmd) = cmd_rx.recv() {
        match cmd {
            UsbCmd::Scan(drive) => {
                let d = norm_drive(&drive);
                send(UsbState::Ejecting(format!("{}:", d)));

                // 快速尝试：简单弹出 (CM_Request_Device_EjectW)
                // 不做 Dismount/Lock，追求秒开
                match device::eject(&d) {
                    Ok(_) => send(UsbState::Done(format!("✅ 驱动器 {}: 已安全弹出", d))),
                    Err(e) => {
                        // 失败才扫描占用
                        send(UsbState::Scanning(format!("{}:", d)));

                        // 1. 尝试 RM 扫描
                        let mut list = rm::list_occupants(&d).unwrap_or_default();

                        // 2. 如果 RM 没找到，尝试手动 fallback 扫描
                        let fallback_list = scan_processes_fallback(&d);
                        for item in fallback_list {
                            if !list.iter().any(|x| x.pid == item.pid) {
                                list.push(item);
                            }
                        }

                        // 翻译错误信息
                        let err_msg = e.to_string();
                        let friendly_err = if list.is_empty() {
                            if err_msg.contains("VetoType: 6") || err_msg.contains("CONFIGRET(23)")
                            {
                                "无法弹出：系统核心组件或驱动锁定。请尝试关闭所有窗口。".to_string()
                            } else {
                                format!("弹出失败：{}", err_msg)
                            }
                        } else {
                            format!("弹出失败：{} (发现占用)", err_msg)
                        };

                        if list.is_empty() {
                            // 列表为空，可能是窗口未关闭或资源管理器锁定
                            send(UsbState::Done(format!("❌ {}", friendly_err)));
                            send(UsbState::Occupied {
                                drive: format!("{}:", d),
                                list: vec![],
                            });
                        } else {
                            send(UsbState::Occupied {
                                drive: format!("{}:", d),
                                list,
                            });
                        }
                    }
                }
            }

            UsbCmd::KillOne(pid, drive) => {
                send(UsbState::Scanning(format!(
                    "{}: 正在终止占用进程...",
                    drive
                )));
                let _ = rust_core_lib::process::kill(pid);
                std::thread::sleep(Duration::from_millis(200));

                // 杀完一个后，重新扫描占用
                let d = norm_drive(&drive);
                let list = rm::list_occupants(&d).unwrap_or_default();
                // 自动尝试弹出
                if list.is_empty() {
                    send(UsbState::Ejecting(format!("{}:", d)));
                    match smart_eject(&d) {
                        Ok(_) => send(UsbState::Done(format!("✅ 驱动器 {}: 已安全弹出", d))),
                        Err(_) => {
                            // 如果还是失败，回到 Occupied 状态让用户强制弹出
                            send(UsbState::Occupied {
                                drive: format!("{}:", d),
                                list: vec![],
                            });
                        }
                    }
                } else {
                    send(UsbState::Occupied {
                        drive: format!("{}:", d),
                        list,
                    });
                }
            }

            UsbCmd::ForceEject(drive, pids) => {
                let d = norm_drive(&drive);
                send(UsbState::Scanning(format!("{}: 正在强制清场...", d)));

                // 1. RM 强制释放 (Force Shutdown)
                let _ = rm::shutdown_occupants(&d, true);

                // 2. Kill 指定 PID (以及重新扫描到的残留)
                for pid in &pids {
                    let _ = rust_core_lib::process::kill(*pid);
                }
                
                // 再次扫描是否有漏网之鱼
                let fallback = scan_processes_fallback(&d);
                for p in fallback {
                    let _ = rust_core_lib::process::kill(p.pid);
                }

                std::thread::sleep(Duration::from_millis(300));

                // 3. 强力弹出 (Smart Eject: Flush -> Lock -> Dismount -> ParentEject)
                let mut last_err = String::new();
                let mut success = false;

                if smart_eject(&d).is_ok() {
                    success = true;
                } else {
                    // 如果失败，尝试 fsutil 辅助
                    let _ = geek_commands::eject_by_fsutil(&d);
                    std::thread::sleep(Duration::from_millis(500));
                    
                    match smart_eject(&d) {
                        Ok(_) => success = true,
                        Err(e) => last_err = e,
                    }
                }

                if success {
                    // 尝试刷新资源管理器 (通知系统)
                    unsafe { SHChangeNotify(0x00002000, 0x0005, std::ptr::null(), std::ptr::null()); }
                    send(UsbState::Done(format!("✅ 驱动器 {}: 已强制弹出", d)));
                } else {
                    let friendly =
                        if last_err.contains("VetoType: 6") || last_err.contains("CONFIGRET(23)") {
                            "系统核心组件锁定，强制移除失败。请重启电脑。"
                        } else {
                            &last_err
                        };

                    send(UsbState::Done(format!("❌ {}", friendly)));
                }
                
                // 刷新系统磁盘列表
                let mut disks = Disks::new_with_refreshed_list();
                disks.refresh_list();
            }

            UsbCmd::FsutilDismount(drive) => {
                let d = norm_drive(&drive);
                send(UsbState::Scanning(format!("{}: 正在执行 fsutil dismount...", d)));
                
                match geek_commands::eject_by_fsutil(&d) {
                    Ok(_) => {
                        send(UsbState::Ejecting(format!("{}: 卷已强制卸载，尝试弹出...", d)));
                        std::thread::sleep(Duration::from_millis(500));
                        match smart_eject(&d) {
                            Ok(_) => send(UsbState::Done(format!("✅ 驱动器 {}: 已安全弹出 (fsutil)", d))),
                            Err(e) => {
                                // 失败才扫描占用
                                send(UsbState::Done(format!("❌ fsutil 成功但弹出失败：{}", e)));
                                let list = rm::list_occupants(&d).unwrap_or_default();
                                send(UsbState::Occupied { drive: format!("{}:", d), list });
                            }
                        }
                    }
                    Err(e) => send(UsbState::Done(format!("❌ fsutil 执行失败：{}", e))),
                }
                
                // 刷新系统磁盘列表
                let mut disks = Disks::new_with_refreshed_list();
                disks.refresh_list();
            }
        }
    }
}

/// 后台监控线程：解决 UI 卡顿的关键
fn monitor_worker(
    snapshot: Arc<RwLock<AppSnapshot>>,
    process_db: HashMap<String, ProcessInfo>,
    ctx: egui::Context,
) {
    let mut sys = System::new_all();
    let mut networks = Networks::new_with_refreshed_list();
    let mut disks = Disks::new_with_refreshed_list();

    // 缓存，避免每次重新分配
    let mut groups_buffer: HashMap<String, ProcessGroup> = HashMap::with_capacity(512);
    // 缓存文件描述，避免重复 I/O (Key: exe_path string)
    let mut desc_cache: HashMap<String, String> = HashMap::with_capacity(512);

    // 资源紧张模式的滞后计数器 (0..=5)
    // >= 3 进入紧张模式, < 3 退出
    let mut tight_counter = 0;

    // 快照版本号，用于减少 UI 锁竞争
    #[allow(unused_assignments)]
    let mut snapshot_version = 0u64;

    loop {
        let start_time = Instant::now();

        // 1. 刷新数据 (耗时操作)
        sys.refresh_cpu_usage();
        sys.refresh_memory();

        // 强制刷新 EXE 路径
        let refresh_kind = ProcessRefreshKind::new()
            .with_cpu()
            .with_memory()
            .with_exe(sysinfo::UpdateKind::Always)
            .with_disk_usage();
        sys.refresh_processes_specifics(sysinfo::ProcessesToUpdate::All, true, refresh_kind);

        networks.refresh();
        disks.refresh_list(); // 刷新磁盘列表以检测插拔

        // 2. 处理进程分组
        groups_buffer.clear();
        for (pid, proc) in sys.processes() {
            let name = proc.name().to_string_lossy().to_string();
            let name_lower = name.to_lowercase();

            // 识别逻辑
            let info = {
                let mut found = None;

                // 0. 优先匹配硬编码映射 (解决部分国产软件/浏览器 FileDescription 不友好的问题)
                if name_lower.contains("firefox") {
                    found = Some(ProcessInfo::new("火狐浏览器", "浏览器"));
                } else if name_lower.contains("doubao") {
                    found = Some(ProcessInfo::new("豆包 (AI助手)", "AI助手"));
                } else if name_lower.contains("dingtalk") {
                    found = Some(ProcessInfo::new("钉钉", "办公"));
                } else if name_lower.contains("feishu") {
                    found = Some(ProcessInfo::new("飞书", "办公"));
                } else if name_lower.contains("wechat") {
                    found = Some(ProcessInfo::new("微信", "通讯"));
                } else if name_lower.contains("qq") {
                    found = Some(ProcessInfo::new("QQ", "通讯"));
                }

                // 1. 尝试从文件描述获取
                if found.is_none() {
                    if let Some(exe_path) = proc.exe() {
                        let path_key = exe_path.to_string_lossy().to_string();
                        if let Some(cached_desc) = desc_cache.get(&path_key) {
                            found = Some(ProcessInfo::new(cached_desc, "应用"));
                        } else if let Some(desc) = get_exe_file_description(exe_path) {
                            desc_cache.insert(path_key, desc.clone());
                            found = Some(ProcessInfo::new(&desc, "应用"));
                        }
                    }
                }

                // 数据库兜底
                if found.is_none() {
                    if let Some(db_info) = process_db.get(&name_lower) {
                        found = Some(db_info.clone());
                    }
                }
                // 路径规则兜底
                found.unwrap_or_else(|| {
                    let exe_path_str = proc
                        .exe()
                        .map(|p| p.to_string_lossy().to_lowercase())
                        .unwrap_or_default();

                    let (friendly, cat) = if exe_path_str.contains("windows\\system32")
                        || exe_path_str.contains("windows\\syswow64")
                    {
                        ("Windows 系统组件", "系统")
                    } else if exe_path_str.contains("program files") {
                        if exe_path_str.contains("nvidia") {
                            ("NVIDIA 驱动", "驱动")
                        } else if exe_path_str.contains("steam") {
                            ("Steam", "游戏")
                        } else {
                            ("", "第三方应用")
                        }
                    } else {
                        ("", "应用")
                    };
                    ProcessInfo::new(friendly, cat)
                })
            };

            let entry = groups_buffer.entry(name.clone()).or_insert(ProcessGroup {
                name,
                friendly_name: info.chinese_name,
                category: info.category,
                total_memory: 0,
                total_cpu: 0.0,
                pids: Vec::new(),
                is_system: false,
                is_not_responding: false,
            });

            entry.total_memory += proc.memory();
            entry.total_cpu += proc.cpu_usage();
            entry.pids.push(pid.as_u32());

            if pid.as_u32() < 1000 || entry.category == "系统" {
                entry.is_system = true;
            }
            if matches!(
                proc.status(),
                sysinfo::ProcessStatus::UninterruptibleDiskSleep | sysinfo::ProcessStatus::Dead
            ) {
                entry.is_not_responding = true;
            }
        }

        // 3. 排序与分类
        let mut all_groups: Vec<ProcessGroup> = groups_buffer.values().cloned().collect();
        all_groups.sort_by(|a, b| b.total_memory.cmp(&a.total_memory));

        let mut new_snapshot = AppSnapshot::default();

        for group in all_groups {
            if group.total_cpu > 10.0 || group.total_memory > 500 * 1024 * 1024 {
                new_snapshot.high_resource.push(group);
            } else if group.is_system {
                new_snapshot.system_groups.push(group);
            } else {
                new_snapshot.other_groups.push(group);
            }
        }

        // 4. 全局数据
        new_snapshot.global_cpu = sys.global_cpu_usage();
        new_snapshot.used_memory = sys.used_memory();
        new_snapshot.total_memory = sys.total_memory();

        // 智能资源模式判定 (滞后处理)
        let is_tight_now =
            new_snapshot.global_cpu > 90.0 || sys.available_memory() < 500 * 1024 * 1024;
        if is_tight_now {
            if tight_counter < 5 {
                tight_counter += 1;
            }
        } else if tight_counter > 0 {
            tight_counter -= 1;
        }
        new_snapshot.is_resource_tight = tight_counter >= 3;

        // 网络
        let mut net_in = 0;
        let mut net_out = 0;
        for (_, data) in &networks {
            net_in += data.received();
            net_out += data.transmitted();
        }
        new_snapshot.network_in = net_in;
        new_snapshot.network_out = net_out;

        // 磁盘
        for disk in &disks {
            let mp = disk.mount_point().to_string_lossy().to_string();
            let mp_clean = mp.trim_end_matches(['\\', '/']).to_string();

            let is_sys = if let Ok(sys_drive) = std::env::var("SystemDrive") {
                mp_clean
                    .to_uppercase()
                    .starts_with(&sys_drive.to_uppercase())
            } else {
                mp_clean.to_uppercase().starts_with('C')
            };

            let is_removable = device::is_removable(&mp_clean) && !is_sys;

            new_snapshot.disks.push(DiskData {
                mount_point: mp,
                name: disk.name().to_string_lossy().to_string(),
                available_space: disk.available_space(),
                total_space: disk.total_space(),
                is_removable,
            });
        }

        // 5. 更新共享状态
        // 仅在数据真正准备好后获取写锁
        if let Ok(mut lock) = snapshot.write() {
            *lock = new_snapshot;
            snapshot_version = snapshot_version.wrapping_add(1);
        }

        // 6. 通知 UI
        ctx.request_repaint();

        // 智能休眠：根据负载自适应调整刷新率
        // 正常模式: 500ms (2Hz) - 保证流畅
        // 极简模式: 2000ms (0.5Hz) - 让出 CPU 资源
        let target_interval = if is_tight_now {
            Duration::from_millis(2000)
        } else {
            Duration::from_millis(500)
        };

        let elapsed = start_time.elapsed();
        if elapsed < target_interval {
            std::thread::sleep(target_interval - elapsed);
        }
    }
}

// ═══════════════════════════════════════════════════════════════
//  UI 实现
// ═══════════════════════════════════════════════════════════════

// 构建已知进程数据库
fn build_known_processes() -> HashMap<String, ProcessInfo> {
    let mut m = HashMap::new();
    m.insert("svchost.exe".into(), ProcessInfo::new("系统服务宿主", "系统"));
    m.insert("explorer.exe".into(), ProcessInfo::new("资源管理器", "系统"));
    m.insert("dwm.exe".into(), ProcessInfo::new("桌面窗口管理器", "系统"));
    m.insert("searchindexer.exe".into(), ProcessInfo::new("Windows 搜索索引", "系统"));
    m.insert("msedge.exe".into(), ProcessInfo::new("Edge 浏览器", "浏览器"));
    m.insert("chrome.exe".into(), ProcessInfo::new("Chrome 浏览器", "浏览器"));
    m.insert("wechat.exe".into(), ProcessInfo::new("微信", "通讯"));
    m.insert("qq.exe".into(), ProcessInfo::new("QQ", "通讯"));
    m.insert("dingtalk.exe".into(), ProcessInfo::new("钉钉", "办公"));
    m.insert("feishu.exe".into(), ProcessInfo::new("飞书", "办公"));
    m.insert("code.exe".into(), ProcessInfo::new("VS Code", "开发"));
    m.insert("steam.exe".into(), ProcessInfo::new("Steam", "游戏"));
    m
}

impl GeekKillerApp {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        ui::setup_custom_fonts(&cc.egui_ctx);

        let mut visuals = egui::Visuals::dark();
        visuals.panel_fill = egui::Color32::from_rgb(20, 18, 15);
        cc.egui_ctx.set_visuals(visuals);

        let (usb_tx, app_rx) = mpsc::channel();
        let (app_tx, usb_rx) = mpsc::channel();
        let ctx_clone = cc.egui_ctx.clone();

        // 启动 USB 线程
        std::thread::spawn(move || {
            usb_worker(app_rx, app_tx, ctx_clone);
        });

        // 启动监控线程
        let snapshot = Arc::new(RwLock::new(AppSnapshot::default()));
        let snapshot_clone = snapshot.clone();
        let ctx_clone2 = cc.egui_ctx.clone();
        let db = build_known_processes();

        std::thread::spawn(move || {
            monitor_worker(snapshot_clone, db, ctx_clone2);
        });

        Self {
            search_query: String::new(),
            is_admin: security::is_admin(),
            show_performance: false,
            show_diagnostics: false,
            show_usb_manager: false, // 默认折叠
            usb_state: UsbState::Idle,
            usb_tx,
            usb_rx,
            usb_status_msg: String::new(),
            usb_msg_time: None,
            snapshot,
            auto_low_power: true,
            enhanced_mode: false,
            paused: false,
            cached_snapshot: Arc::new(AppSnapshot::default()),
            last_tight_state: false,
        }
    }

    fn render_process_table(
        &self,
        ui: &mut egui::Ui,
        ctx: &egui::Context,
        groups: &[ProcessGroup],
        is_high: bool,
    ) {
        let scale = ctx.pixels_per_point();
        let rounding = ui::UiConstants::ROUNDING * scale;
        let text_color = egui::Color32::from_rgb(218, 165, 32);

        let available_width = ui.available_width() - 40.0;
        let name_col_width = (available_width - 320.0).max(150.0);

        egui::Grid::new(format!("grid_{}", if is_high { "high" } else { "norm" }))
            .num_columns(5)
            .spacing([15.0, 10.0])
            .striped(true)
            .show(ui, |ui| {
                // Headers
                ui.add_sized(
                    [40.0, 20.0],
                    egui::Label::new(egui::RichText::new("数量").strong().color(text_color)),
                );
                ui.add_sized(
                    [name_col_width, 20.0],
                    egui::Label::new(egui::RichText::new("进程名称").strong().color(text_color)),
                );
                ui.add_sized(
                    [90.0, 20.0],
                    egui::Label::new(egui::RichText::new("总内存").strong().color(text_color)),
                );
                ui.add_sized(
                    [70.0, 20.0],
                    egui::Label::new(egui::RichText::new("总CPU").strong().color(text_color)),
                );
                ui.add_sized(
                    [80.0, 20.0],
                    egui::Label::new(egui::RichText::new("操作").strong().color(text_color)),
                );
                ui.end_row();

                for group in groups {
                    ui.add_sized(
                        [40.0, 20.0],
                        egui::Label::new(
                            egui::RichText::new(format!("x{}", group.pids.len())).monospace(),
                        ),
                    );

                    // Name
                    ui.add_sized([name_col_width, 20.0], |ui: &mut egui::Ui| {
                        ui.horizontal(|ui| {
                            let name_color = if is_high {
                                egui::Color32::from_rgb(255, 140, 0)
                            } else {
                                egui::Color32::from_rgb(200, 180, 150)
                            };
                            let display = if group.friendly_name.is_empty() {
                                group.name.clone()
                            } else {
                                format!("{} ({})", group.friendly_name, group.name)
                            };

                            if !group.category.is_empty() {
                                ui.label(
                                    egui::RichText::new(format!("[{}]", group.category))
                                        .color(egui::Color32::GRAY)
                                        .small(),
                                );
                            }
                            ui.add(
                                egui::Label::new(
                                    egui::RichText::new(display).color(name_color).strong(),
                                )
                                .truncate(),
                            );

                            if group.is_system {
                                ui.label(
                                    egui::RichText::new("SYS")
                                        .small()
                                        .color(egui::Color32::BROWN),
                                );
                            }
                            if group.is_not_responding {
                                ui.label(
                                    egui::RichText::new("DEAD")
                                        .small()
                                        .color(egui::Color32::RED),
                                );
                            }
                        })
                        .response
                    });

                    // Mem
                    ui.add_sized(
                        [90.0, 20.0],
                        egui::Label::new(format!(
                            "{:.1} MB",
                            group.total_memory as f32 / 1024.0 / 1024.0
                        )),
                    );

                    // CPU
                    let cpu_c = if group.total_cpu > 20.0 {
                        egui::Color32::RED
                    } else {
                        egui::Color32::GOLD
                    };
                    ui.add_sized(
                        [70.0, 20.0],
                        egui::Label::new(
                            egui::RichText::new(format!("{:.1}%", group.total_cpu))
                                .color(cpu_c)
                                .monospace(),
                        ),
                    );

                    // Action
                    ui.add_sized([80.0, 24.0 * scale], |ui: &mut egui::Ui| {
                        let btn = egui::Button::new(
                            egui::RichText::new("终止").color(egui::Color32::WHITE),
                        )
                        .fill(egui::Color32::from_rgb(180, 40, 40))
                        .rounding(rounding / 2.0);
                        let res = ui.add(btn);
                        if res.clicked() {
                            let _ = self
                                .usb_tx
                                .send(UsbCmd::ForceEject("".into(), group.pids.clone()));
                        }
                        res
                    });
                    ui.end_row();
                }
            });
    }
}

impl eframe::App for GeekKillerApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // 处理 USB 消息
        while let Ok(msg) = self.usb_rx.try_recv() {
            let UsbMsg::State(s) = msg;
            self.usb_state = s;
            if let UsbState::Done(ref m) = self.usb_state {
                self.usb_status_msg = m.clone();
                self.usb_msg_time = Some(Instant::now());
            } else {
                // 如果不是 Done 状态，清除旧的完成消息 (Scanning/Ejecting/Occupied)
                self.usb_status_msg.clear();
                self.usb_msg_time = None;
            }
        }

        // 自动清除 Done 消息 (3秒后)
        if let Some(t) = self.usb_msg_time {
            if t.elapsed() > Duration::from_secs(3) {
                self.usb_status_msg.clear();
                self.usb_msg_time = None;
                if matches!(self.usb_state, UsbState::Done(_)) {
                    self.usb_state = UsbState::Idle;
                }
            }
        }

        // 读取快照 (非阻塞 & 零拷贝优化)
        // 1. 尝试获取最新数据 (try_read 避免阻塞 UI 线程)
        if !self.paused {
            if let Ok(guard) = self.snapshot.try_read() {
                // 这里发生了深拷贝，但频率受限于后台刷新率 (0.5Hz - 2Hz)
                self.cached_snapshot = Arc::new(guard.clone());
            }
        }
        // Arc Clone，非常廉价，可以在每一帧执行
        let snapshot = self.cached_snapshot.clone();

        // 2. 处理极简模式切换 (边缘触发)
        if snapshot.is_resource_tight && !self.last_tight_state {
            // 进入极简模式：自动折叠耗资源面板
            self.show_performance = false;
            self.show_diagnostics = false;
        }
        self.last_tight_state = snapshot.is_resource_tight;

        let scale = ctx.pixels_per_point();
        let rounding = ui::UiConstants::ROUNDING * scale;

        // 定义主色调：DodgerBlue
        let primary_color = egui::Color32::from_rgb(100, 180, 255);

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.spacing_mut().item_spacing = egui::vec2(
                ui::UiConstants::SPACING * scale,
                ui::UiConstants::SPACING * 1.5 * scale,
            );
            ui.spacing_mut().window_margin =
                egui::Margin::same(ui::UiConstants::SPACING * 2.0 * scale);

            // Header
            ui.horizontal(|ui| {
                ui.vertical(|ui| {
                    ui.heading(
                        egui::RichText::new("GEEK KILLER PRO")
                            .strong()
                            .color(egui::Color32::from_rgb(218, 165, 32)),
                    );
                    ui.label(
                        egui::RichText::new(STAR_TAP_BRAND.display_full())
                            .small()
                            .color(egui::Color32::from_rgb(100, 80, 60)),
                    );
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if snapshot.is_resource_tight {
                        ui.label(
                            egui::RichText::new("⚡ 极简模式")
                                .color(egui::Color32::YELLOW)
                                .small()
                                .strong(),
                        );
                        ui.add_space(8.0);
                    }

                    let mode_text = if self.is_admin {
                        "ADMIN MODE"
                    } else {
                        "USER MODE"
                    };
                    let mode_color = if self.is_admin {
                        egui::Color32::from_rgb(0, 255, 127)
                    } else {
                        egui::Color32::GOLD
                    };
                    ui.label(egui::RichText::new(mode_text).color(mode_color).strong());
                });
            });
            ui.add_space(15.0);

            // Controls
            ui.horizontal(|ui| {
                ui.label("扫描器:");
                ui.add(
                    egui::TextEdit::singleline(&mut self.search_query)
                        .hint_text("搜索进程...")
                        .desired_width(180.0),
                );
                ui.toggle_value(&mut self.show_performance, "性能监测");
                ui.toggle_value(&mut self.show_diagnostics, "智能诊断");
                ui.toggle_value(&mut self.show_usb_manager, "U盘管理");
                
                ui.separator();
                let pause_text = if self.paused { "▶️ 恢复刷新" } else { "⏸️ 锁定视图" };
                if ui.toggle_value(&mut self.paused, pause_text).clicked() {
                    // 当点击时，cached_snapshot 逻辑会在下一帧 update 中自动处理
                }
            });
            ui.add_space(20.0);

            // USB Manager
            if self.show_usb_manager {
                egui::Frame::group(ui.style())
                    .fill(egui::Color32::from_rgb(30, 25, 20))
                    .stroke(egui::Stroke::new(
                        1.0,
                        primary_color,
                    ))
                    .rounding(rounding)
                    .inner_margin(egui::Margin::symmetric(14.0 * scale, 10.0 * scale))
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.label(
                                egui::RichText::new("💾 外部存储管理")
                                    .strong()
                                    .color(primary_color),
                            );
                        });
                        
                        if !self.usb_status_msg.is_empty() {
                            ui.add_space(5.0);
                            let status_color = if self.usb_status_msg.contains("❌") || self.usb_status_msg.contains("失败") {
                                egui::Color32::from_rgb(255, 80, 80) // Red
                            } else {
                                egui::Color32::GREEN
                            };
                            ui.label(
                                egui::RichText::new(&self.usb_status_msg)
                                    .small()
                                    .color(status_color),
                            );
                        }
                        ui.add_space(10.0);
                        match &self.usb_state {
                            UsbState::Scanning(msg) | UsbState::Ejecting(msg) => {
                                ui.horizontal(|ui| {
                                    ui.spinner();
                                    ui.label(egui::RichText::new(msg).color(primary_color));
                                });
                                ui.add_space(10.0);
                            }
                            _ => {}
                        }

                        // 渲染磁盘列表
                        let mut removable = Vec::new();
                        for d in &snapshot.disks {
                            if d.is_removable && d.mount_point.len() <= 3 {
                                removable.push(d);
                            }
                        }

                        if removable.is_empty() {
                            ui.label(
                                egui::RichText::new("未检测到外部驱动器")
                                    .color(egui::Color32::GRAY),
                            );
                        } else {
                            // Occupied Panel
                            let mut cancel_action = false;
                            if let UsbState::Occupied { drive, list } = &self.usb_state {
                                let drive_c = drive.clone();
                                egui::Frame::group(ui.style())
                                    .fill(egui::Color32::from_rgb(45, 40, 35))
                                    .stroke(egui::Stroke::new(1.0, egui::Color32::from_rgb(200, 100, 100)))
                                    .inner_margin(egui::Margin::same(16.0))
                                    .rounding(rounding)
                                    .show(ui, |ui| {
                                        ui.horizontal(|ui| {
                                            ui.label(
                                                egui::RichText::new(format!("⚠️ {} 被占用", drive))
                                                    .color(egui::Color32::GOLD)
                                                    .strong(),
                                            );
                                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                                if ui.button("取消").clicked() {
                                                    cancel_action = true;
                                                }
                                            });
                                        });

                                        ui.add_space(8.0);

                                        // 顶部操作区
                                        ui.horizontal(|ui| {
                                            // 1. 强力清场 (C位)
                                            let kill_btn = egui::Button::new(
                                                egui::RichText::new(" 强力清场 ").color(egui::Color32::WHITE).strong()
                                            ).fill(egui::Color32::from_rgb(200, 60, 60)).rounding(rounding); // Redder

                                            if ui.add(kill_btn).on_hover_text("强制终止相关进程并弹出").clicked() {
                                                let pids = list.iter().map(|o| o.pid).collect();
                                                let _ = self.usb_tx.send(UsbCmd::ForceEject(drive_c.clone(), pids));
                                            }
                                            
                                            ui.add_space(5.0);

                                            // 2. 强制卸载 (fsutil)
                                            let fsutil_btn = egui::Button::new(
                                                egui::RichText::new(" 强制卸载 ").color(egui::Color32::BLACK).strong()
                                            ).fill(egui::Color32::from_rgb(255, 165, 0)).rounding(rounding);

                                            if ui.add(fsutil_btn).on_hover_text("使用系统 fsutil 工具强制卸载卷").clicked() {
                                                let _ = self.usb_tx.send(UsbCmd::FsutilDismount(drive_c.clone()));
                                            }
                                        });

                                        if !list.is_empty() {
                                            ui.add_space(10.0);
                                            ui.separator();
                                            ui.add_space(5.0);
                                            ui.label(egui::RichText::new("检测到以下占用进程：").small().color(egui::Color32::GRAY));

                                            egui::ScrollArea::vertical().max_height(150.0).show(ui, |ui| {
                                                for occ in list {
                                                    ui.horizontal(|ui| {
                                                        ui.label(format!("• {}", occ.desc));
                                                        ui.with_layout(
                                                            egui::Layout::right_to_left(
                                                                egui::Align::Center,
                                                            ),
                                                            |ui| {
                                                                let btn = egui::Button::new(
                                                                    egui::RichText::new("终止").color(egui::Color32::WHITE),
                                                                )
                                                                .fill(egui::Color32::from_rgb(180, 40, 40))
                                                                .rounding(rounding / 2.0);

                                                                if ui.add(btn).clicked() {
                                                                    let _ =
                                                                        self.usb_tx.send(UsbCmd::KillOne(
                                                                            occ.pid,
                                                                            drive_c.clone(),
                                                                        ));
                                                                }
                                                            },
                                                        );
                                                    });
                                                }
                                            });
                                        } else {
                                            ui.add_space(10.0);
                                            ui.label(
                                                egui::RichText::new("⚠️ 未检测到用户程序占用，可能是系统核心组件或驱动锁定。")
                                                    .color(egui::Color32::KHAKI)
                                                    .italics()
                                            );
                                            ui.label(
                                                egui::RichText::new("建议关闭所有窗口，或点击上方【强力清场】。")
                                                    .small()
                                                    .color(egui::Color32::GRAY)
                                            );
                                        }
                                    });
                            }
                            if cancel_action {
                                self.usb_state = UsbState::Idle;
                            }

                            // Disk List
                            for disk in removable {
                                ui.horizontal(|ui| {
                                    let free_gb =
                                        disk.available_space as f32 / 1024.0 / 1024.0 / 1024.0;
                                    let total_gb =
                                        disk.total_space as f32 / 1024.0 / 1024.0 / 1024.0;
                                    let used_ratio = if total_gb > 0.0 {
                                        1.0 - (free_gb / total_gb)
                                    } else {
                                        0.0
                                    };

                                    // 左侧：设备信息与进度条
                                    ui.vertical(|ui| {
                                        // 1. 蓝色设备名称
                                        ui.label(
                                            egui::RichText::new(format!(
                                                "💿 [{}] {} ({:.1}G/{:.1}G)",
                                                disk.mount_point, disk.name, free_gb, total_gb
                                            ))
                                            .color(primary_color) // 舒适的蓝色
                                            .strong(),
                                        );

                                        // 2. 容量进度条
                                        ui.add(
                                            egui::ProgressBar::new(used_ratio)
                                                .desired_width(320.0)
                                                .desired_height(6.0)
                                                .rounding(rounding)
                                                .fill(primary_color)
                                                .animate(false)
                                        );
                                    });

                                    // 右侧：安全弹出按钮
                                    ui.with_layout(
                                        egui::Layout::right_to_left(egui::Align::Center),
                                        |ui| {
                                            // 统一“安全弹出”按钮风格
                                            let btn = egui::Button::new(
                                                egui::RichText::new("  安全弹出  ")
                                                    .color(egui::Color32::WHITE)
                                                    .strong(),
                                            )
                                            .fill(egui::Color32::from_rgb(46, 139, 87)) // SeaGreen
                                            .rounding(rounding)
                                            .min_size(egui::vec2(80.0, 28.0));

                                            ui.add_space(5.0);
                                            if ui.add(btn).clicked() {
                                                let _ = self
                                                    .usb_tx
                                                    .send(UsbCmd::Scan(disk.mount_point.clone()));
                                            }
                                        },
                                    );
                                });
                                ui.add_space(8.0);
                            }
                        }
                    });
                ui.add_space(10.0);
            }

            // Diagnostics
            if self.show_diagnostics {
                egui::Frame::group(ui.style()).show(ui, |ui| {
                    ui.label(
                        egui::RichText::new("🔍 智能诊断")
                            .strong()
                            .color(egui::Color32::GOLD),
                    );
                    if snapshot.is_resource_tight {
                        ui.label(
                            egui::RichText::new("⚠️ 资源紧张，已进入极简模式")
                                .color(egui::Color32::RED),
                        );
                    } else {
                        ui.label(
                            egui::RichText::new("✨ 系统运行流畅").color(egui::Color32::GREEN),
                        );
                    }
                });
                ui.add_space(10.0);
            }

            // Performance
            if self.show_performance {
                egui::Frame::group(ui.style())
                    .fill(egui::Color32::from_rgb(25, 20, 20))
                    .stroke(egui::Stroke::new(1.0, egui::Color32::from_rgb(50, 50, 50)))
                    .show(ui, |ui| {
                        ui.label(egui::RichText::new("📊 系统遥测面板").strong().color(egui::Color32::GOLD));
                        ui.add_space(5.0);

                        let make_color = |val: f32, warn: f32, crit: f32| {
                            if val > crit {
                                egui::Color32::RED
                            } else if val > warn {
                                egui::Color32::GOLD
                            } else {
                                egui::Color32::GREEN
                            }
                        };

                        egui::Grid::new("perf_grid").num_columns(2).spacing([10.0, 8.0]).show(ui, |ui| {
                            // CPU
                            ui.label("中央处理器 (CPU):");
                            let cpu_color = make_color(snapshot.global_cpu, 50.0, 80.0);
                            let cpu_text = egui::RichText::new(format!("{:.1}%", snapshot.global_cpu)).color(egui::Color32::WHITE).strong();
                            ui.add(egui::ProgressBar::new(snapshot.global_cpu / 100.0).text(cpu_text).fill(cpu_color));
                            ui.end_row();

                            // RAM
                            ui.label("物理内存 (RAM):");
                            let mem_pct = snapshot.used_memory as f32 / snapshot.total_memory as f32;
                            let mem_color = make_color(mem_pct * 100.0, 60.0, 85.0);
                            let mem_text = egui::RichText::new(format!(
                                "{:.1}GB / {:.1}GB",
                                snapshot.used_memory as f32 / 1024.0 / 1024.0 / 1024.0,
                                snapshot.total_memory as f32 / 1024.0 / 1024.0 / 1024.0
                            )).color(egui::Color32::WHITE).strong();
                            ui.add(egui::ProgressBar::new(mem_pct).text(mem_text).fill(mem_color));
                            ui.end_row();

                            // NET
                            ui.label("网络流量 (NET):");
                            let in_kb = snapshot.network_in as f32 / 1024.0;
                            let out_kb = snapshot.network_out as f32 / 1024.0;

                            let in_color = make_color(in_kb, 1024.0, 5120.0);
                            let out_color = make_color(out_kb, 1024.0, 5120.0);

                            ui.horizontal(|ui| {
                                ui.label("In:");
                                ui.label(egui::RichText::new(format!("{:.1} KB/s", in_kb)).color(in_color).strong());
                                ui.label("| Out:");
                                ui.label(egui::RichText::new(format!("{:.1} KB/s", out_kb)).color(out_color).strong());
                            });
                            ui.end_row();

                            // DISK
                            ui.label("磁盘存储 (DISK):");
                            if let Some(sys_disk) = snapshot.disks.iter().find(|d| d.mount_point.contains("C:")) {
                                let total_gb = sys_disk.total_space as f32 / 1024.0 / 1024.0 / 1024.0;
                                let free_gb = sys_disk.available_space as f32 / 1024.0 / 1024.0 / 1024.0;
                                ui.label(format!("{:.1}GB 可用 / {:.1}GB 总计", free_gb, total_gb));
                            } else {
                                ui.label("N/A");
                            }
                            ui.end_row();
                        });
                    });
                ui.add_space(10.0);
            }

            // Process Lists
            egui::ScrollArea::vertical().show(ui, |ui| {
                if !snapshot.high_resource.is_empty() {
                    ui.group(|ui| {
                        ui.label(
                            egui::RichText::new("🔥 极高负载任务")
                                .color(egui::Color32::RED)
                                .strong(),
                        );
                        // 限制高度，避免跳动，支持滚动
                        egui::ScrollArea::vertical()
                            .min_scrolled_height(300.0)
                            .max_height(300.0)
                            .show(ui, |ui| {
                                self.render_process_table(ui, ctx, &snapshot.high_resource, true);
                            });
                    });
                    ui.add_space(5.0);
                }

                if !snapshot.other_groups.is_empty() {
                    // 极简模式下默认折叠
                    let default_open = !snapshot.is_resource_tight;
                    
                    egui::CollapsingHeader::new(
                        egui::RichText::new(format!("👤 活动用户任务 ({})", snapshot.other_groups.len()))
                            .color(primary_color)
                            .strong(),
                    )
                    .default_open(default_open)
                    .show(ui, |ui| {
                        ui.add_space(5.0);
                        egui::ScrollArea::vertical()
                            .max_height(300.0)
                            .show(ui, |ui| {
                                self.render_process_table(ui, ctx, &snapshot.other_groups, false);
                            });
                    });
                    ui.add_space(5.0);
                }

                if !snapshot.system_groups.is_empty() {
                    egui::CollapsingHeader::new(
                        egui::RichText::new(format!("🛡️ 系统核心服务 ({})", snapshot.system_groups.len()))
                            .color(egui::Color32::from_rgb(139, 115, 85))
                            .strong(),
                    )
                    .default_open(false)
                    .show(ui, |ui| {
                        ui.add_space(5.0);
                        egui::ScrollArea::vertical()
                            .max_height(200.0)
                            .show(ui, |ui| {
                                self.render_process_table(ui, ctx, &snapshot.system_groups, false);
                            });
                    });
                }
            });
            ui.add_space(20.0);
        });
    }
}

fn main() -> eframe::Result<()> {
    let icon_data = include_bytes!("../../进程图标.png");
    let icon = image::load_from_memory(icon_data).ok().map(|img| {
        let rgba = img.to_rgba8();
        let (w, h) = rgba.dimensions();
        egui::IconData {
            rgba: rgba.into_raw(),
            width: w,
            height: h,
        }
    });

    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([650.0, 850.0])
            .with_min_inner_size([600.0, 500.0])
            .with_icon(icon.unwrap_or_default()),
        ..Default::default()
    };

    eframe::run_native(
        "Geek Killer Pro",
        native_options,
        Box::new(|cc| Ok(Box::new(GeekKillerApp::new(cc)))),
    )
}
