//! Read-only application identity for a GSMTC source; never controls the player.
use std::time::{Duration, Instant};

#[derive(Clone, Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SourceIdentity {
    pub name: String,
    pub icon_data_url: Option<String>,
}

fn clean_name(value: &str) -> Option<String> {
    let value = value.trim();
    let leaf = value.rsplit(['\\', '/']).next()?;
    let name = if leaf.to_ascii_lowercase().ends_with(".exe") {
        &leaf[..leaf.len() - 4]
    } else {
        leaf
    };
    (!name.is_empty() && name.chars().count() <= 256 && !name.chars().any(char::is_control))
        .then(|| name.to_owned())
}

pub fn fallback_name(id: &str, name: Option<&str>) -> String {
    if id.to_ascii_lowercase().ends_with(".exe") {
        if let Some(name) = clean_name(id) {
            return name;
        }
    }
    // nowplaying synthesizes a name from AUMIDs. Do not expose that internal ID
    // (or its guessed word expansion) while Windows resolves the real app name.
    if !id.contains(['.', '!', '\\', '/', ':']) {
        if let Some(name) = name.and_then(clean_name).or_else(|| clean_name(id)) {
            return name;
        }
    }
    "媒体播放器".into()
}

pub fn resolve(id: &str, fallback: &str) -> SourceIdentity {
    #[cfg(windows)]
    if let Some(identity) = native::resolve(id) {
        return identity;
    }
    SourceIdentity {
        name: fallback.into(),
        icon_data_url: None,
    }
}

/// Only one resolution job and one active-source cache. Shell work never delays
/// media polling; late results cannot be displayed for a different source.
#[derive(Default)]
pub struct SourceCache {
    cached: Option<(String, SourceIdentity, Instant)>,
    pending: Option<(String, String, tokio::task::JoinHandle<SourceIdentity>)>,
}

