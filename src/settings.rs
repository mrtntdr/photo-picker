//! User settings: a small text file of `name = value` lines, created with the
//! defaults on first run.
//!   macOS:   ~/Library/Application Support/Photo Picker/settings.txt
//!   Windows: %APPDATA%\Photo Picker\settings.txt

use std::fs;
use std::path::PathBuf;

use unicode_normalization::UnicodeNormalization;

use crate::keywords::MAX_KEYWORD_BYTES;

const DEFAULT_KEYWORD: &str = "pere";
const DEFAULT_FILE: &str = "\
# Photo Picker settings. Restart the app after changing this file.

# The keyword T adds to and removes from a photo, up to 64 bytes (õ, ä, ö, ü take two).
keyword = pere
";

#[derive(Debug, PartialEq)]
pub struct Settings {
    pub keyword: String,
}

impl Default for Settings {
    fn default() -> Self {
        Settings { keyword: DEFAULT_KEYWORD.to_string() }
    }
}

impl Settings {
    /// Reads the settings, creating the file if there is none. A problem with
    /// the file gives the defaults and an error to show the user.
    pub fn load() -> (Self, Option<String>) {
        let Some(path) = path() else { return (Self::default(), None) };
        match fs::read_to_string(&path) {
            Ok(text) => match parse(&text) {
                Ok(s) => (s, None),
                Err(e) => (Self::default(), Some(format!("{}: {e}", path.display()))),
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // Best effort: without the file the defaults still apply.
                if let Some(dir) = path.parent() {
                    let _ = fs::create_dir_all(dir);
                }
                let _ = fs::write(&path, DEFAULT_FILE);
                (Self::default(), None)
            }
            Err(e) => (Self::default(), Some(format!("{}: {e}", path.display()))),
        }
    }
}

fn path() -> Option<PathBuf> {
    let base = if cfg!(windows) {
        PathBuf::from(std::env::var_os("APPDATA")?)
    } else if cfg!(target_os = "macos") {
        PathBuf::from(std::env::var_os("HOME")?).join("Library/Application Support")
    } else {
        PathBuf::from(std::env::var_os("HOME")?).join(".config")
    };
    Some(base.join("Photo Picker").join("settings.txt"))
}

fn parse(text: &str) -> Result<Settings, String> {
    let mut settings = Settings::default();
    // Notepad may start the file with a byte order mark.
    for line in text.trim_start_matches('\u{feff}').lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        match line.split_once('=').map(|(k, v)| (k.trim(), v.trim())) {
            Some(("keyword", value)) => settings.keyword = check_keyword(value)?,
            _ => return Err(format!("not understood: {line}")),
        }
    }
    Ok(settings)
}

fn check_keyword(value: &str) -> Result<String, String> {
    let keyword: String = value.nfc().collect();
    if keyword.is_empty() || keyword.chars().any(char::is_control) {
        return Err("the keyword must be plain text".to_string());
    }
    if keyword.len() > MAX_KEYWORD_BYTES {
        return Err(format!("the keyword is longer than {MAX_KEYWORD_BYTES} bytes"));
    }
    Ok(keyword)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_keyword_and_rejects_bad_values() {
        assert_eq!(parse(DEFAULT_FILE).unwrap(), Settings::default());
        assert_eq!(parse("").unwrap(), Settings::default());
        assert_eq!(parse("\u{feff}# c\n\n  keyword =  õde \n").unwrap().keyword, "õde");
        // Decomposed input (o + combining tilde) is stored composed.
        assert_eq!(parse("keyword = o\u{303}de").unwrap().keyword, "õde");
        assert!(parse("keyword = ").is_err());
        assert!(parse("keywrod = pere").is_err());
        assert!(parse(&format!("keyword = {}", "õ".repeat(33))).is_err());
        assert!(parse(&format!("keyword = {}", "õ".repeat(32))).is_ok());
    }
}
