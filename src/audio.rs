//! Thin WASAPI layer: enumerates render devices and per-application audio sessions,
//! reads session peak meters and session volumes. All objects live on the engine thread (MTA).

use std::collections::HashMap;
use std::time::{Duration, Instant};

use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Foundation::CloseHandle;
use windows::Win32::Media::Audio::Endpoints::IAudioMeterInformation;
use windows::Win32::Media::Audio::{
    AudioSessionStateExpired, DEVICE_STATE_ACTIVE, IAudioSessionControl, IAudioSessionControl2,
    IAudioSessionManager2, IMMDevice, IMMDeviceEnumerator, ISimpleAudioVolume, MMDeviceEnumerator,
    eRender,
};
use windows::Win32::System::Com::StructuredStorage::{PropVariantClear, PropVariantToStringAlloc};
use windows::Win32::System::Com::{
    CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree, STGM_READ,
};
use windows::Win32::System::Threading::{
    OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};
use windows::core::{GUID, Interface, PWSTR};

use crate::config::normalize_exe;

/// Marks volume changes made by this program, so they are distinguishable from user changes.
pub const EVENT_CONTEXT: GUID = GUID::from_u128(0x6c6f7564_636f_6e74_726f_6c0000000001);

const DEVICE_SCAN_INTERVAL: Duration = Duration::from_secs(3);
const SESSION_SCAN_INTERVAL: Duration = Duration::from_millis(800);

pub fn init_com() -> windows::core::Result<()> {
    unsafe { CoInitializeEx(None, COINIT_MULTITHREADED).ok() }
}

/// Takes ownership of a COM-allocated string.
fn take_pwstr(p: PWSTR) -> String {
    if p.is_null() {
        return String::new();
    }
    unsafe {
        let s = p.to_string().unwrap_or_default();
        CoTaskMemFree(Some(p.0 as *const _));
        s
    }
}

pub struct Session {
    /// Unique per running session (includes the process id).
    pub instance_id: String,
    /// Stable across app restarts; Windows keys remembered per-app volume by it.
    pub session_id: String,
    pub pid: u32,
    /// Executable file name, e.g. `Discord.exe`. Empty for system sounds.
    pub exe: String,
    pub exe_norm: String,
    pub is_system: bool,
    pub device_id: String,
    pub device_name: String,
    meter: IAudioMeterInformation,
    volume: ISimpleAudioVolume,
}

impl Session {
    pub fn label(&self) -> String {
        if self.is_system {
            "System sounds".to_owned()
        } else if !self.exe.is_empty() {
            self.exe.clone()
        } else {
            format!("pid {}", self.pid)
        }
    }

    /// Current peak sample (0..1) of the session's output.
    pub fn peak(&self) -> f32 {
        unsafe { self.meter.GetPeakValue().unwrap_or(0.0) }
    }

    pub fn volume(&self) -> windows::core::Result<f32> {
        unsafe { self.volume.GetMasterVolume() }
    }

    pub fn set_volume(&self, level: f32) -> windows::core::Result<()> {
        unsafe { self.volume.SetMasterVolume(level.clamp(0.0, 1.0), &EVENT_CONTEXT) }
    }
}

struct Device {
    name: String,
    manager: IAudioSessionManager2,
}

pub struct AudioSystem {
    enumerator: IMMDeviceEnumerator,
    devices: HashMap<String, Device>,
    pub sessions: HashMap<String, Session>,
    exe_cache: HashMap<u32, String>,
    last_device_scan: Option<Instant>,
    last_session_scan: Option<Instant>,
}

/// What changed during a refresh.
#[derive(Default)]
pub struct RefreshResult {
    pub added: Vec<String>,
    pub removed: Vec<String>,
}

