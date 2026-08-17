//! Session 级工具可见性策略。

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

/// Session 的工具可见性策略。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum SessionToolSelection {
    All { except: Vec<String> },
    Only { names: Vec<String> },
}

impl Default for SessionToolSelection {
    fn default() -> Self {
        Self::All { except: Vec::new() }
    }
}

impl SessionToolSelection {
    pub fn allows(&self, tool_name: &str) -> bool {
        match self {
            Self::All { except } => !except.iter().any(|name| name == tool_name),
            Self::Only { names } => names.iter().any(|name| name == tool_name),
        }
    }

    pub fn normalized(&self) -> Self {
        match self {
            Self::All { except } => Self::All {
                except: normalized_tool_names(except),
            },
            Self::Only { names } => Self::Only {
                names: normalized_tool_names(names),
            },
        }
    }

    pub fn intersection(&self, other: &Self) -> Self {
        match (self, other) {
            (Self::All { except: current }, Self::All { except: other }) => Self::All {
                except: current
                    .iter()
                    .chain(other)
                    .cloned()
                    .collect::<BTreeSet<_>>()
                    .into_iter()
                    .collect(),
            },
            (Self::All { except }, Self::Only { names })
            | (Self::Only { names }, Self::All { except }) => {
                let excluded = except.iter().collect::<BTreeSet<_>>();
                Self::Only {
                    names: names
                        .iter()
                        .filter(|name| !excluded.contains(name))
                        .cloned()
                        .collect::<BTreeSet<_>>()
                        .into_iter()
                        .collect(),
                }
            },
            (Self::Only { names: current }, Self::Only { names: other }) => {
                let other = other.iter().collect::<BTreeSet<_>>();
                Self::Only {
                    names: current
                        .iter()
                        .filter(|name| other.contains(name))
                        .cloned()
                        .collect::<BTreeSet<_>>()
                        .into_iter()
                        .collect(),
                }
            },
        }
    }

    pub fn restrict(parent: Option<&Self>, requested: &Self) -> Self {
        parent.map_or_else(
            || requested.normalized(),
            |parent| parent.intersection(requested),
        )
    }

    pub fn intersect(parent: Option<&Self>, requested: Option<&Self>) -> Option<Self> {
        match (parent, requested) {
            (None, None) => None,
            (Some(parent), None) => Some(parent.normalized()),
            (parent, Some(requested)) => Some(Self::restrict(parent, requested)),
        }
    }
}

fn normalized_tool_names(tools: &[String]) -> Vec<String> {
    tools
        .iter()
        .cloned()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// 工具名 trim 后为空的校验失败。各输入边界(HTTP、host invoke)把该错误映射为
/// 各自契约的错误消息,`Display` 仅作为默认可读消息。
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("tool names must not be empty")]
pub struct EmptyToolNameError;

/// 外部输入边界的工具名校验:逐项 trim、拒绝空名、去重并排序。
pub fn validated_tool_names(tools: Vec<String>) -> Result<Vec<String>, EmptyToolNameError> {
    let tools = tools
        .into_iter()
        .map(|tool| {
            let tool = tool.trim();
            if tool.is_empty() {
                Err(EmptyToolNameError)
            } else {
                Ok(tool.to_owned())
            }
        })
        .collect::<Result<BTreeSet<_>, _>>()?;
    Ok(tools.into_iter().collect())
}

#[cfg(test)]
mod tests {
    use super::SessionToolSelection;

    #[test]
    fn intersection_preserves_parent_boundary() {
        let all_except_a = SessionToolSelection::All {
            except: vec!["a".into()],
        };
        let all_except_b = SessionToolSelection::All {
            except: vec!["b".into()],
        };
        let only_ab = SessionToolSelection::Only {
            names: vec!["a".into(), "b".into()],
        };
        let only_bc = SessionToolSelection::Only {
            names: vec!["b".into(), "c".into()],
        };

        assert_eq!(SessionToolSelection::intersect(None, None), None);
        assert_eq!(
            SessionToolSelection::intersect(Some(&all_except_a), None),
            Some(all_except_a.clone())
        );
        assert_eq!(
            SessionToolSelection::intersect(Some(&all_except_a), Some(&all_except_b)),
            Some(SessionToolSelection::All {
                except: vec!["a".into(), "b".into()]
            })
        );
        assert_eq!(
            SessionToolSelection::intersect(Some(&all_except_a), Some(&only_ab)),
            Some(SessionToolSelection::Only {
                names: vec!["b".into()]
            })
        );
        assert_eq!(
            SessionToolSelection::intersect(Some(&only_ab), Some(&all_except_b)),
            Some(SessionToolSelection::Only {
                names: vec!["a".into()]
            })
        );
        assert_eq!(
            SessionToolSelection::intersect(Some(&only_ab), Some(&only_bc)),
            Some(SessionToolSelection::Only {
                names: vec!["b".into()]
            })
        );
        assert_eq!(
            only_ab.intersection(&SessionToolSelection::Only {
                names: vec!["c".into()]
            }),
            SessionToolSelection::Only { names: Vec::new() }
        );
        assert_eq!(
            SessionToolSelection::intersect(
                None,
                Some(&SessionToolSelection::Only {
                    names: vec!["b".into(), "a".into(), "b".into()]
                })
            ),
            Some(SessionToolSelection::Only {
                names: vec!["a".into(), "b".into()]
            })
        );
        assert!(only_ab.allows("a"));
        assert!(!only_ab.allows("c"));
        assert!(!all_except_a.allows("a"));
        assert!(all_except_a.allows("b"));
    }

    #[test]
    fn validated_tool_names_trim_reject_empty_and_dedup() {
        use super::validated_tool_names;

        assert_eq!(
            validated_tool_names(vec![" b ".into(), "a".into(), "b".into()]).expect("valid names"),
            vec!["a".to_string(), "b".to_string()]
        );
        for names in [
            vec![String::new()],
            vec!["   ".into()],
            vec!["a".into(), " ".into()],
        ] {
            assert!(validated_tool_names(names).is_err());
        }
    }
}
