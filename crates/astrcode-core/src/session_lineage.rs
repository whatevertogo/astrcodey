//! Session 谱系(parent chain)遍历原语。
//!
//! host router 的 history 可见性过滤与 server 的 lineage 鉴权共用同一条带环检测的
//! 父链上溯;各调用点注入自己的 parent 解析策略(内存表、active/recycled 存储读),
//! 错误码与错误消息由调用方在各自边界映射。

use std::{collections::HashSet, future::Future};

use crate::types::SessionId;

/// 父链上溯时检测到环(session 元数据损坏)。`Display` 消息是线缆契约的一部分,
/// host/server 两侧的历史实现均逐字使用该消息,不得改动。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("session parent chain contains a cycle at {session_id}")]
pub struct ParentChainCycle {
    pub session_id: SessionId,
}

/// [`collect_parent_chain`] 的失败:环(元数据损坏)或 parent 解析失败(调用方错误类型原样返回)。
#[derive(Debug)]
pub enum ParentChainWalkError<E> {
    Cycle(ParentChainCycle),
    Resolve(E),
}

/// 从 `start`(含)沿 parent 链上溯,返回按访问顺序排列的链(起点在首、根在尾)。
///
/// 每一跳先查重再解析 parent:重复访问即环,立即报错;`parent_of` 返回 `None` 表示到达根。
pub async fn collect_parent_chain<E, F, Fut>(
    start: &SessionId,
    mut parent_of: F,
) -> Result<Vec<SessionId>, ParentChainWalkError<E>>
where
    F: FnMut(SessionId) -> Fut,
    Fut: Future<Output = Result<Option<SessionId>, E>>,
{
    let mut chain = Vec::new();
    let mut visited = HashSet::new();
    let mut current = start.clone();
    loop {
        if !visited.insert(current.clone()) {
            return Err(ParentChainWalkError::Cycle(ParentChainCycle {
                session_id: current,
            }));
        }
        chain.push(current.clone());
        let Some(parent) = parent_of(current)
            .await
            .map_err(ParentChainWalkError::Resolve)?
        else {
            break;
        };
        current = parent;
    }
    Ok(chain)
}

#[cfg(test)]
mod tests {
    use std::convert::Infallible;

    use super::*;

    fn parents(
        edges: &[(&str, Option<&str>)],
    ) -> impl FnMut(SessionId) -> std::future::Ready<Result<Option<SessionId>, Infallible>> {
        let edges: Vec<(SessionId, Option<SessionId>)> = edges
            .iter()
            .map(|(child, parent)| (SessionId::new(*child), parent.map(SessionId::new)))
            .collect();
        move |current: SessionId| {
            let parent = edges
                .iter()
                .find(|(child, _)| *child == current)
                .and_then(|(_, parent)| parent.clone());
            std::future::ready(Ok(parent))
        }
    }

    #[tokio::test]
    async fn walks_parent_chains_and_reports_failures() {
        let chain = collect_parent_chain(
            &SessionId::new("grandchild"),
            parents(&[
                ("root", None),
                ("child", Some("root")),
                ("grandchild", Some("child")),
            ]),
        )
        .await
        .expect("acyclic chain");
        assert_eq!(
            chain,
            vec![
                SessionId::new("grandchild"),
                SessionId::new("child"),
                SessionId::new("root"),
            ]
        );

        // 不在表中的 session 视为无 parent(与 history_list 的内存表语义一致)。
        let chain = collect_parent_chain(&SessionId::new("unknown"), parents(&[]))
            .await
            .expect("missing entry is a root");
        assert_eq!(chain, vec![SessionId::new("unknown")]);

        let error = collect_parent_chain(
            &SessionId::new("a"),
            parents(&[("a", Some("b")), ("b", Some("a"))]),
        )
        .await
        .expect_err("cycle must be rejected");
        let cycle = match error {
            ParentChainWalkError::Cycle(cycle) => cycle,
            ParentChainWalkError::Resolve(error) => match error {},
        };
        assert_eq!(
            cycle.to_string(),
            "session parent chain contains a cycle at a"
        );

        let error = collect_parent_chain(&SessionId::new("a"), parents(&[("a", Some("a"))]))
            .await
            .expect_err("self cycle must be rejected");
        assert!(matches!(error, ParentChainWalkError::Cycle(_)));

        let error = collect_parent_chain(&SessionId::new("a"), |_: SessionId| async {
            Err::<Option<SessionId>, _>("read failed")
        })
        .await
        .expect_err("resolver error must propagate");
        let ParentChainWalkError::Resolve(error) = error else {
            panic!("expected resolver error");
        };
        assert_eq!(error, "read failed");
    }
}