impl AudioSystem {
    pub fn new() -> windows::core::Result<Self> {
        let enumerator: IMMDeviceEnumerator =
            unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)? };
        Ok(Self {
            enumerator,
            devices: HashMap::new(),
            sessions: HashMap::new(),
            exe_cache: HashMap::new(),
            last_device_scan: None,
            last_session_scan: None,
        })
    }

    /// Rescans devices and sessions when their intervals have elapsed (or when `force`).
    pub fn refresh(&mut self, force: bool) -> RefreshResult {
        let now = Instant::now();
        let due = |last: Option<Instant>, every| last.is_none_or(|t| now.duration_since(t) >= every);
        let mut devices_changed = false;
        if force || due(self.last_device_scan, DEVICE_SCAN_INTERVAL) {
            self.last_device_scan = Some(now);
            devices_changed = self.scan_devices();
        }
        if force || devices_changed || due(self.last_session_scan, SESSION_SCAN_INTERVAL) {
            self.last_session_scan = Some(now);
            return self.scan_sessions();
        }
        RefreshResult::default()
    }

    fn scan_devices(&mut self) -> bool {
        let found = match self.list_device_ids() {
            Ok(v) => v,
            Err(e) => {
                log::warn!("audio device enumeration failed: {e}");
                return false;
            }
        };
        let mut changed = false;
        self.devices.retain(|id, d| {
            let keep = found.iter().any(|(fid, _)| fid == id);
            if !keep {
                log::info!("audio device removed: {}", d.name);
                changed = true;
            }
            keep
        });
        for (id, device) in found {
            if self.devices.contains_key(&id) {
                continue;
            }
            let name = device_name(&device).unwrap_or_else(|| "Unknown device".to_owned());
            match unsafe { device.Activate::<IAudioSessionManager2>(CLSCTX_ALL, None) } {
                Ok(manager) => {
                    log::info!("audio device added: {name}");
                    self.devices.insert(id, Device { name, manager });
                    changed = true;
                }
                Err(e) => log::warn!("cannot open session manager for {name}: {e}"),
            }
        }
        changed
    }

    fn list_device_ids(&self) -> windows::core::Result<Vec<(String, IMMDevice)>> {
        unsafe {
            let collection = self.enumerator.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE)?;
            let mut out = Vec::new();
            for i in 0..collection.GetCount()? {
                let device = collection.Item(i)?;
                let id = take_pwstr(device.GetId()?);
                out.push((id, device));
            }
            Ok(out)
        }
    }

    fn scan_sessions(&mut self) -> RefreshResult {
        let mut seen: Vec<String> = Vec::with_capacity(self.sessions.len());
        let mut result = RefreshResult::default();
        let mut fresh: Vec<Session> = Vec::new();
        // Devices whose enumeration failed this time; their known sessions are kept as they are,
        // so a transient error does not look like every app on the device going away.
        let mut failed: Vec<&str> = Vec::new();

        for (device_id, device) in &self.devices {
            let list = match unsafe { device.manager.GetSessionEnumerator() } {
                Ok(l) => l,
                Err(e) => {
                    log::debug!("session enumeration failed on {}: {e}", device.name);
                    failed.push(device_id.as_str());
                    continue;
                }
            };
            let count = unsafe { list.GetCount() }.unwrap_or(0);
            for i in 0..count {
                let Ok(control) = (unsafe { list.GetSession(i) }) else { continue };
                if unsafe { control.GetState() }.is_ok_and(|s| s == AudioSessionStateExpired) {
                    continue;
                }
                let Ok(control2) = control.cast::<IAudioSessionControl2>() else { continue };
                let instance_id = unsafe { control2.GetSessionInstanceIdentifier() }
                    .map(take_pwstr)
                    .unwrap_or_default();
                if instance_id.is_empty() {
                    continue;
                }
                seen.push(instance_id.clone());
                if self.sessions.contains_key(&instance_id) || fresh.iter().any(|s| s.instance_id == instance_id) {
                    continue;
                }
                if let Some(s) = open_session(&control, &control2, instance_id, device_id, &device.name, &mut self.exe_cache) {
                    fresh.push(s);
                }
            }
        }

        self.sessions.retain(|id, s| {
            let keep = seen.contains(id) || failed.contains(&s.device_id.as_str());
            if !keep {
                log::debug!("session gone: {} on {}", s.label(), s.device_name);
                result.removed.push(id.clone());
            }
            keep
        });
        for s in fresh {
            log::debug!("session found: {} (pid {}) on {}", s.label(), s.pid, s.device_name);
            result.added.push(s.instance_id.clone());
            self.sessions.insert(s.instance_id.clone(), s);
        }
        // Forget cached names of processes that no longer own any session.
        let live: Vec<u32> = self.sessions.values().map(|s| s.pid).collect();
        self.exe_cache.retain(|pid, _| live.contains(pid));
        result
    }
}

fn open_session(
    control: &IAudioSessionControl,
    control2: &IAudioSessionControl2,
    instance_id: String,
    device_id: &str,
    device_name: &str,
    exe_cache: &mut HashMap<u32, String>,
) -> Option<Session> {
    let meter = control.cast::<IAudioMeterInformation>().ok()?;
    let volume = control.cast::<ISimpleAudioVolume>().ok()?;
    let pid = unsafe { control2.GetProcessId() }.unwrap_or(0);
    // S_OK means system sounds; S_FALSE means a regular session.
    let is_system = unsafe { control2.IsSystemSoundsSession() }.0 == 0;
    let session_id = unsafe { control2.GetSessionIdentifier() }.map(take_pwstr).unwrap_or_default();
    let exe = if is_system || pid == 0 {
        String::new()
    } else {
        exe_cache.entry(pid).or_insert_with(|| process_exe(pid).unwrap_or_default()).clone()
    };
    Some(Session {
        instance_id,
        session_id,
        pid,
        exe_norm: normalize_exe(&exe),
        exe,
        is_system,
        device_id: device_id.to_owned(),
        device_name: device_name.to_owned(),
        meter,
        volume,
    })
}

fn device_name(device: &IMMDevice) -> Option<String> {
    unsafe {
        let store = device.OpenPropertyStore(STGM_READ).ok()?;
        let mut value = store.GetValue(&PKEY_Device_FriendlyName).ok()?;
        let name = PropVariantToStringAlloc(&value).ok().map(take_pwstr);
        let _ = PropVariantClear(&mut value);
        name
    }
}

/// Executable file name of a process, e.g. `Discord.exe`.
pub fn process_exe(pid: u32) -> Option<String> {
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buf = [0u16; 1024];
        let mut len = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(handle, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len);
        let _ = CloseHandle(handle);
        ok.ok()?;
        let path = String::from_utf16_lossy(&buf[..len as usize]);
        path.rsplit(['\\', '/']).next().map(str::to_owned)
    }
}
