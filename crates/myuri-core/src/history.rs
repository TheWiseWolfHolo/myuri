//! Saved conversations: one JSON file each in `history\`, plus an index of
//! their summaries so that listing them does not read every file.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub const HISTORY_DIR: &str = "history";
const INDEX_FILE: &str = "index.json";

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    /// "user" or "assistant".
    pub role: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub text: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub thoughts: String,
    /// Pictures sent with a user message, as MIME type and base64 data.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pictures: Vec<(String, String)>,
    /// Files of pictures the assistant generated.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub shots: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Conversation {
    pub id: String,
    pub assistant: String,
    /// `provider/model` it was last answered by.
    #[serde(default)]
    pub model: String,
    /// Seconds since 1970.
    pub created: u64,
    pub updated: u64,
    pub messages: Vec<Entry>,
}

/// What a list of conversations shows.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Summary {
    pub id: String,
    pub assistant: String,
    #[serde(default)]
    pub model: String,
    pub title: String,
    pub updated: u64,
    pub messages: usize,
}

impl Conversation {
    /// The first thing the user said, cut to one short line.
    pub fn title(&self) -> String {
        let first = self.messages.iter().find(|entry| entry.role == "user");
        let line = first.map(|entry| entry.text.trim()).unwrap_or_default();
        let line = line.lines().next().unwrap_or_default().trim();
        if line.is_empty() {
            return if first.is_some_and(|entry| !entry.pictures.is_empty()) { "图片".into() } else { "对话".into() };
        }
        let mut title: String = line.chars().take(48).collect();
        if line.chars().count() > 48 {
            title.push('…');
        }
        title
    }

    pub fn summary(&self) -> Summary {
        Summary {
            id: self.id.clone(),
            assistant: self.assistant.clone(),
            model: self.model.clone(),
            title: self.title(),
            updated: self.updated,
            messages: self.messages.len(),
        }
    }
}

/// An id that sorts by time and is safe as a file name.
pub fn new_id(millis: u128) -> String {
    format!("{millis:014}")
}

fn folder(dir: &Path) -> PathBuf {
    dir.join(HISTORY_DIR)
}

fn file(dir: &Path, id: &str) -> Option<PathBuf> {
    // Ids are made here; anything else must not reach the file system.
    let plain = !id.is_empty() && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-');
    plain.then(|| folder(dir).join(format!("{id}.json")))
}

fn read_index(dir: &Path) -> Option<BTreeMap<String, Summary>> {
    let text = fs::read_to_string(folder(dir).join(INDEX_FILE)).ok()?;
    serde_json::from_str(&text).ok()
}

fn write_index(dir: &Path, index: &BTreeMap<String, Summary>) -> io::Result<()> {
    replace(&folder(dir).join(INDEX_FILE), serde_json::to_string(index)?.as_bytes())
}

/// Write through a temporary file, so a crash never leaves half a file.
fn replace(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let partial = path.with_extension("tmp");
    fs::write(&partial, bytes)?;
    fs::rename(partial, path)
}

/// Summaries rebuilt from the conversation files themselves.
fn scan(dir: &Path) -> BTreeMap<String, Summary> {
    let Ok(entries) = fs::read_dir(folder(dir)) else { return BTreeMap::new() };
    entries
        .flatten()
        .filter(|entry| entry.file_name() != INDEX_FILE)
        .filter_map(|entry| fs::read_to_string(entry.path()).ok())
        .filter_map(|text| serde_json::from_str::<Conversation>(&text).ok())
        .map(|conversation| (conversation.id.clone(), conversation.summary()))
        .collect()
}

pub fn save(dir: &Path, conversation: &Conversation) -> io::Result<()> {
    let path = file(dir, &conversation.id).ok_or(io::ErrorKind::InvalidInput)?;
    fs::create_dir_all(folder(dir))?;
    replace(&path, serde_json::to_string(conversation)?.as_bytes())?;
    let mut index = read_index(dir).unwrap_or_else(|| scan(dir));
    index.insert(conversation.id.clone(), conversation.summary());
    write_index(dir, &index)
}

pub fn load(dir: &Path, id: &str) -> Option<Conversation> {
    let text = fs::read_to_string(file(dir, id)?).ok()?;
    serde_json::from_str(&text).ok()
}