impl SourceCache {
    pub async fn current(&mut self, id: &str, fallback: &str) -> SourceIdentity {
        if self
            .pending
            .as_ref()
            .is_some_and(|(_, _, job)| job.is_finished())
        {
            let (completed_id, completed_fallback, job) = self.pending.take().unwrap();
            let identity = job.await.unwrap_or(SourceIdentity {
                name: completed_fallback,
                icon_data_url: None,
            });
            self.cached = Some((completed_id, identity, Instant::now()));
        }
        let cached = self.cached.as_ref().filter(|(key, _, _)| key == id);
        let needs_refresh = cached.is_none_or(|(_, identity, at)| {
            at.elapsed()
                >= Duration::from_secs(if identity.icon_data_url.is_some() {
                    300
                } else {
                    30
                })
        });
        if self.pending.is_none() && needs_refresh {
            let key = id.to_owned();
            let name = fallback.to_owned();
            let (job_key, job_name) = (key.clone(), name.clone());
            self.pending = Some((
                key,
                name,
                tokio::task::spawn_blocking(move || resolve(&job_key, &job_name)),
            ));
        }
        cached
            .map(|(_, identity, _)| identity.clone())
            .unwrap_or(SourceIdentity {
                name: fallback.into(),
                icon_data_url: None,
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn cached_identity_stays_with_its_source_without_restarting_resolution() {
        let identity = SourceIdentity {
            name: "Folia".into(),
            icon_data_url: Some("cached-icon".into()),
        };
        let mut cache = SourceCache {
            cached: Some(("folia.exe".into(), identity, Instant::now())),
            pending: None,
        };
        let result = cache.current("folia.exe", "Fallback").await;
        assert_eq!(result.name, "Folia");
        assert_eq!(result.icon_data_url.as_deref(), Some("cached-icon"));
        assert!(cache.pending.is_none());
    }

    #[tokio::test]
    async fn a_pending_old_source_neither_blocks_nor_supplies_the_new_sources_icon() {
        let job = tokio::spawn(std::future::pending::<SourceIdentity>());
        let mut cache = SourceCache {
            cached: Some((
                "old.exe".into(),
                SourceIdentity {
                    name: "Old".into(),
                    icon_data_url: Some("old-icon".into()),
                },
                Instant::now(),
            )),
            pending: Some(("old.exe".into(), "Old".into(), job)),
        };
        let result = tokio::time::timeout(
            Duration::from_millis(100),
            cache.current("next.exe", "Next"),
        )
        .await
        .unwrap();
        assert_eq!(result.name, "Next");
        assert!(result.icon_data_url.is_none());
        assert_eq!(cache.pending.as_ref().unwrap().0, "old.exe");
        cache.pending.take().unwrap().2.abort();
    }
}

#[cfg(windows)]
mod native {
    use super::{clean_name, SourceIdentity};
    use base64::Engine;
    use std::{mem::size_of, path::PathBuf};
    use windows::{
        core::{Interface, HSTRING, PWSTR},
        Win32::{
            Foundation::{CloseHandle, HANDLE, SIZE},
            Graphics::Gdi::{
                DeleteObject, GetDC, GetDIBits, GetObjectW, ReleaseDC, BITMAP, BITMAPINFO,
                BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, HBITMAP, HGDIOBJ,
            },
            System::{
                Com::{CoInitializeEx, CoTaskMemFree, CoUninitialize, COINIT_APARTMENTTHREADED},
                Diagnostics::ToolHelp::{
                    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
                    TH32CS_SNAPPROCESS,
                },
                Threading::{
                    OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
                    PROCESS_QUERY_LIMITED_INFORMATION,
                },
            },
            UI::Shell::{
                IShellItem, IShellItemImageFactory, SHCreateItemFromParsingName,
                SIGDN_NORMALDISPLAY, SIIGBF_ICONONLY,
            },
        },
    };

    struct Apartment;
    impl Drop for Apartment {
        fn drop(&mut self) {
            unsafe {
                CoUninitialize();
            }
        }
    }
    struct Handle(HANDLE);
    impl Drop for Handle {
        fn drop(&mut self) {
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }
    struct Bitmap(HBITMAP);
    impl Drop for Bitmap {
        fn drop(&mut self) {
            unsafe {
                let _ = DeleteObject(HGDIOBJ(self.0 .0));
            }
        }
    }

    pub(super) fn resolve(id: &str) -> Option<SourceIdentity> {
        if id.is_empty() || id.len() > 1024 || id.chars().any(char::is_control) {
            return None;
        }
        unsafe {
            CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok().ok()?;
        }
        let _apartment = Apartment;
        // AppsFolder resolves the OS's registered identity for Store apps and
        // registered Win32/Electron apps alike, including their real icons.
        if !id.contains(['\\', '/', ':']) {
            if let Some(identity) = shell_identity(&format!("shell:AppsFolder\\{id}")) {
                return Some(identity);
            }
        }
        // Bare executable IDs need the running image's path, not a hardcoded
        // player table or an arbitrary file path supplied by media metadata.
        let path = process_path(id)?;
        let mut identity = shell_identity(path.to_str()?)?;
        identity.name = clean_name(path.to_str()?)?;
        Some(identity)
    }

    fn shell_identity(path: &str) -> Option<SourceIdentity> {
        let item: IShellItem =
            unsafe { SHCreateItemFromParsingName(&HSTRING::from(path), None).ok()? };
        let name = unsafe {
            let text = item.GetDisplayName(SIGDN_NORMALDISPLAY).ok()?;
            let value = text.to_string().ok();
            CoTaskMemFree(Some(text.0.cast()));
            clean_name(&value?)?
        };
        let icon_data_url = item
            .cast::<IShellItemImageFactory>()
            .ok()
            .and_then(|factory| {
                let bitmap = Bitmap(unsafe {
                    factory
                        .GetImage(SIZE { cx: 32, cy: 32 }, SIIGBF_ICONONLY)
                        .ok()?
                });
                bitmap_png(&bitmap)
            });
        Some(SourceIdentity {
            name,
            icon_data_url,
        })
    }

    fn process_path(id: &str) -> Option<PathBuf> {
        let filename = id.rsplit(['\\', '/']).next()?;
        if !filename.to_ascii_lowercase().ends_with(".exe") {
            return None;
        }
        let snapshot = Handle(unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0).ok()? });
        let mut entry = PROCESSENTRY32W {
            dwSize: size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        let mut result = None;
        let mut next = unsafe { Process32FirstW(snapshot.0, &mut entry) };
        while next.is_ok() {
            let length = entry
                .szExeFile
                .iter()
                .position(|c| *c == 0)
                .unwrap_or(entry.szExeFile.len());
            if String::from_utf16_lossy(&entry.szExeFile[..length]).eq_ignore_ascii_case(filename) {
                if let Ok(handle) = unsafe {
                    OpenProcess(
                        PROCESS_QUERY_LIMITED_INFORMATION,
                        false,
                        entry.th32ProcessID,
                    )
                } {
                    let handle = Handle(handle);
                    let mut buffer = vec![0_u16; 32768];
                    let mut length = buffer.len() as u32;
                    if unsafe {
                        QueryFullProcessImageNameW(
                            handle.0,
                            PROCESS_NAME_WIN32,
                            PWSTR(buffer.as_mut_ptr()),
                            &mut length,
                        )
                    }
                    .is_ok()
                    {
                        let path = String::from_utf16_lossy(&buffer[..length as usize]);
                        let matching_path =
                            !id.contains(['\\', '/']) || path.eq_ignore_ascii_case(id);
                        // Do not ask Shell to resolve network paths.
                        if matching_path && !path.starts_with("\\\\") {
                            let candidate = PathBuf::from(path);
                            if result
                                .as_ref()
                                .is_some_and(|old: &PathBuf| old != &candidate)
                            {
                                return None;
                            }
                            result = Some(candidate);
                        }
                    }
                }
            }
            next = unsafe { Process32NextW(snapshot.0, &mut entry) };
        }
        result
    }

    fn bitmap_png(bitmap: &Bitmap) -> Option<String> {
        let mut info = BITMAP::default();
        let read = unsafe {
            GetObjectW(
                HGDIOBJ(bitmap.0 .0),
                size_of::<BITMAP>() as i32,
                Some((&mut info as *mut BITMAP).cast()),
            )
        };
        if read != size_of::<BITMAP>() as i32
            || !(1..=256).contains(&info.bmWidth)
            || !(1..=256).contains(&info.bmHeight)
        {
            return None;
        }
        let (width, height) = (info.bmWidth as u32, info.bmHeight as u32);
        let mut pixels = vec![0_u8; (width * height * 4) as usize];
        let mut dib = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: info.bmWidth,
                biHeight: -info.bmHeight,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let dc = unsafe { GetDC(None) };
        if dc.0.is_null() {
            return None;
        }
        let lines = unsafe {
            GetDIBits(
                dc,
                bitmap.0,
                0,
                height,
                Some(pixels.as_mut_ptr().cast()),
                &mut dib,
                DIB_RGB_COLORS,
            )
        };
        unsafe {
            ReleaseDC(None, dc);
        }
        if lines != height as i32 {
            return None;
        }
        // Shell bitmaps are premultiplied BGRA; PNG stores straight RGBA.
        let has_alpha = pixels.chunks_exact(4).any(|pixel| pixel[3] != 0);
        for pixel in pixels.chunks_exact_mut(4) {
            pixel.swap(0, 2);
            if !has_alpha {
                pixel[3] = 255;
            }
            let alpha = u32::from(pixel[3]);
            if alpha > 0 && alpha < 255 {
                for channel in &mut pixel[..3] {
                    *channel = ((u32::from(*channel) * 255 + alpha / 2) / alpha).min(255) as u8;
                }
            }
        }
        let mut bytes = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut bytes, width, height);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            let mut writer = encoder.write_header().ok()?;
            writer.write_image_data(&pixels).ok()?;
            writer.finish().ok()?;
        }
        Some(format!(
            "data:image/png;base64,{}",
            base64::engine::general_purpose::STANDARD.encode(bytes)
        ))
    }
}
