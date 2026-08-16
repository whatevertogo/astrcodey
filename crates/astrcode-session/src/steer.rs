//! Mid-turn 用户输入的 accepted→absorbed 对账。
//!
//! 调度层（`TurnScheduler::inject_internal` / queue）接受输入时只写 durable
//! `UserInputAccepted`（steering 输入的信封归属活跃 `turn_id`），不进 transcript。
//! turn 在每个 agent step 边界（上一轮工具结果已配对落盘之后）把归属自己的 accepted
//! 输入按 `accepted_seq` 顺序吸收为 `UserMessage { accepted_seq: Some(..) }`，projection
//! 据此将条目移出 `pending_inputs`。归属给其它（已结束）turn 的遗留条目留在队列，
//! 由 queue 链路 FIFO 启动为新 turn。

use astrcode_core::types::TurnId;
use astrcode_session_projection::{PendingInput, SessionReadModel};

/// 归属本 turn、尚未吸收的 accepted 输入。
///
/// `pending_inputs` 按 durable seq 追加，迭代序即 `accepted_seq` 升序。
pub(crate) fn absorbable_inputs_for_turn<'a>(
    model: &'a SessionReadModel,
    turn_id: &TurnId,
) -> impl Iterator<Item = &'a PendingInput> {
    model
        .execution
        .pending_inputs
        .iter()
        .filter(move |input| input.turn_id.as_ref() == Some(turn_id))
}

#[cfg(test)]
mod tests {
    use astrcode_core::{
        event::{DurableEvent, DurableEventPayload, StoredEvent},
        types::{SessionId, new_turn_id},
        user_input::UserInput,
    };
    use astrcode_session_projection::reduce;

    use super::*;
    use crate::test_support::read_model;

    #[test]
    fn absorbable_inputs_match_only_the_owning_turn() {
        let session_id = SessionId::new("s-steer");
        let turn_a = new_turn_id();
        let turn_b = new_turn_id();
        let mut model = read_model(session_id.clone());
        let accepted = [
            (Some(turn_a.clone()), "for a"),
            (None, "queued"),
            (Some(turn_b), "for b"),
            (Some(turn_a.clone()), "for a again"),
        ];
        for (index, (turn_id, text)) in accepted.into_iter().enumerate() {
            let event = StoredEvent::new(
                index as u64 + 1,
                DurableEvent::new(
                    session_id.clone(),
                    turn_id,
                    DurableEventPayload::UserInputAccepted {
                        input: UserInput::text_only(text),
                    },
                ),
            );
            reduce(&event, &mut model).unwrap();
        }

        let absorbable: Vec<_> = absorbable_inputs_for_turn(&model, &turn_a)
            .map(|input| (input.accepted_seq, input.input.text.as_str()))
            .collect();
        assert_eq!(absorbable, [(1, "for a"), (4, "for a again")]);
        assert!(
            absorbable_inputs_for_turn(&model, &new_turn_id())
                .next()
                .is_none()
        );
    }
}
