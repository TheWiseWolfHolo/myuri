//! Changing single values in the configuration files while leaving the rest
//! of the text, comments included, as the user wrote it.

use toml_edit::{DocumentMut, Item, value};

/// A new value for a key. `Absent` removes the key.
#[derive(Clone, Debug, PartialEq)]
pub enum Field {
    Text(String),
    Flag(bool),
    Number(i64),
    Float(f64),
    /// A list written one item per line. Empty removes the key.
    List(Vec<String>),
    /// An array of inline tables, one row per entry, e.g.
    /// `keys = [ { note = "a" }, { enabled = false } ]`. Empty removes the key.
    Table(Vec<Vec<(&'static str, Field)>>),
    /// An inline table of text values, e.g. `env = { A = "1" }`. Empty
    /// removes the key.
    Map(Vec<(String, String)>),
    Absent,
}

impl Field {
    /// Text, or absent when the text is blank.
    pub fn optional(text: &str) -> Self {
        let text = text.trim();
        if text.is_empty() { Self::Absent } else { Self::Text(text.to_string()) }
    }

    fn item(&self) -> Option<Item> {
        match self {
            Self::Text(text) => Some(value(text.as_str())),
            Self::Flag(flag) => Some(value(*flag)),
            Self::Number(number) => Some(value(*number)),
            Self::Float(number) => {
                if !number.is_finite() {
                    return Some(value(*number));
                }
                let rounded = format!("{number:.3}");
                let stripped = rounded.trim_end_matches('0');
                let text = if stripped.ends_with('.') {
                    format!("{stripped}0")
                } else {
                    stripped.to_string()
                };
                Some(Item::Value(text.parse().expect("formatted TOML float")))
            }
            Self::List(items) if items.is_empty() => None,
            Self::List(items) => {
                let mut array = toml_edit::Array::new();
                for item in items {
                    array.push_formatted(toml_edit::Value::from(item.as_str()).decorated("\n  ", ""));
                }
                array.set_trailing("\n");
                array.set_trailing_comma(true);
                Some(value(array))
            }
            Self::Map(entries) if entries.is_empty() => None,
            Self::Map(entries) => {
                let mut table = toml_edit::InlineTable::new();
                for (key, text) in entries {
                    table.insert(key, text.as_str().into());
                }
                Some(value(table))
            }
            Self::Table(rows) if rows.is_empty() => None,
            Self::Table(rows) => {
                let mut array = toml_edit::Array::new();
                for row in rows {
                    let mut table = toml_edit::InlineTable::new();
                    for (key, field) in row {
                        if let Some(value) = field.item().and_then(|item| item.as_value().cloned()) {
                            table.insert(*key, value);
                        }
                    }
                    array.push_formatted(
                        toml_edit::Value::InlineTable(table).decorated("\n  ", ""),
                    );
                }
                array.set_trailing("\n");
                array.set_trailing_comma(true);
                Some(value(array))
            }
            Self::Absent => None,
        }
    }
}

/// Set or remove one key. A value that is replaced keeps the comment that
/// follows it on its line.
fn put(table: &mut dyn toml_edit::TableLike, key: &str, field: &Field) {
    let Some(item) = field.item() else {
        table.remove(key);
        return;
    };
    let decor = table.get(key).and_then(Item::as_value).map(|old| old.decor().clone());
    table.insert(key, item);
    if let (Some(decor), Some(new)) = (decor, table.get_mut(key).and_then(Item::as_value_mut)) {
        *new.decor_mut() = decor;
    }
}

fn parse(source: &str) -> Result<DocumentMut, String> {
    source.trim_start_matches('\u{feff}').parse().map_err(|error: toml_edit::TomlError| error.to_string())
}

/// Change keys of one `[[array]]` entry, the one whose `id_key` is `id`.
pub fn set_entry(
    source: &str,
    array: &str,
    id_key: &str,
    id: &str,
    changes: &[(&str, Field)],
) -> Result<String, String> {
    let mut document = parse(source)?;
    let table = document
        .get_mut(array)
        .and_then(Item::as_array_of_tables_mut)
        .and_then(|tables| {
            tables.iter_mut().find(|table| table.get(id_key).and_then(Item::as_str) == Some(id))
        })
        .ok_or_else(|| format!("文件里没有 {id_key} 是 \"{id}\" 的 [[{array}]]"))?;
    for (key, field) in changes {
        put(table, key, field);
    }
    Ok(document.to_string())
}

/// Put the `[[array]]` entries in a new order: `order` lists their current
/// positions as they should now stand. Comments stay with their entries,
/// except the ones heading the file, which stay at the top.
pub fn reorder(source: &str, array: &str, order: &[usize]) -> Result<String, String> {
    let mut document = parse(source)?;
    let tables = document
        .get_mut(array)
        .and_then(Item::as_array_of_tables_mut)
        .ok_or_else(|| format!("文件里没有 [[{array}]]"))?;
    let mut all: Vec<toml_edit::Table> = tables.iter().cloned().collect();
    let mut sorted = order.to_vec();
    sorted.sort_unstable();
    if sorted != (0..all.len()).collect::<Vec<_>>() {
        return Err("文件刚被改过，重新打开设置再试".into());
    }
    if order.first().is_some_and(|&first| first != 0) {
        let head = all[0].decor().prefix().cloned();
        let moved = all[order[0]].decor().prefix().cloned();
        if let (Some(head), Some(moved)) = (head, moved) {
            all[0].decor_mut().set_prefix(moved);
            all[order[0]].decor_mut().set_prefix(head);
        }
    }
    // Tables are written in the order of their recorded positions, so each
    // one takes over the position of the slot it moves into.
    let slots: Vec<Option<isize>> = all.iter().map(toml_edit::Table::position).collect();
    tables.clear();
    for (slot, &index) in slots.into_iter().zip(order) {
        let mut table = all[index].clone();
        table.set_position(slot);
        clear_nested_positions(&mut table);
        tables.push(table);
    }
    Ok(document.to_string())
}

fn clear_nested_positions(table: &mut toml_edit::Table) {
    for (_, item) in table.iter_mut() {
        match item {
            Item::Table(sub) => {
                sub.set_position(None);
                clear_nested_positions(sub);
            }
            Item::ArrayOfTables(array) => {
                for sub in array.iter_mut() {
                    sub.set_position(None);
                    clear_nested_positions(sub);
                }
            }
            _ => {}
        }
    }
}

/// Remove the `[[array]]` entry whose `id_key` is `id`.
pub fn remove_entry(source: &str, array: &str, id_key: &str, id: &str) -> Result<String, String> {
    let mut document = parse(source)?;
    let tables = document
        .get_mut(array)
        .and_then(Item::as_array_of_tables_mut)
        .ok_or_else(|| format!("文件里没有 [[{array}]]"))?;
    let index = tables
        .iter()
        .position(|table| table.get(id_key).and_then(Item::as_str) == Some(id))
        .ok_or_else(|| format!("文件里没有 {id_key} 是 \"{id}\" 的 [[{array}]]"))?;
    tables.remove(index);
    Ok(document.to_string())
}

/// The order after moving the entries at `block` next to `anchor`: right
/// after it when `after`, otherwise right before.
pub fn moved(len: usize, block: &[usize], anchor: usize, after: bool) -> Vec<usize> {
    let mut order: Vec<usize> = (0..len).filter(|index| !block.contains(index)).collect();
    let at = match order.iter().position(|&index| index == anchor) {
        Some(at) if after => at + 1,
        Some(at) => at,
        None => return (0..len).collect(),
    };
    order.splice(at..at, block.iter().copied());
    order
}

/// Rename provider `old` to `new` in the three files' texts: its `[[provider]]`
/// id, `favorites` entries, assistants' `model = "old/..."`, and `[models]`
/// tier values in config.toml. Comments kept. Only `old/` prefixes move, so
/// renaming `wolf` leaves `wolfpack/x` alone.
pub fn rename_provider(
    providers: &str,
    assistants: &str,
    settings: &str,
    old: &str,
    new: &str,
) -> Result<(String, String, String), String> {
    let prefix = format!("{old}/");
    let retarget = |text: &str| text.strip_prefix(&prefix).map(|rest| format!("{new}/{rest}"));

    let providers = rename_provider_with_favorites(providers, old, new)?;

    let mut assistants = parse(assistants)?;
    if let Some(tables) = assistants.get_mut("assistant").and_then(Item::as_array_of_tables_mut) {
        for table in tables.iter_mut() {
            if let Some(next) = table.get("model").and_then(Item::as_str).and_then(&retarget) {
                put(table, "model", &Field::Text(next));
            }
        }
    }

    let mut settings = parse(settings)?;
    if let Some(models) = settings.get_mut("models").and_then(Item::as_table_like_mut) {
        let keys: Vec<String> = models.iter().map(|(key, _)| key.to_string()).collect();
        for key in keys {
            if let Some(next) = models.get(&key).and_then(Item::as_str).and_then(&retarget) {
                put(models, &key, &Field::Text(next));
            }
        }
    }
    Ok((providers, assistants.to_string(), settings.to_string()))
}

pub fn rename_image_provider(
    providers: &str,
    settings: &str,
    old: &str,
    new: &str,
) -> Result<(String, String), String> {
    let providers = rename_provider_with_favorites(providers, old, new)?;
    let mut settings = parse(settings)?;
    let prefix = format!("{old}/");
    if let Some(image) = settings.get_mut("image").and_then(Item::as_table_like_mut)
        && let Some(next) = image
            .get("default")
            .and_then(Item::as_str)
            .and_then(|text| text.strip_prefix(&prefix))
            .map(|rest| format!("{new}/{rest}"))
    {
        put(image, "default", &Field::Text(next));
    }
    Ok((providers, settings.to_string()))
}

/// Add or remove `reference` in the top-level `favorites` array of a provider file.
/// Returns the updated TOML text and whether `reference` is now favorited.
pub fn toggle_favorite(source: &str, reference: &str) -> Result<(String, bool), String> {
    let mut document = parse(source)?;
    let mut list: Vec<String> = document
        .get("favorites")
        .and_then(Item::as_array)
        .map(|array| {
            array
                .iter()
                .filter_map(|value| value.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let added = if let Some(position) = list.iter().position(|existing| existing == reference) {
        list.remove(position);
        false
    } else {
        list.push(reference.to_string());
        true
    };
    if list.is_empty() {
        document.as_table_mut().remove("favorites");
    } else {
        let mut array = toml_edit::Array::new();
        for item in &list {
            array.push(item.as_str());
        }
        document.as_table_mut().insert("favorites", value(array));
    }
    Ok((document.to_string(), added))
}

fn rename_provider_with_favorites(providers: &str, old: &str, new: &str) -> Result<String, String> {
    let mut document = parse(providers)?;
    rename_provider_entry(&mut document, old, new)?;
    let prefix = format!("{old}/");
    if let Some(favorites) = document.get_mut("favorites").and_then(Item::as_array_mut) {
        for entry in favorites.iter_mut() {
            if let Some(rest) = entry.as_str().and_then(|text| text.strip_prefix(&prefix)) {
                let decor = entry.decor().clone();
                let mut value = toml_edit::Value::from(format!("{new}/{rest}"));
                *value.decor_mut() = decor;
                *entry = value;
            }
        }
    }
    Ok(document.to_string())
}

pub fn rename_speech_provider(
    providers: &str,
    settings: &str,
    old: &str,
    new: &str,
) -> Result<(String, String), String> {
    let mut document = parse(providers)?;
    rename_provider_entry(&mut document, old, new)?;
    let mut settings = parse(settings)?;
    let prefix = format!("{old}/");
    if let Some(speech) = settings.get_mut("speech").and_then(Item::as_table_like_mut) {
        for key in ["asr", "tts"] {
            let next = speech.get(key).and_then(Item::as_str).and_then(|text| text.strip_prefix(&prefix))
                .map(|rest| format!("{new}/{rest}"));
            if let Some(next) = next {
                put(speech, key, &Field::Text(next));
            }
        }
    }
    Ok((document.to_string(), settings.to_string()))
}

fn rename_provider_entry(document: &mut DocumentMut, old: &str, new: &str) -> Result<(), String> {
    let table = document
        .get_mut("provider")
        .and_then(Item::as_array_of_tables_mut)
        .and_then(|tables| {
            tables.iter_mut().find(|table| table.get("id").and_then(Item::as_str) == Some(old))
        })
        .ok_or_else(|| format!("文件里没有 id 是 \"{old}\" 的 [[provider]]"))?;
    put(table, "id", &Field::Text(new.to_string()));
    Ok(())
}

/// Change `key` in `[table]`, creating the table when it is missing.
pub fn set_setting(source: &str, table: &str, key: &str, field: Field) -> Result<String, String> {
    let mut document = parse(source)?;
    if document.get(table).is_none() {
        document[table] = toml_edit::table();
    }
    let entries = document
        .get_mut(table)
        .and_then(Item::as_table_like_mut)
        .ok_or_else(|| format!("{table} 不是一个表"))?;
    put(entries, key, &field);
    Ok(document.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn editing_split_routes_preserves_unrelated_references() {
        let provider = "[[provider]]\nid = \"old\"\nbase_url = \"https://example.test\"\n";
        let image = format!("favorites = [\"old/org/image\", \"older/image\"]\n{provider}");
        let settings = "[models]\ndefault = \"old/chat\"\n[image]\ndefault = \"old/org/image\"\n[speech]\nasr = \"old/asr\"\ntts = \"older/tts\"\n";
        let (edited, settings) = rename_image_provider(&image, settings, "old", "new").unwrap();
        assert!(edited.contains("\"new/org/image\"") && edited.contains("\"older/image\""));
        assert!(settings.contains("[image]\ndefault = \"new/org/image\""));
        assert!(settings.contains("[models]\ndefault = \"old/chat\""));
        assert!(settings.contains("asr = \"old/asr\""));
        let (edited, settings) = rename_speech_provider(provider, &settings, "old", "new").unwrap();
        let edited = set_entry(&edited, "provider", "id", "new", &[
            ("speed", Field::Float(1.15)),
            ("style", Field::Float(0.0)),
            ("stability", Field::Float(0.34567)),
        ]).unwrap();
        assert!(edited.contains("speed = 1.15\n"));
        assert!(edited.contains("style = 0.0\n"));
        assert!(edited.contains("stability = 0.346\n"));
        let channels = crate::config::parse_speech_providers(&edited).unwrap();
        assert_eq!(channels[0].id, "new");
        assert!(settings.contains("asr = \"new/asr\""));
        assert!(settings.contains("tts = \"older/tts\""));
        assert!(settings.contains("[models]\ndefault = \"old/chat\""));
    }

    #[test]
    fn toggle_favorite_inserts_before_providers_and_clears_when_empty() {
        let source = "# header\n\n[[provider]]\nid = \"p1\"\nbase_url = \"https://example.test\"\nmodels = [\"m1\"]\n";
        let (added, now_fav) = toggle_favorite(source, "p1/m1").unwrap();
        assert!(now_fav);
        assert!(added.contains("# header"));
        assert_eq!(crate::config::parse_providers(&added).unwrap()[0].id, "p1");
        let parsed: toml::Value = toml::from_str(&added).unwrap();
        assert_eq!(parsed["favorites"].as_array().unwrap()[0].as_str(), Some("p1/m1"));
        let (removed, now_fav) = toggle_favorite(&added, "p1/m1").unwrap();
        assert!(!now_fav);
        assert!(!removed.contains("favorites"));
        assert_eq!(crate::config::parse_providers(&removed).unwrap()[0].id, "p1");
    }
    use crate::config::{parse_assistants, parse_settings};

    const ASSISTANTS: &str = "# mine\n[[assistant]]\nname = \"翻译\" # keep\nicon = \"译\"\n\n[[assistant]]\nname = \"润色\"\nmodel = \"a/b\"\n";

    #[test]
    fn entries_change_in_place() {
        let changed = set_entry(
            ASSISTANTS,
            "assistant",
            "name",
            "润色",
            &[
                ("model", Field::optional("  ")),
                ("enabled", Field::Flag(false)),
                ("icon", Field::Text("pen-line".into())),
            ],
        )
        .unwrap();
        assert!(changed.starts_with("# mine\n") && changed.contains("name = \"翻译\" # keep"));
        let assistants = parse_assistants(&changed).unwrap();
        assert_eq!(assistants[0].icon, "译");
        assert_eq!(assistants[1].icon, "pen-line");
        assert_eq!(assistants[1].model, None);
        assert!(!assistants[1].enabled);
        assert!(set_entry(ASSISTANTS, "assistant", "name", "没有", &[]).is_err());
        let listed = set_entry(ASSISTANTS, "assistant", "name", "翻译", &[("tools", Field::List(vec!["a".into(), "b".into()]))]).unwrap();
        assert_eq!(parse_assistants(&listed).unwrap()[0].tools, ["a", "b"]);
        let cleared = set_entry(&listed, "assistant", "name", "翻译", &[("tools", Field::List(Vec::new()))]).unwrap();
        assert!(parse_assistants(&cleared).unwrap()[0].tools.is_empty());
        assert!(set_entry("not toml [", "assistant", "name", "x", &[]).is_err());
    }

    #[test]
    fn entries_move_and_go() {
        let source = "# head\n[[assistant]]\nname = \"a\"\n\n# about b\n[[assistant]]\nname = \"b\"\n\n[[assistant]]\nname = \"c\"\n";
        let names = |text: &str| parse_assistants(text).unwrap().into_iter().map(|a| a.name).collect::<Vec<_>>();
        let swapped = reorder(source, "assistant", &[2, 0, 1]).unwrap();
        assert_eq!(names(&swapped), ["c", "a", "b"]);
        assert!(swapped.starts_with("# head\n"));
        assert!(swapped.contains("# about b\n[[assistant]]\nname = \"b\""));
        assert!(reorder(source, "assistant", &[0, 1]).is_err());
        assert_eq!(names(&remove_entry(source, "assistant", "name", "b").unwrap()), ["a", "c"]);
        assert!(remove_entry(source, "assistant", "name", "x").is_err());
        assert_eq!(moved(4, &[0], 2, true), [1, 2, 0, 3]);
        assert_eq!(moved(4, &[3], 1, false), [0, 3, 1, 2]);
        assert_eq!(moved(5, &[3, 4], 0, false), [3, 4, 0, 1, 2]);
        assert_eq!(moved(3, &[1], 1, false), [0, 1, 2]);
    }

    #[test]
    fn key_notes_write_as_inline_tables() {
        use crate::config::parse_providers;
        let source = "[[provider]]\nid = \"a\"\nbase_url = \"https://x\"\n";
        let notes = Field::Table(vec![
            vec![("note", Field::Text("公司".into()))],
            vec![("note", Field::Text("备用".into())), ("enabled", Field::Flag(false))],
            vec![],
        ]);
        let changed = set_entry(source, "provider", "id", "a", &[("keys", notes)]).unwrap();
        assert!(changed.contains("keys = [\n  { note = \"公司\" },\n  { note = \"备用\", enabled = false },\n  {},\n]"), "{changed}");
        let providers = parse_providers(&changed).unwrap();
        assert_eq!(providers[0].keys[0].note, "公司");
        assert!(providers[0].keys[1].note == "备用" && !providers[0].keys[1].enabled);
        assert!(providers[0].keys[2].note.is_empty() && providers[0].keys[2].enabled);
        let cleared = set_entry(&changed, "provider", "id", "a", &[("keys", Field::Table(Vec::new()))]).unwrap();
        assert!(!cleared.contains("keys"));
    }

    #[test]
    fn provider_renames_everywhere() {
        use crate::config::parse_providers;
        let providers = "# fav\nfavorites = [\n  \"wolf/a\",\n  \"wolfpack/x\",\n]\n\n[[provider]]\nid = \"wolf\" # mine\nbase_url = \"https://x\"\n\n[[provider]]\nid = \"wolfpack\"\nbase_url = \"https://y\"\n";
        let assistants = "[[assistant]]\nname = \"翻译\"\nmodel = \"wolf/a\" # keep\n\n[[assistant]]\nname = \"润色\"\nmodel = \"wolfpack/x\"\n\n[[assistant]]\nname = \"默认\"\nmodel = \"default\"\n";
        let settings = "[models]\ndefault = \"wolf/a\" # tier\nfast = \"wolfpack/x\"\nnone = \"cat/y\"\n";
        let (providers, assistants, settings) =
            rename_provider(providers, assistants, settings, "wolf", "fox").unwrap();
        assert!(providers.contains("id = \"fox\" # mine") && providers.contains("id = \"wolfpack\""));
        assert!(providers.contains("\"fox/a\"") && providers.contains("\"wolfpack/x\""));
        assert!(providers.starts_with("# fav\n"));
        assert_eq!(parse_providers(&providers).unwrap()[0].id, "fox");
        assert!(assistants.contains("model = \"fox/a\" # keep"));
        assert!(assistants.contains("model = \"wolfpack/x\"") && assistants.contains("model = \"default\""));
        assert!(settings.contains("default = \"fox/a\" # tier") && settings.contains("fast = \"wolfpack/x\"") && settings.contains("none = \"cat/y\""));
        assert!(rename_provider(&providers, &assistants, &settings, "wolf", "fox").is_err());
    }

    #[test]
    fn settings_change_in_place() {
        let source = "# top\n[gesture]\nhold_ms = 350 # ms\n";
        let changed = set_setting(source, "gesture", "hold_ms", Field::Number(180)).unwrap();
        assert!(changed.contains("# top") && changed.contains("# ms"));
        assert_eq!(parse_settings(&changed).unwrap().gesture.hold_ms, 180);
        let added = set_setting(&changed, "hotkey", "panel", Field::Text("Ctrl+Alt+A".into())).unwrap();
        assert_eq!(parse_settings(&added).unwrap().hotkey.panel, "Ctrl+Alt+A");
        let removed = set_setting(&added, "hotkey", "panel", Field::Absent).unwrap();
        assert_eq!(parse_settings(&removed).unwrap().hotkey.panel, "Alt+Space");
    }

    #[test]
    fn maps_write_as_inline_tables() {
        let source = "[[server]]\nname = \"a\"\ncommand = \"x\"\n";
        let env = Field::Map(vec![("HTTP_PROXY".into(), "http://127.0.0.1:1".into()), ("B C".into(), "2".into())]);
        let changed = set_entry(source, "server", "name", "a", &[("env", env)]).unwrap();
        let servers = crate::mcp::parse_servers(&changed).unwrap();
        assert_eq!(servers[0].env["HTTP_PROXY"], "http://127.0.0.1:1");
        assert_eq!(servers[0].env["B C"], "2");
        let cleared = set_entry(&changed, "server", "name", "a", &[("env", Field::Map(Vec::new()))]).unwrap();
        assert!(!cleared.contains("env"));
    }

    #[test]
    fn reorder_keeps_nested_subtables_with_their_parent() {
        let source = "\
[[provider]]
id = \"first\"
base_url = \"https://first.invalid\"

[provider.headers]
X-Owner = \"first\"

[[provider.keys]]
note = \"k1\"

[[provider]]
id = \"second\"
base_url = \"https://second.invalid\"

[provider.headers]
X-Owner = \"second\"

[[provider.keys]]
note = \"k2\"
";
        let moved = reorder(source, "provider", &[1, 0]).unwrap();
        let providers = crate::config::parse_image_providers(&moved).unwrap();
        assert_eq!(providers[0].id, "second");
        assert_eq!(providers[0].headers.get("X-Owner").map(String::as_str), Some("second"));
        assert_eq!(providers[0].keys[0].note, "k2");
        assert_eq!(providers[1].id, "first");
        assert_eq!(providers[1].headers.get("X-Owner").map(String::as_str), Some("first"));
        assert_eq!(providers[1].keys[0].note, "k1");
    }
}
