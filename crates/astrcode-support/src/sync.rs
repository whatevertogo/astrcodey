//! 进程内锁的统一恢复策略。
//!
//! 互斥锁中毒时记录错误并恢复内部状态（[`std::sync::Mutex::into_inner`] /
//! [`parking_lot::Mutex::into_inner`]），避免调用方在 `.ok()` 与 `.into_inner()` 之间各自为政。

use std::sync::{Mutex as StdMutex, MutexGuard as StdMutexGuard};

use parking_lot::{Mutex as ParkingMutex, MutexGuard as ParkingMutexGuard};

/// 获取 [`std::sync::Mutex`] 守卫；中毒时记录并恢复。
pub fn lock_std<T>(mutex: &StdMutex<T>) -> StdMutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => {
            tracing::error!("std::sync::Mutex poisoned; recovering inner state");
            poisoned.into_inner()
        },
    }
}

/// 获取 [`parking_lot::Mutex`] 守卫。
pub fn lock_parking<T>(mutex: &ParkingMutex<T>) -> ParkingMutexGuard<'_, T> {
    mutex.lock()
}
