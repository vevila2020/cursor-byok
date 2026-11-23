//! Controls the Cursor desktop process around an explicit takeover.

use tokio::process::Command;

use crate::{Error, Result};

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

#[cfg(windows)]
fn hide_console(command: &mut Command) {
    command.creation_flags(CREATE_NO_WINDOW);
}

pub async fn terminate_cursor() -> Result<()> {
    terminate_platform_cursor().await
}

pub async fn launch_cursor() -> Result<()> {
    launch_platform_cursor().await
}

#[cfg(target_os = "macos")]
async fn launch_platform_cursor() -> Result<()> {
    let launched = Command::new("open").args(["-a", "Cursor"]).status().await?;
    if launched.success() {
        Ok(())
    } else {
        Err(Error::Config(
            "failed to launch Cursor; make sure it is installed".into(),
        ))
    }
}

#[cfg(target_os = "windows")]
async fn launch_platform_cursor() -> Result<()> {
    let executable = windows_cursor_executable().ok_or_else(|| {
        Error::Config("failed to launch Cursor; make sure it is installed".into())
    })?;
    let mut command = Command::new(executable);
    hide_console(&mut command);
    command.spawn()?;
    Ok(())
}

#[cfg(target_os = "windows")]
fn windows_cursor_executable() -> Option<std::path::PathBuf> {
    use std::path::PathBuf;

    if let Some(executable) = registered_cursor_executable() {
        return Some(executable);
    }

    let mut candidates = Vec::new();
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        candidates.push(
            PathBuf::from(&local)
                .join("Programs")
                .join("cursor")
                .join("Cursor.exe"),
        );
        candidates.push(
            PathBuf::from(local)
                .join("Programs")
                .join("Cursor")
                .join("Cursor.exe"),
        );
    }
    for variable in ["ProgramFiles", "ProgramFiles(x86)"] {
        if let Some(programs) = std::env::var_os(variable) {
            candidates.push(PathBuf::from(programs).join("Cursor").join("Cursor.exe"));
        }
    }
    candidates.into_iter().find(|path| path.is_file())
}

/// Reads the executable from the registered `cursor://` protocol handler.
/// The installer registers the scheme for deep links, which also covers
/// installations outside the default directories.
#[cfg(target_os = "windows")]
fn registered_cursor_executable() -> Option<std::path::PathBuf> {
    use windows_sys::Win32::System::Registry::{HKEY_CLASSES_ROOT, HKEY_CURRENT_USER};

    [
        (
            HKEY_CURRENT_USER,
            r"Software\Classes\cursor\shell\open\command",
        ),
        (HKEY_CLASSES_ROOT, r"cursor\shell\open\command"),
    ]
    .into_iter()
    .find_map(|(root, subkey)| {
        let command = read_registry_string(root, subkey)?;
        let executable = command_executable(&command)?;
        executable.is_file().then_some(executable)
    })
}

#[cfg(target_os = "windows")]
fn read_registry_string(
    root: windows_sys::Win32::System::Registry::HKEY,
    subkey: &str,
) -> Option<String> {
    use windows_sys::Win32::System::Registry::{
        RegCloseKey, RegOpenKeyExW, RegQueryValueExW, KEY_READ, REG_SZ,
    };

    let subkey: Vec<u16> = subkey.encode_utf16().chain([0]).collect();
    let mut key = std::ptr::null_mut();
    if unsafe { RegOpenKeyExW(root, subkey.as_ptr(), 0, KEY_READ, &mut key) } != 0 {
        return None;
    }
    let mut value_type = 0;
    let mut size = 0;
    let mut status = unsafe {
        RegQueryValueExW(
            key,
            std::ptr::null(),
            std::ptr::null(),
            &mut value_type,
            std::ptr::null_mut(),
            &mut size,
        )
    };
    let mut buffer = Vec::new();
    if status == 0 && value_type == REG_SZ && size >= 2 {
        buffer.resize(size as usize, 0);
        status = unsafe {
            RegQueryValueExW(
                key,
                std::ptr::null(),
                std::ptr::null(),
                &mut value_type,
                buffer.as_mut_ptr(),
                &mut size,
            )
        };
    }
    unsafe { RegCloseKey(key) };
    if status != 0 || value_type != REG_SZ {
        return None;
    }
    buffer.truncate(size as usize);
    let value: Vec<u16> = buffer
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| u16::from_le_bytes(*pair))
        .collect();
    Some(
        String::from_utf16_lossy(&value)
            .trim_end_matches('\0')
            .to_owned(),
    )
}

