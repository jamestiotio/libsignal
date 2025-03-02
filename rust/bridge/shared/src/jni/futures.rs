//
// Copyright 2023 Signal Messenger, LLC.
// SPDX-License-Identifier: AGPL-3.0-only
//

use super::*;
use crate::support::AsyncRuntime;

use futures_util::{FutureExt, TryFutureExt};

use std::future::Future;

/// Used to complete a Java CompletableFuture from any thread.
pub struct FutureCompleter<T> {
    jvm: JavaVM,
    future: GlobalRef,
    complete_signature: PhantomData<fn(T)>,
}

impl<T: for<'a> ResultTypeInfo<'a> + std::panic::UnwindSafe> FutureCompleter<T> {
    /// Stores a handle to the JVM and a global reference to the given future.
    ///
    /// `future` is expected to refer to a CompletableFuture instance, and will
    /// have methods called on it that match the signatures on CompletableFuture.
    pub fn new(env: &mut JNIEnv, future: &JObject) -> jni::errors::Result<Self> {
        Ok(Self {
            jvm: env.get_java_vm()?,
            future: env.new_global_ref(future)?,
            complete_signature: PhantomData,
        })
    }

    /// Converts `result` to a Java value and completes the future in `self`.
    pub fn complete(self, result: SignalJniResult<T>) {
        self.complete_with_extra_args_to_drop(result, ())
    }

    /// Converts `result` to a Java value and completes the future in `self`.
    ///
    /// Additionally, drops `extra_args_to_drop` while attached to the JVM.
    /// This is important if `extra_args_to_drop` contains global references,
    /// or other data that requires an attached JVM to clean up.
    pub fn complete_with_extra_args_to_drop<U>(
        self,
        result: SignalJniResult<T>,
        extra_args_to_drop: U,
    ) {
        let Self {
            jvm,
            future,
            complete_signature: _,
        } = self;

        let mut env = match jvm.attach_current_thread() {
            Ok(attach_guard) => attach_guard,
            Err(e) => {
                // Most likely this log will fail too,
                // but really we don't expect attach_current_thread to fail at all.
                log::error!("failed to attach to JVM: {e}");
                return;
            }
        };

        // Catch panics while converting successful results to Java values.
        // (We have no *expected* panics, but we don't want to bring down the process for a
        // libsignal-internal bug if we can help it.)
        let maybe_error: SignalJniResult<()> = result.and_then(|result| {
            // This AssertUnwindSafe isn't totally justified, but if we get a panic talking to the
            // JVM, we have bigger problems.
            let env_for_catch_unwind = std::panic::AssertUnwindSafe(&mut env);
            let future_for_catch_unwind = &future;
            std::panic::catch_unwind(move || {
                // Force the lambda to capture the whole struct instead of an individual field.
                let _ = &env_for_catch_unwind;
                let env = env_for_catch_unwind.0;
                result.convert_into(env).and_then(|result| {
                    let result_as_jobject = box_primitive_if_needed(env, result.into())?;
                    call_method_checked(
                        env,
                        future_for_catch_unwind,
                        "complete",
                        jni_args!((result_as_jobject => java.lang.Object) -> boolean),
                    )
                    .map(|_| ())
                })
            })
            .unwrap_or_else(|panic| Err(SignalJniError::UnexpectedPanic(panic)))
        });

        // From this point on we can't catch panics, because SignalJniError isn't UnwindSafe. This
        // is consistent with the synchronous implementation in run_ffi_safe, which doesn't catch
        // panics when converting errors to exceptions either.
        maybe_error.unwrap_or_else(|error| {
            convert_to_exception(&mut env, error, |env, throwable, error| {
                throwable
                    .and_then(|throwable| {
                        call_method_checked(
                            env,
                            &future,
                            "completeExceptionally",
                            jni_args!((throwable => java.lang.Throwable) -> boolean),
                        )
                        .map(|_| ())
                    })
                    .unwrap_or_else(|completion_error| {
                        log::error!(
                            "failed to complete Future with error \"{error}\": {completion_error}"
                        );
                    });
            })
        });

        // Explicitly drop these while the thread is still attached to the JVM.
        drop(future);
        drop(extra_args_to_drop);
    }
}

/// Runs a future as a task on the given async runtime, and saves the result in a new Java Future
/// object (the return value).
///
/// More specifically, this method expects `make_future` to produce a Rust Future (`F`); that Future
/// should compute some output (`O`) and report it to the given `FutureCompleter`. This complex
/// arrangement allows the Rust Future to clean up any saved data that needs an attached JVM thread.
///
/// ## Example
///
/// ```no_run
/// # use jni::JNIEnv;
/// # use libsignal_bridge::jni::*;
/// # use libsignal_bridge::AsyncRuntime;
/// # struct ExampleAsyncRuntime;
/// # impl<F: std::future::Future<Output = ()>> AsyncRuntime<F> for ExampleAsyncRuntime {
/// #   fn run_future(&self, future: F) { unimplemented!() }
/// # }
/// # fn test(env: &mut JNIEnv, async_runtime: &ExampleAsyncRuntime) -> SignalJniResult<()> {
/// let java_future = run_future_on_runtime(env, async_runtime, |completer| async {
///     let result: i32 = 1 + 2;
///     // Do some complicated awaiting here.
///     completer.complete(Ok(result));
/// })?;
/// # Ok(())
/// # }
pub fn run_future_on_runtime<'local, F, O>(
    env: &mut JNIEnv<'local>,
    runtime: &impl AsyncRuntime<F>,
    make_future: impl FnOnce(FutureCompleter<O>) -> F,
) -> SignalJniResult<JavaFuture<'local, <O as ResultTypeInfo<'local>>::ResultType>>
where
    F: Future<Output = ()> + std::panic::UnwindSafe + 'static,
    O: for<'a> ResultTypeInfo<'a> + std::panic::UnwindSafe + 'static,
{
    let java_future = new_object(
        env,
        jni_class_name!(org.signal.libsignal.internal.CompletableFuture),
        jni_args!(() -> void),
    )?;
    let completer = FutureCompleter::new(env, &java_future)?;
    runtime.run_future(make_future(completer));
    Ok(java_future.into())
}

/// Catches panics that occur in `future` and converts them to [`SignalJniError::UnexpectedPanic`].
pub fn catch_unwind<'a, O>(
    future: impl Future<Output = SignalJniResult<O>> + Send + std::panic::UnwindSafe + 'a,
) -> impl Future<Output = SignalJniResult<O>> + Send + std::panic::UnwindSafe + 'a {
    future
        .catch_unwind()
        .unwrap_or_else(|panic| Err(SignalJniError::UnexpectedPanic(panic)))
}
