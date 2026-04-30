//! 模板引擎，支持 `{{variable}}` 语法。
//!
//! 变量替换按 key 长度降序排列。

use std::{cmp::Reverse, collections::BTreeMap};

pub struct PromptTemplate;

impl PromptTemplate {
    pub fn render(template: &str, vars: &BTreeMap<String, String>) -> String {
        let mut result = template.to_string();
        let mut replacements: Vec<(String, &str)> = vars
            .iter()
            .map(|(k, v)| (format!("{{{{{}}}}}", k), v.as_str()))
            .collect();
        replacements.sort_by_key(|(key, _)| Reverse(key.len()));
        for (key, value) in replacements {
            result = result.replace(&key, value);
        }
        result
    }
}
