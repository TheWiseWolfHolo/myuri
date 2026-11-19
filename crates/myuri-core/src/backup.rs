//! A backup of the configuration as one file: the settings, assistants,
//! prompts, providers, icons and skills, optionally the conversations, and
//! optionally the keys, which are encrypted with a password. The same file
//! is what import, export and WebDAV sync exchange.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use chacha20poly1305::aead::{Aead, AeadCore, KeyInit, OsRng};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use serde::{Deserialize, Serialize};

use crate::config;
use crate::history::HISTORY_DIR;

/// The name the backup has on a WebDAV server.
pub const REMOTE_NAME: &str = "myuri-backup.json";
const VERSION: u32 = 1;
const ROUNDS: u32 = 210_000;

/// Folders that are part of a backup, besides the three settings files.
const FOLDERS: &[&str] = &[config::PROMPTS_DIR, "icons", "skills"];
/// Downloaded copies that come back by themselves.
const CACHES: &[&str] = &["icons/lobehub", "icons/web"];
const MCP_FILE: &str = "mcp.toml";

#[derive(Serialize, Deserialize)]
struct Sealed {
    salt: String,
    nonce: String,
    data: String,
}

#[derive(Serialize, Deserialize)]
pub struct Bundle {
    version: u32,
    /// Seconds since 1970.
    pub created: u64,
    /// Path inside the configuration folder, with `/`, to base64 contents.
    files: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    keys: Option<Sealed>,
}

fn walk(root: &Path, folder: &Path, found: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(folder) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk(root, &path, found);
        } else if let Ok(relative) = path.strip_prefix(root) {
            found.push(relative.to_path_buf());
        }
    }
}

fn slashed(path: &Path) -> String {
    path.components().map(|part| part.as_os_str().to_string_lossy()).collect::<Vec<_>>().join("/")
}

/// Everything in `dir` that belongs in a backup.
pub fn collect(dir: &Path, with_history: bool) -> BTreeMap<String, Vec<u8>> {
    let mut paths: Vec<PathBuf> = [config::SETTINGS_FILE, config::ASSISTANTS_FILE, config::PROVIDERS_FILE, MCP_FILE]
        .iter()
        .map(PathBuf::from)
        .collect();
    let folders = FOLDERS.iter().copied().chain(with_history.then_some(HISTORY_DIR));
    for folder in folders {
        walk(dir, &dir.join(folder), &mut paths);
    }
    paths
        .into_iter()
        .map(|path| (slashed(&path), path))
        .filter(|(name, _)| !CACHES.iter().any(|cache| name.starts_with(&format!("{cache}/"))))
        .filter_map(|(name, path)| fs::read(dir.join(path)).ok().map(|bytes| (name, bytes)))
        .collect()
}

fn cipher(password: &str, salt: &[u8]) -> ChaCha20Poly1305 {
    let mut key = [0u8; 32];
    pbkdf2::pbkdf2_hmac::<sha2::Sha256>(password.as_bytes(), salt, ROUNDS, &mut key);
    ChaCha20Poly1305::new(Key::from_slice(&key))
}

/// The backup as the bytes of its file. `keys` are provider ids with their
/// stored keys; they go in only together with the password that locks them.
pub fn pack(
    files: &BTreeMap<String, Vec<u8>>,
    keys: Option<(&BTreeMap<String, String>, &str)>,
    created: u64,
) -> Result<Vec<u8>, String> {
    let keys = match keys {
        None => None,
        Some((_, password)) if password.is_empty() => return Err("带上密钥的话要设一个密码".into()),
        Some((keys, password)) => {
            let plain = serde_json::to_vec(keys).map_err(|error| error.to_string())?;
            let salt: [u8; 16] = rand_bytes();
            let nonce = ChaCha20Poly1305::generate_nonce(&mut OsRng);
            let data = cipher(password, &salt)
                .encrypt(&nonce, plain.as_slice())
                .map_err(|_| "密钥加密失败".to_string())?;
            Some(Sealed { salt: STANDARD.encode(salt), nonce: STANDARD.encode(nonce), data: STANDARD.encode(data) })
        }
    };
    let bundle = Bundle {
        version: VERSION,
        created,
        files: files.iter().map(|(name, bytes)| (name.clone(), STANDARD.encode(bytes))).collect(),
        keys,
    };
    serde_json::to_vec(&bundle).map_err(|error| error.to_string())
}

