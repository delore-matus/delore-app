use std::time::Duration;

use crate::resource::{IdleState, PowerState};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SystemSample {
    pub idle: IdleState,
    pub power: PowerState,
    pub diagnostic: Option<String>,
}

pub trait SystemProbe: Send + Sync {
    fn sample(&self) -> SystemSample;
}

#[derive(Debug, Default)]
pub struct NativeSystemProbe;

impl SystemProbe for NativeSystemProbe {
    fn sample(&self) -> SystemSample {
        native_sample()
    }
}

#[cfg(target_os = "windows")]
fn native_sample() -> SystemSample {
    use std::mem::size_of;

    use windows_sys::Win32::{
        System::{
            Power::{GetSystemPowerStatus, SYSTEM_POWER_STATUS},
            SystemInformation::GetTickCount,
        },
        UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO},
    };

    let idle = unsafe {
        let mut last_input = LASTINPUTINFO {
            cbSize: size_of::<LASTINPUTINFO>() as u32,
            dwTime: 0,
        };
        if GetLastInputInfo(&mut last_input) == 0 {
            IdleState::Unknown
        } else {
            let elapsed_ms = GetTickCount().wrapping_sub(last_input.dwTime);
            IdleState::IdleFor(Duration::from_millis(u64::from(elapsed_ms)))
        }
    };

    let power = unsafe {
        let mut status = SYSTEM_POWER_STATUS {
            ACLineStatus: 255,
            BatteryFlag: 255,
            BatteryLifePercent: 255,
            SystemStatusFlag: 0,
            BatteryLifeTime: 0,
            BatteryFullLifeTime: 0,
        };
        if GetSystemPowerStatus(&mut status) == 0 {
            PowerState::Unknown {
                battery_present: true,
            }
        } else if status.BatteryFlag == 128 {
            PowerState::Ac
        } else {
            match status.ACLineStatus {
                1 => PowerState::Ac,
                0 => PowerState::OnBattery,
                _ => PowerState::Unknown {
                    battery_present: true,
                },
            }
        }
    };

    SystemSample {
        idle,
        power,
        diagnostic: None,
    }
}

#[cfg(target_os = "linux")]
fn native_sample() -> SystemSample {
    let idle = linux_idle_state().unwrap_or(IdleState::Unknown);
    let upower = linux_power_state();
    let (power, diagnostic) = match upower {
        Ok(power) => (power, None),
        Err(upower_error) => match linux_sysfs_power_state() {
            Ok(power) => (
                power,
                Some(format!("UPower unavailable; used sysfs: {upower_error}")),
            ),
            Err(sysfs_error) => (
                PowerState::Unknown {
                    battery_present: true,
                },
                Some(format!(
                    "UPower and sysfs power state unavailable: {upower_error}; {sysfs_error}"
                )),
            ),
        },
    };
    SystemSample {
        idle,
        power,
        diagnostic,
    }
}

#[cfg(target_os = "linux")]
fn linux_idle_state() -> anyhow::Result<IdleState> {
    use zbus::{blocking::Connection, zvariant::OwnedObjectPath};

    let connection = Connection::system()?;
    let manager = zbus::blocking::Proxy::new(
        &connection,
        "org.freedesktop.login1",
        "/org/freedesktop/login1",
        "org.freedesktop.login1.Manager",
    )?;
    let session_path: OwnedObjectPath = manager.call("GetSessionByPID", &(std::process::id(),))?;
    let session = zbus::blocking::Proxy::new(
        &connection,
        "org.freedesktop.login1",
        session_path.as_str(),
        "org.freedesktop.login1.Session",
    )?;
    let idle_hint: bool = session.get_property("IdleHint")?;
    if !idle_hint {
        return Ok(IdleState::Active);
    }
    let since_monotonic: u64 = session.get_property("IdleSinceHintMonotonic")?;
    if since_monotonic == 0 {
        return Ok(IdleState::Unknown);
    }
    let uptime = std::fs::read_to_string("/proc/uptime")?;
    let uptime_seconds = uptime
        .split_whitespace()
        .next()
        .and_then(|seconds| seconds.parse::<f64>().ok())
        .ok_or_else(|| anyhow::anyhow!("could not parse /proc/uptime"))?;
    let idle_seconds = uptime_seconds - (since_monotonic as f64 / 1_000_000.0);
    if idle_seconds.is_sign_negative() {
        return Ok(IdleState::Unknown);
    }
    Ok(IdleState::IdleFor(Duration::from_secs_f64(idle_seconds)))
}

#[cfg(target_os = "linux")]
fn linux_power_state() -> anyhow::Result<PowerState> {
    use zbus::blocking::{Connection, Proxy};

    let connection = Connection::system()?;
    let upower = Proxy::new(
        &connection,
        "org.freedesktop.UPower",
        "/org/freedesktop/UPower",
        "org.freedesktop.UPower",
    )?;
    let has_battery: bool = upower.get_property("HasBattery")?;
    if !has_battery {
        return Ok(PowerState::Ac);
    }
    let on_battery: bool = upower.get_property("OnBattery")?;
    Ok(if on_battery {
        PowerState::OnBattery
    } else {
        PowerState::Ac
    })
}

