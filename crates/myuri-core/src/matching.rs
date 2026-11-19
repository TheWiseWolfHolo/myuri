//! Type-to-filter matching for assistant names: by the name itself, by pinyin
//! initials (`fy` finds 翻译) and by full pinyin (`fanyi`).

use pinyin::{ToPinyin, ToPinyinMulti};

/// Search keys derived once from a name.
#[derive(Clone, Debug)]
pub struct Keys {
    /// Lowercased name without spaces or punctuation.
    name: String,
    /// Plain pinyin of each character joined together, using the most common
    /// reading. Non-Chinese characters stand for themselves.
    full: String,
    /// Offset in `full` where each character's pinyin begins.
    starts: Vec<usize>,
    /// Possible initials of each character. Characters with several readings
    /// list every initial, so 重 matches both `z` and `c`.
    initials: Vec<Vec<char>>,
}

impl Keys {
    pub fn new(name: &str) -> Self {
        let mut keys = Self {
            name: String::new(),
            full: String::new(),
            starts: Vec::new(),
            initials: Vec::new(),
        };
        for ch in name.chars().filter(|c| c.is_alphanumeric()) {
            let lower: String = ch.to_lowercase().collect();
            keys.name.push_str(&lower);
            keys.starts.push(keys.full.len());
            match ch.to_pinyin() {
                Some(reading) => {
                    keys.full.push_str(reading.plain());
                    let mut set: Vec<char> = ch
                        .to_pinyin_multi()
                        .into_iter()
                        .flatten()
                        .filter_map(|r| r.first_letter().chars().next())
                        .collect();
                    set.sort_unstable();
                    set.dedup();
                    keys.initials.push(set);
                }
                None => {
                    keys.full.push_str(&lower);
                    keys.initials.push(lower.chars().collect());
                }
            }
        }
        keys
    }

    /// How well `query` matches; lower is better, `None` is no match. The
    /// query must already be normalized with [`normalize`].
    pub fn score(&self, query: &str) -> Option<u8> {
        if query.is_empty() {
            return Some(0);
        }
        let initials = self.initials_position(query);
        // Full pinyin only matches from the start of a character, otherwise
        // "oc" would find 报错 through ba(o c)uo.
        let full = self.starts.iter().position(|&at| self.full[at..].starts_with(query));
        if self.name.starts_with(query) {
            Some(0)
        } else if initials == Some(0) {
            Some(1)
        } else if full == Some(0) {
            Some(2)
        } else if self.name.contains(query) {
            Some(3)
        } else if initials.is_some() {
            Some(4)
        } else if full.is_some() {
            Some(5)
        } else {
            None
        }
    }

    /// First position where every query character is a possible initial of the
    /// corresponding name character.
    fn initials_position(&self, query: &str) -> Option<usize> {
        let query: Vec<char> = query.chars().collect();
        if query.len() > self.initials.len() {
            return None;
        }
        (0..=self.initials.len() - query.len()).find(|&start| {
            query.iter().zip(&self.initials[start..]).all(|(q, set)| set.contains(q))
        })
    }
}

/// Lowercase and drop everything that is not a letter or digit.
pub fn normalize(query: &str) -> String {
    query.chars().filter(|c| c.is_alphanumeric()).flat_map(char::to_lowercase).collect()
}

/// Indices of the matching entries, best match first; ties keep their
/// original order.
pub fn rank<'a>(keys: impl IntoIterator<Item = &'a Keys>, query: &str) -> Vec<usize> {
    let query = normalize(query);
    let mut hits: Vec<(u8, usize)> = keys
        .into_iter()
        .enumerate()
        .filter_map(|(index, keys)| keys.score(&query).map(|score| (score, index)))
        .collect();
    hits.sort();
    hits.into_iter().map(|(_, index)| index).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ranked(names: &[&str], query: &str) -> Vec<String> {
        let keys: Vec<Keys> = names.iter().map(|n| Keys::new(n)).collect();
        rank(&keys, query).into_iter().map(|i| names[i].to_string()).collect()
    }

    #[test]
    fn empty_query_keeps_everything_in_order() {
        assert_eq!(ranked(&["翻译", "润色", "OCR"], ""), ["翻译", "润色", "OCR"]);
        assert_eq!(ranked(&["翻译", "润色"], "  "), ["翻译", "润色"]);
    }

    #[test]
    fn matches_initials_full_pinyin_and_name() {
        let names = ["翻译", "润色", "解释报错", "图片翻译", "OCR", "AI 写作"];
        assert_eq!(ranked(&names, "fy"), ["翻译", "图片翻译"]);
        assert_eq!(ranked(&names, "fanyi"), ["翻译", "图片翻译"]);
        assert_eq!(ranked(&names, "翻"), ["翻译", "图片翻译"]);
        assert_eq!(ranked(&names, "ocr"), ["OCR"]);
        assert_eq!(ranked(&names, "OC"), ["OCR"]);
        assert_eq!(ranked(&names, "baocuo"), ["解释报错"]);
        assert_eq!(ranked(&names, "shibao"), ["解释报错"]);
        assert_eq!(ranked(&names, "jsbc"), ["解释报错"]);
        // Spaces in the name or the query do not matter.
        assert_eq!(ranked(&names, "aixz"), ["AI 写作"]);
        assert_eq!(ranked(&names, "ai xie"), ["AI 写作"]);
        assert!(ranked(&names, "zzz").is_empty());
    }

    #[test]
    fn prefix_matches_come_first() {
        // "s" is the initial of 色 in 润色 (middle) and of 设 in 设置 (start).
        assert_eq!(ranked(&["润色", "设置"], "s"), ["设置", "润色"]);
    }

    #[test]
    fn any_reading_of_a_character_matches() {
        let names = ["重写", "长文总结"];
        assert_eq!(ranked(&names, "cx"), ["重写"]);
        assert_eq!(ranked(&names, "zx"), ["重写"]);
        assert_eq!(ranked(&names, "cwzj"), ["长文总结"]);
        assert_eq!(ranked(&names, "zwzj"), ["长文总结"]);
    }
}