fn rand_bytes<const N: usize>() -> [u8; N] {
    use chacha20poly1305::aead::rand_core::RngCore;
    let mut bytes = [0u8; N];
    OsRng.fill_bytes(&mut bytes);
    bytes
}

/// A path from a backup may only lead into the configuration folder.
fn safe(name: &str) -> Option<PathBuf> {
    let path = Path::new(name);
    let plain = !name.is_empty()
        && !name.contains(['\\', ':'])
        && path.components().all(|part| matches!(part, Component::Normal(_)));
    plain.then(|| path.to_path_buf())
}

impl Bundle {
    pub fn open(bytes: &[u8]) -> Result<Self, String> {
        let bundle: Self = serde_json::from_slice(bytes).map_err(|_| "这不是 Myuri 的备份文件".to_string())?;
        if bundle.version > VERSION {
            return Err("这份备份来自更新的版本，先升级 Myuri".into());
        }
        Ok(bundle)
    }

    pub fn has_keys(&self) -> bool {
        self.keys.is_some()
    }

    pub fn file_count(&self) -> usize {
        self.files.len()
    }

    /// The keys in the backup, by provider id.
    pub fn keys(&self, password: &str) -> Result<BTreeMap<String, String>, String> {
        let Some(sealed) = &self.keys else { return Ok(BTreeMap::new()) };
        let decode = |text: &str| STANDARD.decode(text).map_err(|_| "备份里的密钥部分坏了".to_string());
        let (salt, nonce, data) = (decode(&sealed.salt)?, decode(&sealed.nonce)?, decode(&sealed.data)?);
        if nonce.len() != 12 {
            return Err("备份里的密钥部分坏了".into());
        }
        let plain = cipher(password, &salt)
            .decrypt(Nonce::from_slice(&nonce), data.as_slice())
            .map_err(|_| "密码不对".to_string())?;
        serde_json::from_slice(&plain).map_err(|_| "备份里的密钥部分坏了".to_string())
    }

    fn file(&self, name: &str) -> Result<Option<Vec<u8>>, String> {
        self.files
            .get(name)
            .map(|text| STANDARD.decode(text).map_err(|_| format!("备份里的 {name} 坏了")))
            .transpose()
    }