#[cfg(target_os = "windows")]
fn command_executable(command: &str) -> Option<std::path::PathBuf> {
    let command = command.trim();
    let executable = match command.strip_prefix('"') {
        Some(rest) => &rest[..rest.find('"')?],
        None => command.split_whitespace().next()?,
    };
    (!executable.is_empty()).then(|| std::path::PathBuf::from(executable))
}

#[cfg(target_os = "linux")]
async fn launch_platform_cursor() -> Result<()> {
    match Command::new("cursor").spawn() {
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Err(Error::Config(
            "failed to launch Cursor; make sure it is installed".into(),
        )),
        Err(error) => Err(error.into()),
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
async fn launch_platform_cursor() -> Result<()> {
    Err(Error::Config(format!(
        "launching Cursor is unsupported on {}",
        std::env::consts::OS
    )))
}

#[cfg(target_os = "macos")]
async fn terminate_platform_cursor() -> Result<()> {
    terminate_unix_process("Cursor").await
}

#[cfg(target_os = "linux")]
async fn terminate_platform_cursor() -> Result<()> {
    terminate_unix_process("cursor").await?;
    terminate_unix_process("Cursor").await
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
async fn terminate_unix_process(name: &str) -> Result<()> {
    let running = Command::new("pgrep").args(["-x", name]).status().await?;
    if !running.success() {
        return match running.code() {
            Some(1) => Ok(()),
            _ => Err(Error::Config(format!(
                "failed to inspect the {name} process"
            ))),
        };
    }
    let terminated = Command::new("pkill").args(["-x", name]).status().await?;
    if terminated.success() || terminated.code() == Some(1) {
        Ok(())
    } else {
        Err(Error::Config(format!(
            "failed to terminate the {name} process"
        )))
    }
}

#[cfg(target_os = "windows")]
async fn terminate_platform_cursor() -> Result<()> {
    let mut list = Command::new("tasklist");
    hide_console(&mut list);
    let processes = list
        .args(["/FI", "IMAGENAME eq Cursor.exe", "/NH", "/FO", "CSV"])
        .output()
        .await?;
    if !processes.status.success() {
        return Err(Error::Config(
            "failed to inspect the Cursor.exe process".into(),
        ));
    }
    if !String::from_utf8_lossy(&processes.stdout)
        .to_ascii_lowercase()
        .contains("cursor.exe")
    {
        return Ok(());
    }
    let mut kill = Command::new("taskkill");
    hide_console(&mut kill);
    let terminated = kill
        .args(["/F", "/T", "/IM", "Cursor.exe"])
        .status()
        .await?;
    if terminated.success() {
        Ok(())
    } else {
        Err(Error::Config(
            "failed to terminate the Cursor.exe process".into(),
        ))
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
async fn terminate_platform_cursor() -> Result<()> {
    Err(Error::Config(format!(
        "terminating Cursor is unsupported on {}",
        std::env::consts::OS
    )))
}

#[cfg(all(test, target_os = "windows"))]
mod tests {
    use std::path::PathBuf;

    use super::command_executable;

    #[test]
    fn extracts_the_protocol_handler_executable() {
        assert_eq!(
            command_executable(r#""D:\WORK\software\cursor\Cursor.exe" --open-url -- "%1""#),
            Some(PathBuf::from(r"D:\WORK\software\cursor\Cursor.exe"))
        );
        assert_eq!(
            command_executable("cursor.exe --flag"),
            Some(PathBuf::from("cursor.exe"))
        );
        assert_eq!(command_executable("   "), None);
    }
}
