// Copyright 2026-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

use std::any::Any;
use std::backtrace::Backtrace;
use std::cell::RefCell;
use std::panic;
use std::sync::Once;

pub struct PanicDetails {
    pub message: String,
    pub location: Option<String>,
    pub backtrace: Backtrace,
}

thread_local! {
    static LAST: RefCell<Option<PanicDetails>> = const { RefCell::new(None) };
}

/// Records the details of every panic on the panicking thread, then runs the previous hook.
pub fn install_hook() {
    static INSTALL: Once = Once::new();
    INSTALL.call_once(|| {
        let previous = panic::take_hook();
        panic::set_hook(Box::new(move |info| {
            let details = PanicDetails {
                message: payload_message(info.payload()),
                location: info
                    .location()
                    .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column())),
                backtrace: Backtrace::force_capture(),
            };
            LAST.with(|last| *last.borrow_mut() = Some(details));
            previous(info);
        }));
    });
}

pub fn clear() {
    LAST.with(|last| *last.borrow_mut() = None);
}

pub fn take() -> Option<PanicDetails> {
    LAST.with(|last| last.borrow_mut().take())
}

pub fn payload_message(payload: &(dyn Any + Send)) -> String {
    payload_str(payload)
        .map(str::to_string)
        .unwrap_or_else(|| "Box<dyn Any>".to_string())
}

pub fn payload_str(payload: &(dyn Any + Send)) -> Option<&str> {
    payload
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| payload.downcast_ref::<&'static str>().copied())
}
