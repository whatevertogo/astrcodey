// TODO: Implement Trie struct here

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_insert_and_search() {
        let mut trie = Trie::new();
        trie.insert("apple");
        trie.insert("app");
        trie.insert("banana");

        assert!(trie.search("apple"));
        assert!(trie.search("app"));
        assert!(trie.search("banana"));
        assert!(!trie.search("ap"));
        assert!(!trie.search("ban"));
        assert!(!trie.search("orange"));
    }

    #[test]
    fn test_starts_with() {
        let mut trie = Trie::new();
        trie.insert("hello");
        trie.insert("help");
        trie.insert("world");

        assert!(trie.starts_with("hel"));
        assert!(trie.starts_with("hello"));
        assert!(trie.starts_with("wor"));
        assert!(!trie.starts_with("xyz"));
        assert!(!trie.starts_with("helloo"));
    }

    #[test]
    fn test_words_with_prefix() {
        let mut trie = Trie::new();
        trie.insert("car");
        trie.insert("card");
        trie.insert("care");
        trie.insert("careful");
        trie.insert("dog");

        let mut words = trie.words_with_prefix("car");
        words.sort();
        assert_eq!(words, vec!["car", "card", "care", "careful"]);

        let words = trie.words_with_prefix("dog");
        assert_eq!(words, vec!["dog"]);

        let words = trie.words_with_prefix("cat");
        assert!(words.is_empty());
    }

    #[test]
    fn test_empty_trie() {
        let trie = Trie::new();
        assert!(!trie.search("anything"));
        assert!(!trie.starts_with("a"));
        assert!(trie.words_with_prefix("").is_empty());
    }

    #[test]
    fn test_empty_string() {
        let mut trie = Trie::new();
        trie.insert("");
        assert!(trie.search(""));
        assert!(!trie.search("a"));
    }

    #[test]
    fn test_unicode() {
        let mut trie = Trie::new();
        trie.insert("你好");
        trie.insert("你好世界");

        assert!(trie.search("你好"));
        assert!(trie.starts_with("你"));
        assert!(!trie.search("你"));

        let words = trie.words_with_prefix("你好");
        assert_eq!(words.len(), 2);
    }
}
