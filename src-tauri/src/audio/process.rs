#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProcessRecord {
    pub pid: u32,
    pub parent: u32,
    pub created: u64,
    pub image: String,
    pub aumid: Option<String>,
}
fn local_path(path: &str) -> bool {
    let b = path.as_bytes();
    b.len() > 3
        && b[0].is_ascii_alphabetic()
        && b[1] == b':'
        && [b'/', b'\\'].contains(&b[2])
        && !path.chars().any(char::is_control)
}
fn path_key(path: &str) -> String {
    path.replace('/', "\\").to_lowercase()
}
pub fn select_process(
    source: &str,
    registered: Option<&str>,
    records: &[ProcessRecord],
) -> Option<ProcessRecord> {
    if source.trim() != source
        || source.is_empty()
        || source.contains(['/', '\\', ':'])
        || source.chars().any(char::is_control)
    {
        return None;
    }
    if registered.is_some_and(|p| !local_path(p)) {
        return None;
    }
    let exact: Vec<_> = records
        .iter()
        .filter(|r| r.aumid.as_deref() == Some(source))
        .collect();
    let candidates: Vec<&ProcessRecord> = if !exact.is_empty() {
        exact
    } else if let Some(path) = registered {
        records
            .iter()
            .filter(|r| path_key(&r.image) == path_key(path))
            .collect()
    } else if source.to_ascii_lowercase().ends_with(".exe") {
        records
            .iter()
            .filter(|r| {
                r.image
                    .rsplit(['/', '\\'])
                    .next()
                    .is_some_and(|leaf| leaf.eq_ignore_ascii_case(source))
            })
            .collect()
    } else {
        return None;
    };
    let first = *candidates.first()?;
    if !local_path(&first.image)
        || candidates
            .iter()
            .any(|r| path_key(&r.image) != path_key(&first.image))
    {
        return None;
    }
    // Walk only the same verified executable; never include the launching shell.
    let same: Vec<_> = records
        .iter()
        .filter(|r| path_key(&r.image) == path_key(&first.image))
        .collect();
    let mut root_pid = None;
    for candidate in &same {
        let mut current = *candidate;
        let mut seen = std::collections::HashSet::new();
        loop {
            if !seen.insert(current.pid) {
                return None;
            }
            let Some(parent) = same.iter().find(|p| p.pid == current.parent) else {
                break;
            };
            if parent.created >= current.created {
                return None;
            }
            current = parent;
        }
        if root_pid.is_some_and(|old| old != current.pid) {
            return None;
        }
        root_pid = Some(current.pid);
    }
    records.iter().find(|r| Some(r.pid) == root_pid).cloned()
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn exact_aumid_still_rejects_an_independent_root_of_the_same_image() {
        let records = vec![
            p(10, 1, 10, "C:/Apps/Player.exe", Some("vendor.player")),
            p(20, 1, 20, "C:/Apps/Player.exe", None),
        ];
        assert!(select_process("vendor.player", None, &records).is_none());
    }
    fn p(pid: u32, parent: u32, created: u64, image: &str, aumid: Option<&str>) -> ProcessRecord {
        ProcessRecord {
            pid,
            parent,
            created,
            image: image.into(),
            aumid: aumid.map(str::to_owned),
        }
    }
    #[test]
    fn same_image_tree_selects_app_root_not_shell_parent() {
        let rs = vec![
            p(1, 0, 1, "C:/Windows/explorer.exe", None),
            p(10, 1, 10, "C:/Apps/Player.exe", None),
            p(11, 10, 11, "C:/Apps/Player.exe", None),
        ];
        assert_eq!(
            select_process("vendor.player", Some("C:/Apps/Player.exe"), &rs).map(|p| p.pid),
            Some(10)
        );
    }
    #[test]
    fn independent_roots_and_different_paths_are_ambiguous() {
        let rs = vec![
            p(10, 1, 10, "C:/Apps/Player.exe", None),
            p(20, 1, 20, "C:/Apps/Player.exe", None),
        ];
        assert!(select_process("Player.exe", None, &rs).is_none());
        let rs = vec![
            p(10, 1, 10, "C:/Apps/Player.exe", None),
            p(20, 10, 20, "D:/Other/Player.exe", None),
        ];
        assert!(select_process("Player.exe", None, &rs).is_none());
    }
    #[test]
    fn exact_aumid_is_preferred_and_substrings_are_rejected() {
        let rs = vec![
            p(10, 1, 10, "C:/Apps/Player.exe", Some("vendor.player")),
            p(
                20,
                1,
                20,
                "D:/Other/Player.exe",
                Some("vendor.player.extra"),
            ),
        ];
        assert_eq!(
            select_process("vendor.player", None, &rs).map(|p| p.pid),
            Some(10)
        );
        assert!(select_process("vendor", None, &rs).is_none());
    }
    #[test]
    fn reused_parent_and_cycles_are_rejected() {
        let reused = vec![
            p(10, 11, 10, "C:/Apps/Player.exe", None),
            p(11, 1, 20, "C:/Apps/Player.exe", None),
        ];
        assert!(select_process("Player.exe", None, &reused).is_none());
        let cycle = vec![
            p(10, 11, 10, "C:/Apps/Player.exe", None),
            p(11, 10, 10, "C:/Apps/Player.exe", None),
        ];
        assert!(select_process("Player.exe", None, &cycle).is_none());
    }
    #[test]
    fn invalid_source_never_matches() {
        let rs = vec![p(10, 1, 10, "C:/Apps/Player.exe", Some("vendor.player"))];
        for id in ["", " Player.exe", "../Player.exe"] {
            assert!(select_process(id, Some("C:/Apps/Player.exe"), &rs).is_none());
        }
        assert!(select_process("other", Some("//server/Player.exe"), &rs).is_none());
    }
}

