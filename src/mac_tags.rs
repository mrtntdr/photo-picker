//! macOS Finder tags: picked photos get a "Picked" tag, so they can be found
//! in Finder and Spotlight. Tags live in an extended attribute, which a
//! temp-file-and-rename write would drop, so attributes are carried over.

use std::io;
use std::path::Path;

pub const PICKED_TAG: &str = "Picked";
const TAGS_ATTR: &str = "com.apple.metadata:_kMDItemUserTags";

/// Copies all extended attributes from `from` to `to`, adding or removing the
/// "Picked" tag. Other tags the user set are kept.
pub fn carry_over(from: &Path, to: &Path, picked: bool) -> io::Result<()> {
    for name in xattr::list(from)? {
        if name == TAGS_ATTR {
            continue;
        }
        if let Some(value) = xattr::get(from, &name)? {
            // Best effort: some attributes (e.g. protected system ones) can't be set.
            let _ = xattr::set(to, &name, &value);
        }
    }

    let old = xattr::get(from, TAGS_ATTR)?;
    let mut tags = match &old {
        Some(bytes) => match parse_tags(bytes) {
            Some(t) => t,
            None => {
                // Unreadable tag data: keep it untouched rather than lose it.
                return xattr::set(to, TAGS_ATTR, bytes);
            }
        },
        None => Vec::new(),
    };
    // A tag is stored as "Name" or "Name\n<color index>".
    let is_picked = |t: &String| t.split('\n').next() == Some(PICKED_TAG);
    let had = tags.iter().any(is_picked);
    if picked && !had {
        tags.push(PICKED_TAG.to_string());
    } else if !picked {
        tags.retain(|t| !is_picked(t));
    }

    if tags.is_empty() {
        return Ok(());
    }
    let mut buf = Vec::new();
    plist::Value::Array(tags.into_iter().map(plist::Value::String).collect())
        .to_writer_binary(&mut buf)
        .map_err(io::Error::other)?;
    xattr::set(to, TAGS_ATTR, &buf)
}

fn parse_tags(bytes: &[u8]) -> Option<Vec<String>> {
    let value = plist::Value::from_reader(io::Cursor::new(bytes)).ok()?;
    value
        .into_array()?
        .into_iter()
        .map(|v| v.into_string())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn adds_and_removes_picked_keeping_other_tags() {
        let dir = std::env::temp_dir().join(format!("picker-tags-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let (a, b, c) = (dir.join("a"), dir.join("b"), dir.join("c"));
        for p in [&a, &b, &c] {
            fs::write(p, b"x").unwrap();
        }
        let mut buf = Vec::new();
        plist::Value::Array(vec![plist::Value::String("Red\n6".into())]).to_writer_binary(&mut buf).unwrap();
        xattr::set(&a, TAGS_ATTR, &buf).unwrap();
        xattr::set(&a, "com.example.keep", b"1").unwrap();

        carry_over(&a, &b, true).unwrap();
        let tags = parse_tags(&xattr::get(&b, TAGS_ATTR).unwrap().unwrap()).unwrap();
        assert_eq!(tags, ["Red\n6", "Picked"]);
        assert_eq!(xattr::get(&b, "com.example.keep").unwrap().as_deref(), Some(&b"1"[..]));

        carry_over(&b, &c, false).unwrap();
        let tags = parse_tags(&xattr::get(&c, TAGS_ATTR).unwrap().unwrap()).unwrap();
        assert_eq!(tags, ["Red\n6"]);
        fs::remove_dir_all(&dir).unwrap();
    }
}
