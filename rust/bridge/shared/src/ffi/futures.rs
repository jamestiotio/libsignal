//
// Copyright 2023 Signal Messenger, LLC.
// SPDX-License-Identifier: AGPL-3.0-only
//

use super::*;
use crate::support::AsyncRuntime;

use futures_util::{FutureExt, TryFutureExt};

use std::future::Future;

/// A C callback used to report the results of Rust futures.
///
/// cbindgen will produce independent C types like `SignalCPromisei32` and
/// `SignalCPromiseProtocolAddress`.
pub type CPromise<T> =
    extern "C" fn(error: *mut SignalFfiError, result: *const T, context: *const libc::c_void);

/// Keeps track of the information necessary to report a promise result back to C.
///
/// Because this represents a C callback that might be holding on to resources, users of
/// PromiseCompleter *must* consume it by calling [`complete`][Self::complete]. Failure to do so
/// will result in a panic in debug mode and an error log in release mode.
pub struct PromiseCompleter<T: ResultTypeInfo> {
    promise: CPromise<T::ResultType>,
    promise_context: *const libc::c_void,
}

/// Pointers are not Send by default just in case,
/// but we just pass the promise context around opaquely without using it from Rust.
///
/// Of course, the C code has to handle the pointer being sent across threads too.
unsafe impl<T: ResultTypeInfo> Send for PromiseCompleter<T> {}

impl<T: ResultTypeInfo + std::panic::UnwindSafe> PromiseCompleter<T> {
    /// Report a result to C.
    pub fn complete(self, result: SignalFfiResult<T>) {
        let Self {
            promise,
            promise_context,
        } = self;
        // Disable the check for uncompleted promises in our Drop before we do anything else.
        std::mem::forget(self);

        let result = result.and_then(|result| {
            std::panic::catch_unwind(|| result.convert_into())
                .unwrap_or_else(|panic| Err(SignalFfiError::UnexpectedPanic(panic)))
        });

        match result {
            Ok(value) => promise(std::ptr::null_mut(), &value, promise_context),
            Err(err) => promise(
                Box::into_raw(Box::new(err)),
                std::ptr::null(),
                promise_context,
            ),
        }
    }
}

impl<T: ResultTypeInfo> Drop for PromiseCompleter<T> {
    fn drop(&mut self) {
        // Dropping a promise likely leaks resources on the other side of the bridge (whatever's in
        // promise_context). It won't bring down the application, but it definitely indicates a bug.
        debug_assert!(false, "CPromise dropped without completing");
        log::error!(
            "CPromise<{}> dropped without completing",
            std::any::type_name::<T>()
        );
    }
}

/// Runs a future as a task on the given async runtime, and reports the result back to `promise`.
///
/// More specifically, this method expects `make_future` to produce a Rust Future (`F`); that Future
/// should compute some output (`O`) and report it to the given `PromiseCompleter`. This structure
/// mirrors [`crate::jni::run_future_on_runtime`], where it's necessary for cleanup.
///
/// `promise_context` is passed through unchanged.
///
/// ## Example
///
/// ```no_run
/// # use libsignal_bridge::ffi::*;
/// # use libsignal_bridge::AsyncRuntime;
/// # struct ExampleAsyncRuntime;
/// # impl<F: std::future::Future<Output = ()>> AsyncRuntime<F> for ExampleAsyncRuntime {
/// #   fn run_future(&self, future: F) { unimplemented!() }
/// # }
/// # fn test(promise: CPromise<i32>, promise_context: *const libc::c_void, async_runtime: &ExampleAsyncRuntime) {
/// run_future_on_runtime(async_runtime, promise, promise_context, |completer| async {
///     let result: i32 = 1 + 2;
///     // Do some complicated awaiting here.
///     completer.complete(Ok(result));
/// });
/// # }
#[inline]
pub fn run_future_on_runtime<F, O>(
    runtime: &impl AsyncRuntime<F>,
    promise: CPromise<O::ResultType>,
    promise_context: *const libc::c_void,
    make_future: impl FnOnce(PromiseCompleter<O>) -> F,
) where
    F: Future<Output = ()> + std::panic::UnwindSafe + 'static,
    O: ResultTypeInfo + 'static,
{
    let completion = PromiseCompleter {
        promise,
        promise_context,
    };
    runtime.run_future(make_future(completion));
}

/// Catches panics that occur in `future` and converts them to [`SignalFfiError::UnexpectedPanic`].
pub fn catch_unwind<T>(
    future: impl Future<Output = SignalFfiResult<T>> + Send + std::panic::UnwindSafe + 'static,
) -> impl Future<Output = SignalFfiResult<T>> + Send + std::panic::UnwindSafe + 'static {
    future
        .catch_unwind()
        .unwrap_or_else(|panic| Err(SignalFfiError::UnexpectedPanic(panic)))
}
