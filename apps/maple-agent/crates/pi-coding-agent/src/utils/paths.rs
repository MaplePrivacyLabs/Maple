//! Selected path functions from `utils/paths.ts`; installer cloud-sync attributes are excluded.
use std::{
    fs,
    path::{Component, Path, PathBuf},
};

#[derive(Clone, Debug, Default)]
pub struct PathInputOptions {
    pub trim: bool,
    pub expand_tilde: Option<bool>,
    pub home_dir: Option<String>,
    pub strip_at_prefix: bool,
    pub normalize_unicode_spaces: bool,
}
pub fn canonicalize_path(path: &str) -> String {
    fs::canonicalize(path)
        .map(|p| p.to_string_lossy().into())
        .unwrap_or_else(|_| path.into())
}
pub fn get_file_revision(path: &str) -> Option<String> {
    let meta = fs::metadata(path).ok()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Some(format!(
            "{}:{}:{}:{}:{}",
            meta.dev(),
            meta.ino(),
            meta.size(),
            i128::from(meta.mtime()) * 1_000_000_000 + i128::from(meta.mtime_nsec()),
            i128::from(meta.ctime()) * 1_000_000_000 + i128::from(meta.ctime_nsec())
        ))
    }
    #[cfg(not(unix))]
    {
        let _ = meta;
        None
    }
}
pub fn is_local_path(value: &str) -> bool {
    let value = value.trim_matches(js_whitespace);
    ![
        "npm:", "git:", "github:", "http:", "https:", "ssh:", "builtin:",
    ]
    .iter()
    .any(|prefix| value.starts_with(prefix))
}
pub fn js_whitespace(c: char) -> bool {
    matches!(c,'\u{9}'..='\u{d}'|'\u{20}'|'\u{a0}'|'\u{1680}'|'\u{2000}'..='\u{200a}'|'\u{2028}'|'\u{2029}'|'\u{202f}'|'\u{205f}'|'\u{3000}'|'\u{feff}')
}
pub fn normalize_windows_shell_path(path: &str) -> String {
    if !path.starts_with('/') || path.starts_with("//") || path.contains('\\') {
        return path.into();
    }
    let value = path
        .strip_prefix("/mnt/")
        .or_else(|| path.strip_prefix("/cygdrive/"))
        .unwrap_or(&path[1..]);
    let bytes = value.as_bytes();
    if bytes.first().is_none_or(|b| !b.is_ascii_alphabetic())
        || bytes.get(1).is_some_and(|b| *b != b'/')
    {
        return path.into();
    }
    format!(
        "{}:\\{}",
        char::from(bytes[0].to_ascii_uppercase()),
        value.get(2..).unwrap_or_default().replace('/', "\\")
    )
}
pub fn normalize_path(input: &str, options: &PathInputOptions) -> Result<String, String> {
    let mut value = if options.trim {
        input.trim_matches(js_whitespace)
    } else {
        input
    }
    .to_owned();
    if options.normalize_unicode_spaces {
        value = value
            .chars()
            .map(|c| {
                if matches!(
                    c,
                    '\u{a0}' | '\u{2000}'..='\u{200a}' | '\u{202f}' | '\u{205f}' | '\u{3000}'
                ) {
                    ' '
                } else {
                    c
                }
            })
            .collect();
    }
    if options.strip_at_prefix && value.starts_with('@') {
        value.remove(0);
    }
    if cfg!(windows) {
        value = normalize_windows_shell_path(&value);
    }
    if options.expand_tilde != Some(false) {
        let home = options.home_dir.clone().unwrap_or_else(|| {
            std::env::var(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).unwrap_or_default()
        });
        if value == "~" {
            return Ok(home);
        }
        if value.starts_with("~/") || (cfg!(windows) && value.starts_with("~\\")) {
            return Ok(lexical_path(&Path::new(&home).join(&value[2..]))
                .to_string_lossy()
                .into());
        }
    }
    if value.starts_with("file://") {
        let parsed = url::Url::parse(&value).map_err(|e| e.to_string())?;
        if !cfg!(windows)
            && parsed
                .host_str()
                .is_some_and(|host| host != "localhost" && !host.is_empty())
        {
            return Err("File URL host must be localhost or empty on this platform".into());
        }
        let path = parsed.path();
        let bytes = path.as_bytes();
        let mut decoded = Vec::new();
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == b'%' {
                if i + 2 >= bytes.len() {
                    return Err("URI malformed".into());
                }
                let digit = |v: u8| -> Option<u8> {
                    match v {
                        b'0'..=b'9' => Some(v - b'0'),
                        b'a'..=b'f' => Some(v - b'a' + 10),
                        b'A'..=b'F' => Some(v - b'A' + 10),
                        _ => None,
                    }
                };
                let next = digit(bytes[i + 1])
                    .zip(digit(bytes[i + 2]))
                    .map(|(a, b)| a * 16 + b)
                    .ok_or("URI malformed")?;
                if next == b'/' || (cfg!(windows) && next == b'\\') {
                    return Err("File URL path must not include encoded path separators".into());
                }
                decoded.push(next);
                i += 3;
            } else {
                decoded.push(bytes[i]);
                i += 1;
            }
        }
        let path = String::from_utf8(decoded).map_err(|_| "URI malformed")?;
        if cfg!(windows) {
            return parsed
                .to_file_path()
                .map(|p| p.to_string_lossy().into())
                .map_err(|_| "Invalid file URL path".into());
        }
        return Ok(path);
    }
    Ok(value)
}
pub fn lexical_path(path: &Path) -> PathBuf {
    let mut result = PathBuf::new();
    for part in path.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => {
                if !result.pop() && !path.is_absolute() {
                    result.push("..");
                }
            }
            other => result.push(other.as_os_str()),
        }
    }
    if result.as_os_str().is_empty() {
        result.push(".");
    }
    result
}
pub fn resolve_path(
    input: &str,
    base_dir: &str,
    options: &PathInputOptions,
) -> Result<String, String> {
    let path = normalize_path(input, options)?;
    let base = normalize_path(base_dir, &PathInputOptions::default())?;
    let path = if Path::new(&path).is_absolute() {
        PathBuf::from(path)
    } else {
        Path::new(&base).join(path)
    };
    let absolute = if path.is_absolute() {
        path
    } else {
        std::env::current_dir()
            .map_err(|e| e.to_string())?
            .join(path)
    };
    Ok(lexical_path(&absolute).to_string_lossy().into())
}
pub fn get_cwd_relative_path(file_path: &str, cwd: &str) -> Result<Option<String>, String> {
    let process_cwd = std::env::current_dir().map_err(|e| e.to_string())?;
    let resolved_cwd = resolve_path(
        cwd,
        &process_cwd.to_string_lossy(),
        &PathInputOptions::default(),
    )?;
    let resolved_path = resolve_path(file_path, &resolved_cwd, &PathInputOptions::default())?;
    Ok(Path::new(&resolved_path)
        .strip_prefix(&resolved_cwd)
        .ok()
        .map(|path| {
            if path.as_os_str().is_empty() {
                ".".into()
            } else {
                path.to_string_lossy().into()
            }
        }))
}
pub fn format_path_relative_to_cwd_or_absolute(
    file_path: &str,
    cwd: &str,
) -> Result<String, String> {
    let absolute = resolve_path(file_path, cwd, &PathInputOptions::default())?;
    Ok(get_cwd_relative_path(&absolute, cwd)?
        .unwrap_or(absolute)
        .replace(std::path::MAIN_SEPARATOR, "/"))
}
