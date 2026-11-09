use std::sync::Arc;

use anyhow::anyhow;
use mqb::support::plugin_abi::{MqbStatus, MQB_OK};
use mqb::{Caller, Sent};
use tokio::runtime::Runtime;

use crate::message::mqb_message_t;
use crate::route::mqb_route_t;
use crate::{guard, guard_new, handle, set_error, status_of};

/// Calls a route like a function. Safe to use from several threads; calls made at
/// the same time reach the route as one batch.
pub struct mqb_caller_t {
    runtime: Arc<Runtime>,
    caller: Caller,
}

/// Deploys `route` (with its handlers) under its name and returns a caller for it.
/// The route input must be an in-process `memory` endpoint. Do not also start the
/// route. Null on error.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mqb_caller_from_route(route: *const mqb_route_t) -> *mut mqb_caller_t {
    guard_new(|| {
        let (runtime, route, name) = unsafe { handle(route) }?.for_caller()?;
        let caller = runtime.block_on(Caller::deploy(&name, route))?;
        Ok(mqb_caller_t { runtime, caller })
    })
}

/// Sends `message` through the route and stores its answer in `*response` (free it
/// with `mqb_message_free`; null on failure). A failed message returns its error.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mqb_caller_call(
    caller: *const mqb_caller_t,
    message: *const mqb_message_t,
    response: *mut *mut mqb_message_t,
) -> MqbStatus {
    deliver(caller, message, response, true)
}

/// Like `mqb_caller_call`, but a route that accepts the message without an answer
/// succeeds with `*response` null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mqb_caller_send(
    caller: *const mqb_caller_t,
    message: *const mqb_message_t,
    response: *mut *mut mqb_message_t,
) -> MqbStatus {
    deliver(caller, message, response, false)
}

/// Sends `count` messages through the route as one batch. For message `i`,
/// `statuses[i]` gets its status and `responses[i]` its answer, or on failure a message
/// whose payload is the error text (free each with `mqb_message_free`). Returns `MQB_OK`
/// if all succeeded, else the first failure's status, its text also in `mqb_last_error`.
/// A call that fails before sending sets every status to its error and every response null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mqb_caller_call_many(
    caller: *const mqb_caller_t,
    messages: *const *const mqb_message_t,
    count: usize,
    responses: *mut *mut mqb_message_t,
    statuses: *mut MqbStatus,
) -> MqbStatus {
    let mut first_failure = None;
    let status = guard(|| {
        if count > 0 && (messages.is_null() || responses.is_null() || statuses.is_null()) {
            return Err(anyhow!("null array"));
        }
        let caller = unsafe { handle(caller) }?;
        let requests = (0..count)
            .map(|i| Ok(unsafe { handle(*messages.add(i)) }?.inner.clone()))
            .collect::<anyhow::Result<Vec<_>>>()?;
        let answers = caller
            .runtime
            .block_on(caller.caller.call_many_messages(requests));
        for (i, answer) in answers.into_iter().enumerate() {
            let (response, status) = match answer {
                Ok(answer) => (answer, MQB_OK),
                Err(err) => {
                    let status = status_of(&err);
                    let text = format!("{err:#}");
                    first_failure.get_or_insert((status, text.clone()));
                    (text.into(), status)
                }
            };
            unsafe {
                *responses.add(i) = Box::into_raw(Box::new(mqb_message_t::wrap(response)));
                *statuses.add(i) = status;
            }
        }
        Ok(())
    });
    if status != MQB_OK && !responses.is_null() && !statuses.is_null() {
        for i in 0..count {
            unsafe {
                *responses.add(i) = std::ptr::null_mut();
                *statuses.add(i) = status;
            }
        }
    }
    match first_failure {
        Some((failed, text)) if status == MQB_OK => {
            set_error(text);
            failed
        }
        _ => status,
    }
}

fn deliver(
    caller: *const mqb_caller_t,
    message: *const mqb_message_t,
    response: *mut *mut mqb_message_t,
    needs_answer: bool,
) -> MqbStatus {
    guard(|| {
        if response.is_null() {
            return Err(anyhow!("null output pointer"));
        }
        unsafe { *response = std::ptr::null_mut() };
        let caller = unsafe { handle(caller) }?;
        let message = unsafe { handle(message) }?.inner.clone();
        let answer = if needs_answer {
            Some(
                caller
                    .runtime
                    .block_on(caller.caller.call_message(message))?,
            )
        } else {
            match caller.runtime.block_on(caller.caller.send(message))? {
                Sent::Ack => None,
                Sent::Response(answer) => Some(answer),
            }
        };
        if let Some(answer) = answer {
            unsafe { *response = Box::into_raw(Box::new(mqb_message_t::wrap(answer))) };
        }
        Ok(())
    })
}

/// Stops the route and frees the caller.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mqb_caller_free(caller: *mut mqb_caller_t) {
    if !caller.is_null() {
        let caller = unsafe { Box::from_raw(caller) };
        caller.runtime.block_on(caller.caller.stop());
    }
}
