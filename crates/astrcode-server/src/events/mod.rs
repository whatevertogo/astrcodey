//! 客户端事件发布层。
//!
//! `ClientEventPublisher` 是一个薄包装，把通知投递到广播 channel；
//! durable event 由 `SessionActor` 在调用 publish 之前先写入 EventStore。

mod publisher;

pub use publisher::ClientEventPublisher;
