//! Wording shared by messages.

/// `n` followed by `noun`, in the plural unless `n` is one: `1 item`,
/// `3 items`, `2 copies`.
pub fn count(n: usize, noun: &str) -> String {
    if n == 1 {
        return format!("1 {noun}");
    }
    match noun.strip_suffix('y') {
        Some(stem) if !stem.ends_with(['a', 'e', 'i', 'o', 'u']) => format!("{n} {stem}ies"),
        _ => format!("{n} {noun}s"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_agree_with_their_noun() {
        assert_eq!(count(1, "item"), "1 item");
        assert_eq!(count(0, "item"), "0 items");
        assert_eq!(count(2, "attachment file"), "2 attachment files");
        assert_eq!(count(2, "copy"), "2 copies");
        assert_eq!(count(2, "key"), "2 keys");
    }
}