pub fn delete(dir: &Path, id: &str) -> io::Result<()> {
    if let Some(path) = file(dir, id) {
        match fs::remove_file(path) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error),
            _ => {}
        }
    }
    let mut index = read_index(dir).unwrap_or_else(|| scan(dir));
    index.remove(id);
    write_index(dir, &index)
}

/// Every saved conversation, newest first.
pub fn list(dir: &Path) -> Vec<Summary> {
    let index = read_index(dir).unwrap_or_else(|| {
        let index = scan(dir);
        if !index.is_empty() {
            let _ = write_index(dir, &index);
        }
        index
    });
    let mut summaries: Vec<Summary> = index.into_values().collect();
    summaries.sort_by(|a, b| b.updated.cmp(&a.updated).then_with(|| b.id.cmp(&a.id)));
    summaries
}

/// How long ago `then` was, in words.
pub fn ago(now: u64, then: u64) -> String {
    let seconds = now.saturating_sub(then);
    match seconds {
        0..60 => "刚刚".into(),
        60..3600 => format!("{} 分钟前", seconds / 60),
        3600..86_400 => format!("{} 小时前", seconds / 3600),
        86_400..172_800 => "昨天".into(),
        172_800..2_592_000 => format!("{} 天前", seconds / 86_400),
        2_592_000..31_536_000 => format!("{} 个月前", seconds / 2_592_000),
        _ => format!("{} 年前", seconds / 31_536_000),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(id: &str, updated: u64, text: &str) -> Conversation {
        Conversation {
            id: id.into(),
            assistant: "翻译".into(),
            model: "a/b".into(),
            created: updated,
            updated,
            messages: vec![
                Entry { role: "user".into(), text: text.into(), ..Default::default() },
                Entry { role: "assistant".into(), text: "好".into(), thoughts: "想".into(), ..Default::default() },
            ],
        }
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("myuri-history-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn conversations_round_trip_and_list_newest_first() {
        let dir = scratch("roundtrip");
        save(&dir, &sample("00000000000001", 10, "first\nsecond line")).unwrap();
        save(&dir, &sample("00000000000002", 30, "later")).unwrap();
        assert_eq!(load(&dir, "00000000000001").unwrap(), sample("00000000000001", 10, "first\nsecond line"));
        let listed = list(&dir);
        assert_eq!(listed.iter().map(|s| s.title.as_str()).collect::<Vec<_>>(), ["later", "first"]);
        assert_eq!(listed[0].messages, 2);
        // Saving again replaces, it does not add.
        save(&dir, &sample("00000000000001", 40, "first")).unwrap();
        assert_eq!(list(&dir).len(), 2);
        assert_eq!(list(&dir)[0].id, "00000000000001");
        delete(&dir, "00000000000002").unwrap();
        delete(&dir, "00000000000002").unwrap();
        assert_eq!(list(&dir).len(), 1);
        assert!(load(&dir, "00000000000002").is_none());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_lost_index_is_rebuilt() {
        let dir = scratch("index");
        save(&dir, &sample("00000000000001", 10, "kept")).unwrap();
        fs::write(folder(&dir).join(INDEX_FILE), "not json").unwrap();
        assert_eq!(list(&dir)[0].title, "kept");
        fs::remove_file(folder(&dir).join(INDEX_FILE)).unwrap();
        assert_eq!(list(&dir).len(), 1);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn ids_cannot_leave_the_folder() {
        let dir = scratch("ids");
        assert!(load(&dir, "../config").is_none());
        assert!(save(&dir, &sample("..\\x", 1, "x")).is_err());
        assert_eq!(new_id(1_763_568_000_123), "01763568000123");
    }

    #[test]
    fn titles_and_ages_read_naturally() {
        let long = "字".repeat(60);
        assert_eq!(sample("1", 1, &long).title().chars().count(), 49);
        let mut picture = sample("1", 1, "");
        picture.messages[0].pictures.push(("image/png".into(), "x".into()));
        assert_eq!(picture.title(), "图片");
        assert_eq!(ago(100, 90), "刚刚");
        assert_eq!(ago(4000, 100), "1 小时前");
        assert_eq!(ago(200_000, 100), "2 天前");
    }
}