#[cfg(windows)]
pub mod native {
    use super::*;
    use windows::{
        core::{GUID, HSTRING, PWSTR},
        Win32::{
            Foundation::{CloseHandle, FILETIME, HANDLE, PROPERTYKEY, WAIT_TIMEOUT},
            Storage::Packaging::Appx::GetApplicationUserModelId,
            System::{
                Com::CoTaskMemFree,
                Diagnostics::ToolHelp::{
                    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
                    TH32CS_SNAPPROCESS,
                },
                Threading::{
                    GetProcessTimes, OpenProcess, QueryFullProcessImageNameW, WaitForSingleObject,
                    PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
                },
            },
            UI::Shell::{IShellItem2, SHCreateItemFromParsingName},
        },
    };
    struct Handle(HANDLE);
    impl Drop for Handle {
        fn drop(&mut self) {
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }
    pub struct ProcessIdentity {
        handle: Handle,
        pub record: ProcessRecord,
    }
    fn created(handle: HANDLE) -> Option<u64> {
        let (mut start, mut exit, mut kernel, mut user) = (
            FILETIME::default(),
            FILETIME::default(),
            FILETIME::default(),
            FILETIME::default(),
        );
        unsafe { GetProcessTimes(handle, &mut start, &mut exit, &mut kernel, &mut user).ok()? };
        Some((start.dwHighDateTime as u64) << 32 | start.dwLowDateTime as u64)
    }
    fn image(handle: HANDLE) -> Option<String> {
        let mut b = vec![0u16; 32768];
        let mut n = b.len() as u32;
        unsafe {
            QueryFullProcessImageNameW(handle, PROCESS_NAME_WIN32, PWSTR(b.as_mut_ptr()), &mut n)
                .ok()?
        };
        Some(String::from_utf16_lossy(&b[..n as usize]))
    }
    fn aumid(handle: HANDLE) -> Option<String> {
        let mut n = 0;
        let _ = unsafe { GetApplicationUserModelId(handle, &mut n, None) };
        if n == 0 || n > 32768 {
            return None;
        }
        let mut b = vec![0u16; n as usize];
        if unsafe { GetApplicationUserModelId(handle, &mut n, Some(PWSTR(b.as_mut_ptr()))) }.0 != 0
        {
            return None;
        }
        let end = b.iter().position(|c| *c == 0).unwrap_or(b.len());
        Some(String::from_utf16_lossy(&b[..end]))
    }
    impl ProcessIdentity {
        pub fn open(pid: u32) -> Result<Self, String> {
            let handle = Handle(
                unsafe {
                    OpenProcess(
                        PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
                        false,
                        pid,
                    )
                }
                .map_err(|e| e.to_string())?,
            );
            let record = ProcessRecord {
                pid,
                parent: 0,
                created: created(handle.0).ok_or("process creation time unavailable")?,
                image: image(handle.0).ok_or("process image unavailable")?,
                aumid: aumid(handle.0),
            };
            let identity = Self { handle, record };
            if !identity.is_alive() {
                return Err("process exited".into());
            }
            Ok(identity)
        }
        pub fn is_alive(&self) -> bool {
            (unsafe { WaitForSingleObject(self.handle.0, 0) }) == WAIT_TIMEOUT
                && created(self.handle.0) == Some(self.record.created)
        }
    }
    fn registered_path(source: &str) -> Option<String> {
        if source.is_empty()
            || source.len() > 1024
            || source.contains(['\\', '/', ':'])
            || source.chars().any(char::is_control)
        {
            return None;
        }
        let item: IShellItem2 = unsafe {
            SHCreateItemFromParsingName(&HSTRING::from(format!("shell:AppsFolder\\{source}")), None)
                .ok()?
        };
        let key = PROPERTYKEY {
            fmtid: GUID::from_u128(0xb9b4b3fc_2b51_4a42_b5d8_324146afcf25),
            pid: 2,
        };
        let text = unsafe { item.GetString(&key).ok()? };
        let path = unsafe { text.to_string().ok() };
        unsafe {
            CoTaskMemFree(Some(text.0.cast()));
        }
        path.filter(|p| local_path(p) && p.to_ascii_lowercase().ends_with(".exe"))
    }
    pub fn resolve(source: &str) -> Result<ProcessIdentity, String> {
        let snapshot = Handle(
            unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) }
                .map_err(|e| e.to_string())?,
        );
        let mut entry = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        let mut records = Vec::new();
        let mut inaccessible_names = Vec::new();
        let mut next = unsafe { Process32FirstW(snapshot.0, &mut entry) };
        while next.is_ok() {
            if let Ok(mut identity) = ProcessIdentity::open(entry.th32ProcessID) {
                identity.record.parent = entry.th32ParentProcessID;
                records.push(identity.record.clone());
            } else {
                let end = entry
                    .szExeFile
                    .iter()
                    .position(|c| *c == 0)
                    .unwrap_or(entry.szExeFile.len());
                inaccessible_names.push(String::from_utf16_lossy(&entry.szExeFile[..end]));
            }
            next = unsafe { Process32NextW(snapshot.0, &mut entry) };
        }
        let path = registered_path(source);
        let selected = select_process(source, path.as_deref(), &records)
            .ok_or("source has no unique verified application process tree")?;
        if selected
            .image
            .rsplit(['/', '\\'])
            .next()
            .is_some_and(|leaf| {
                inaccessible_names
                    .iter()
                    .any(|name| name.eq_ignore_ascii_case(leaf))
            })
        {
            return Err(
                "a same-name process could not be verified; refusing ambiguous capture".into(),
            );
        }
        let mut opened = ProcessIdentity::open(selected.pid)?;
        if opened.record.created != selected.created
            || path_key(&opened.record.image) != path_key(&selected.image)
        {
            return Err("process identity changed while resolving".into());
        }
        opened.record = selected;
        Ok(opened)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// A player that exits while the identity (and so a handle on it) is still held: the
        /// devocal supervisor's player target must drop it at once, never hand the dead pid on
        /// (it would be attached again and spend the attach attempts on a process that is gone).
        #[test]
        fn identity_of_an_exited_process_is_not_alive() {
            let mut child = std::process::Command::new("ping")
                .args(["-n", "30", "127.0.0.1"])
                .stdout(std::process::Stdio::null())
                .spawn()
                .expect("spawn ping");
            let identity = ProcessIdentity::open(child.id());
            let alive = identity.as_ref().is_ok_and(ProcessIdentity::is_alive);
            let _ = child.kill();
            let _ = child.wait();
            let identity = identity.expect("open a running child");
            assert!(alive, "a running child is alive");
            assert!(!identity.is_alive());
            assert!(ProcessIdentity::open(identity.record.pid).is_err());
        }
    }
}