#[cfg(target_os = "linux")]
fn linux_sysfs_power_state() -> anyhow::Result<PowerState> {
    let supplies = std::fs::read_dir("/sys/class/power_supply")?;
    let mut saw_battery = false;
    for supply in supplies {
        let supply = supply?;
        let path = supply.path();
        let supply_type = std::fs::read_to_string(path.join("type"))?;
        if supply_type.trim() != "Battery" {
            continue;
        }
        saw_battery = true;
        if let Ok(status) = std::fs::read_to_string(path.join("status")) {
            match status.trim() {
                "Discharging" => return Ok(PowerState::OnBattery),
                "Charging" | "Full" | "Not charging" => return Ok(PowerState::Ac),
                _ => {}
            }
        }
    }
    if saw_battery {
        Ok(PowerState::Unknown {
            battery_present: true,
        })
    } else {
        Ok(PowerState::Ac)
    }
}

#[cfg(target_os = "macos")]
fn native_sample() -> SystemSample {
    let (power, diagnostic) = match macos_power_state() {
        Ok(power) => (
            power,
            Some("UNVERIFIED: IOPowerSources has not been run on macOS.".into()),
        ),
        Err(error) => (
            PowerState::Unknown {
                battery_present: true,
            },
            Some(format!(
                "UNVERIFIED: IOPowerSources power detection failed: {error}"
            )),
        ),
    };
    SystemSample {
        idle: IdleState::Unknown,
        power,
        diagnostic: Some(format!(
            "UNVERIFIED: CGEventSource idle detection could not be confirmed from Apple documentation; \
             Only when idle remains paused. Choose Always on (low priority) to proceed. {}",
            diagnostic.unwrap_or_default()
        )),
    }
}

#[cfg(target_os = "macos")]
fn macos_power_state() -> anyhow::Result<PowerState> {
    use std::{ffi::CString, ptr};

    type CfRef = *const std::ffi::c_void;
    const UTF8_ENCODING: u32 = 0x0800_0100;

    #[link(name = "IOKit", kind = "framework")]
    unsafe extern "C" {
        fn IOPSCopyPowerSourcesInfo() -> CfRef;
        fn IOPSCopyPowerSourcesList(info: CfRef) -> CfRef;
        fn IOPSGetPowerSourceDescription(info: CfRef, source: CfRef) -> CfRef;
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFArrayGetCount(array: CfRef) -> isize;
        fn CFArrayGetValueAtIndex(array: CfRef, index: isize) -> CfRef;
        fn CFDictionaryGetValue(dictionary: CfRef, key: CfRef) -> CfRef;
        fn CFStringCreateWithCString(allocator: CfRef, value: *const i8, encoding: u32) -> CfRef;
        fn CFStringGetCString(
            value: CfRef,
            buffer: *mut i8,
            buffer_size: isize,
            encoding: u32,
        ) -> u8;
        fn CFRelease(value: CfRef);
    }

    unsafe fn cf_string(value: &str) -> anyhow::Result<CfRef> {
        let value = CString::new(value)?;
        let string = CFStringCreateWithCString(ptr::null(), value.as_ptr(), UTF8_ENCODING);
        if string.is_null() {
            anyhow::bail!("could not allocate a CoreFoundation string");
        }
        Ok(string)
    }

    unsafe fn dictionary_string(dictionary: CfRef, key: CfRef) -> anyhow::Result<Option<String>> {
        let value = CFDictionaryGetValue(dictionary, key);
        if value.is_null() {
            return Ok(None);
        }
        let mut buffer = [0_i8; 128];
        if CFStringGetCString(
            value,
            buffer.as_mut_ptr(),
            buffer.len() as isize,
            UTF8_ENCODING,
        ) == 0
        {
            return Ok(None);
        }
        let bytes = buffer
            .iter()
            .take_while(|byte| **byte != 0)
            .map(|byte| *byte as u8)
            .collect::<Vec<_>>();
        Ok(Some(String::from_utf8(bytes)?))
    }

    unsafe {
        let info = IOPSCopyPowerSourcesInfo();
        if info.is_null() {
            anyhow::bail!("IOPSCopyPowerSourcesInfo returned null");
        }
        let sources = IOPSCopyPowerSourcesList(info);
        if sources.is_null() {
            CFRelease(info);
            anyhow::bail!("IOPSCopyPowerSourcesList returned null");
        }
        let state_key = cf_string("Power Source State")?;
        let type_key = cf_string("Type")?;
        let battery_value = "Battery Power";
        let ac_value = "AC Power";
        let mut found_battery = false;
        let mut state = None;
        for index in 0..CFArrayGetCount(sources) {
            let source = CFArrayGetValueAtIndex(sources, index);
            let description = IOPSGetPowerSourceDescription(info, source);
            if description.is_null() {
                continue;
            }
            if dictionary_string(description, type_key)?.as_deref() == Some("InternalBattery") {
                found_battery = true;
                state = dictionary_string(description, state_key)?;
                break;
            }
        }
        CFRelease(state_key);
        CFRelease(type_key);
        CFRelease(sources);
        CFRelease(info);
        if !found_battery {
            return Ok(PowerState::Ac);
        }
        match state.as_deref() {
            Some(value) if value == battery_value => Ok(PowerState::OnBattery),
            Some(value) if value == ac_value => Ok(PowerState::Ac),
            _ => Ok(PowerState::Unknown {
                battery_present: true,
            }),
        }
    }
}

#[cfg(not(any(target_os = "windows", target_os = "linux", target_os = "macos")))]
fn native_sample() -> SystemSample {
    SystemSample {
        idle: IdleState::Unknown,
        power: PowerState::Unknown {
            battery_present: true,
        },
        diagnostic: Some("Idle and battery detection are unsupported on this platform.".into()),
    }
}