    /// Write the files of the backup into `dir`. The three settings files
    /// are checked first; what is about to be replaced is copied to
    /// `backups\<stamp>\`. Files the backup does not have stay as they are.
    /// Returns how many files were written.
    pub fn restore(&self, dir: &Path, stamp: u64) -> Result<usize, String> {
        type Check = fn(&str) -> Result<(), String>;
        let checks: [(&str, Check); 3] = [
            (config::SETTINGS_FILE, |text| config::parse_settings(text).map(|_| ())),
            (config::ASSISTANTS_FILE, |text| config::parse_assistants(text).map(|_| ())),
            (config::PROVIDERS_FILE, |text| config::parse_providers(text).map(|_| ())),
        ];
        for (name, check) in checks {
            if let Some(bytes) = self.file(name)? {
                let text = String::from_utf8(bytes).map_err(|_| format!("备份里的 {name} 不是文本"))?;
                check(text.trim_start_matches('\u{feff}')).map_err(|problem| format!("备份里的 {name} 有问题：{problem}"))?;
            }
        }
        let mut writes = Vec::new();
        for name in self.files.keys() {
            let path = safe(name).ok_or_else(|| format!("备份里有不该有的路径：{name}"))?;
            writes.push((path, self.file(name)?.unwrap_or_default()));
        }
        let kept = dir.join("backups").join(stamp.to_string());
        let io = |error: io::Error| error.to_string();
        for (path, bytes) in &writes {
            let target = dir.join(path);
            if let Ok(old) = fs::read(&target) {
                if &old == bytes {
                    continue;
                }
                let copy = kept.join(path);
                if let Some(folder) = copy.parent() {
                    fs::create_dir_all(folder).map_err(io)?;
                }
                fs::write(copy, old).map_err(io)?;
            }
            if let Some(folder) = target.parent() {
                fs::create_dir_all(folder).map_err(io)?;
            }
            fs::write(target, bytes).map_err(io)?;
        }
        Ok(writes.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("myuri-backup-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_backup_restores_into_another_folder() {
        let from = scratch("from");
        config::ensure_defaults(&from).unwrap();
        fs::create_dir_all(from.join("icons/lobehub")).unwrap();
        fs::write(from.join("icons/a.png"), [1, 2, 3]).unwrap();
        fs::write(from.join("icons/lobehub/x.svg"), "<svg/>").unwrap();
        fs::write(from.join("ui-state.toml"), "page = \"chat\"").unwrap();
        let files = collect(&from, false);
        assert!(files.contains_key("config.toml") && files.contains_key("prompts/翻译.md"));
        assert_eq!(files["icons/a.png"], [1, 2, 3]);
        assert!(!files.contains_key("icons/lobehub/x.svg") && !files.contains_key("ui-state.toml"));

        let keys = BTreeMap::from([("openai".to_string(), "sk-one,sk-two".to_string())]);
        let packed = pack(&files, Some((&keys, "correct horse")), 7).unwrap();
        assert!(!String::from_utf8_lossy(&packed).contains("sk-one"));
        let bundle = Bundle::open(&packed).unwrap();
        assert!(bundle.has_keys() && bundle.created == 7);
        assert_eq!(bundle.keys("correct horse").unwrap(), keys);
        assert_eq!(bundle.keys("wrong").unwrap_err(), "密码不对");

        let to = scratch("to");
        fs::write(to.join("config.toml"), "# old\n").unwrap();
        assert_eq!(bundle.restore(&to, 99).unwrap(), files.len());
        assert_eq!(fs::read(to.join("icons/a.png")).unwrap(), [1, 2, 3]);
        assert_eq!(fs::read_to_string(to.join("backups/99/config.toml")).unwrap(), "# old\n");
        assert!(config::load_assistants(&to).is_ok());
        let _ = fs::remove_dir_all(from);
        let _ = fs::remove_dir_all(to);
    }

    #[test]
    fn keys_need_a_password_and_are_optional() {
        let files = BTreeMap::from([("config.toml".to_string(), b"".to_vec())]);
        let keys = BTreeMap::new();
        assert!(pack(&files, Some((&keys, "")), 1).is_err());
        let bundle = Bundle::open(&pack(&files, None, 1).unwrap()).unwrap();
        assert!(!bundle.has_keys() && bundle.keys("").unwrap().is_empty());
        assert!(Bundle::open(b"{}").is_err());
    }

    #[test]
    fn bad_backups_change_nothing() {
        let dir = scratch("bad");
        let escape = BTreeMap::from([("../evil.txt".to_string(), b"x".to_vec())]);
        let bundle = Bundle::open(&pack(&escape, None, 1).unwrap()).unwrap();
        assert!(bundle.restore(&dir, 1).is_err());
        let broken = BTreeMap::from([
            ("prompts/a.md".to_string(), b"x".to_vec()),
            ("providers.toml".to_string(), b"[[provider]]\nnope = 1\n".to_vec()),
        ]);
        let bundle = Bundle::open(&pack(&broken, None, 1).unwrap()).unwrap();
        assert!(bundle.restore(&dir, 1).unwrap_err().contains("providers.toml"));
        assert!(!dir.join("prompts/a.md").exists());
        assert!(safe("C:/x").is_none() && safe("a\\b").is_none() && safe("a/b.md").is_some());
        let _ = fs::remove_dir_all(dir);
    }
}
