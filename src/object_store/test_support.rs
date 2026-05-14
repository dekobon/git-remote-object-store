//! Test-only support for `ObjectStore` decorator boilerplate.
//!
//! Per-test [`ObjectStore`] decorators wrap a `MockStore` and override
//! a single trait method (often to fire a one-shot hook or count
//! calls), forwarding every other method unchanged. Hand-writing the
//! forwarders takes ~80 lines per decorator; this module exposes
//! [`delegate_to_inner_impl!`], a macro that emits the per-method
//! forwarders plus the `#[async_trait::async_trait] impl ObjectStore`
//! wrapper so each decorator collapses to its struct + the one
//! intercepted method.
//!
//! ## Why the macro emits the full `impl` block
//!
//! `#[async_trait::async_trait]` is an attribute macro that processes
//! the token tree of the impl block *as written*; it does not
//! recursively expand inner macro invocations. A naive forwarder
//! macro placed inside a hand-written `#[async_trait] impl ObjectStore
//! { ... }` would emit raw `async fn` items that never receive
//! `async_trait`'s lifetime-desugaring pass, triggering `E0195`
//! lifetime mismatches against the trait declaration.
//!
//! To avoid that, [`delegate_to_inner_impl!`] expands as a TT-muncher
//! that builds the complete method list in a single accumulator and
//! emits one `#[async_trait]`-decorated impl block with every method
//! body inline -- no nested macro invocations remain at the moment
//! `async_trait` sees the impl block.
//!
//! ## Usage
//!
//! ```ignore
//! delegate_to_inner_impl! {
//!     impl ObjectStore for MyDecorator {
//!         // Methods to forward verbatim to `self.inner`:
//!         forward: get_to_file, get_bytes, get_bytes_range,
//!                  put_bytes, put_path, put_if_absent,
//!                  head, copy, delete;
//!
//!         // Caller's override(s) -- written as normal `async fn`:
//!         async fn list(&self, prefix: &str)
//!             -> Result<Vec<ObjectMeta>, ObjectStoreError>
//!         {
//!             // ...custom behavior...
//!             self.inner.list(prefix).await
//!         }
//!     }
//! }
//! ```
//!
//! The macro requires `self.inner` to be a value (or reference) that
//! implements [`crate::object_store::ObjectStore`].
//!
//! ## Omitting a method from `forward:` inherits the trait default
//!
//! Listing a method under `forward:` emits a direct `self.inner.<m>`
//! delegator. Omitting it inherits the `ObjectStore` *trait default*
//! (which is NOT a delegator) — load-bearing for two cases today:
//!
//! - `presigned_get_url`: no decorator in this crate overrides it,
//!   and the trait default returns `Unsupported`. Always omit.
//! - `put_path`: the trait default routes through `put_bytes` after
//!   reading the file into memory, so any decorator that intercepts
//!   `put_bytes` will also observe the put-path traffic without
//!   needing an explicit forwarder. `read.rs::EvolvingChainStore`
//!   and `protocol/push.rs::CountingStore` rely on this.
//!
//! For any other method, omission means the impl block is incomplete
//! and the build fails with E0046 — listing it under `forward:` is
//! the safe default.

/// Emit a `#[async_trait::async_trait] impl ObjectStore for $Type`
/// block whose body is the caller's overridden methods plus
/// per-method forwarders to `self.inner` for every name listed in the
/// `forward:` clause. See the module docs for usage and the rationale
/// behind wrapping the impl block.
///
/// Internally a TT-muncher accumulates each forwarder's tokens so the
/// final emitted impl block contains every method inline -- no nested
/// macro invocations remain when `#[async_trait]` runs.
#[macro_export]
macro_rules! delegate_to_inner_impl {
    // Public entrypoint: kick off the muncher with an empty accumulator.
    (
        impl ObjectStore for $Type:ty {
            forward: $($method:ident),* $(,)? ;
            $($overrides:tt)*
        }
    ) => {
        $crate::delegate_to_inner_impl! {
            @munch
            ty: $Type;
            overrides: { $($overrides)* };
            acc: {};
            remaining: [ $($method)* ];
        }
    };

    // Base case: no methods left to forward -> emit the impl block.
    (
        @munch
        ty: $Type:ty;
        overrides: { $($overrides:tt)* };
        acc: { $($acc:tt)* };
        remaining: [];
    ) => {
        #[::async_trait::async_trait]
        impl $crate::object_store::ObjectStore for $Type {
            $($overrides)*
            $($acc)*
        }
    };

    // -- Per-method munching arms: append the forwarder for the head
    //    of `remaining` to `acc`, then recurse on the tail.

    (
        @munch
        ty: $Type:ty;
        overrides: { $($overrides:tt)* };
        acc: { $($acc:tt)* };
        remaining: [ list $($rest:ident)* ];
    ) => {
        $crate::delegate_to_inner_impl! {
            @munch
            ty: $Type;
            overrides: { $($overrides)* };
            acc: {
                $($acc)*
                async fn list(
                    &self,
                    prefix: &str,
                ) -> ::std::result::Result<
                    ::std::vec::Vec<$crate::object_store::ObjectMeta>,
                    $crate::object_store::ObjectStoreError,
                > {
                    self.inner.list(prefix).await
                }
            };
            remaining: [ $($rest)* ];
        }
    };

    (
        @munch
        ty: $Type:ty;
        overrides: { $($overrides:tt)* };
        acc: { $($acc:tt)* };
        remaining: [ get_to_file $($rest:ident)* ];
    ) => {
        $crate::delegate_to_inner_impl! {
            @munch
            ty: $Type;
            overrides: { $($overrides)* };
            acc: {
                $($acc)*
                async fn get_to_file(
                    &self,
                    key: &str,
                    dest: &::std::path::Path,
                    opts: $crate::object_store::GetOpts,
                ) -> ::std::result::Result<(), $crate::object_store::ObjectStoreError> {
                    self.inner.get_to_file(key, dest, opts).await
                }
            };
            remaining: [ $($rest)* ];
        }
    };

    (
        @munch
        ty: $Type:ty;
        overrides: { $($overrides:tt)* };
        acc: { $($acc:tt)* };
        remaining: [ get_bytes $($rest:ident)* ];
    ) => {
        $crate::delegate_to_inner_impl! {
            @munch
            ty: $Type;
            overrides: { $($overrides)* };
            acc: {
                $($acc)*
                async fn get_bytes(
                    &self,
                    key: &str,
                ) -> ::std::result::Result<
                    ::bytes::Bytes,
                    $crate::object_store::ObjectStoreError,
                > {
                    self.inner.get_bytes(key).await
                }
            };
            remaining: [ $($rest)* ];
        }
    };

    (
        @munch
        ty: $Type:ty;
        overrides: { $($overrides:tt)* };
        acc: { $($acc:tt)* };
        remaining: [ get_bytes_range $($rest:ident)* ];
    ) => {
        $crate::delegate_to_inner_impl! {
            @munch
            ty: $Type;
            overrides: { $($overrides)* };
            acc: {
                $($acc)*
                async fn get_bytes_range(
                    &self,
                    key: &str,
                    range: ::std::ops::Range<u64>,
                ) -> ::std::result::Result<
                    ::bytes::Bytes,
                    $crate::object_store::ObjectStoreError,
                > {
                    self.inner.get_bytes_range(key, range).await
                }
            };
            remaining: [ $($rest)* ];
        }
    };

    (
        @munch
        ty: $Type:ty;
        overrides: { $($overrides:tt)* };
        acc: { $($acc:tt)* };
        remaining: [ put_bytes $($rest:ident)* ];
    ) => {
        $crate::delegate_to_inner_impl! {
            @munch
            ty: $Type;
            overrides: { $($overrides)* };
            acc: {
                $($acc)*
                async fn put_bytes(
                    &self,
                    key: &str,
                    body: ::bytes::Bytes,
                    opts: $crate::object_store::PutOpts,
                ) -> ::std::result::Result<(), $crate::object_store::ObjectStoreError> {
                    self.inner.put_bytes(key, body, opts).await
                }
            };
            remaining: [ $($rest)* ];
        }
    };

    (
        @munch
        ty: $Type:ty;
        overrides: { $($overrides:tt)* };
        acc: { $($acc:tt)* };
        remaining: [ put_path $($rest:ident)* ];
    ) => {
        $crate::delegate_to_inner_impl! {
            @munch
            ty: $Type;
            overrides: { $($overrides)* };
            acc: {
                $($acc)*
                async fn put_path(
                    &self,
                    key: &str,
                    src: &::std::path::Path,
                    opts: $crate::object_store::PutOpts,
                ) -> ::std::result::Result<(), $crate::object_store::ObjectStoreError> {
                    self.inner.put_path(key, src, opts).await
                }
            };
            remaining: [ $($rest)* ];
        }
    };

    (
        @munch
        ty: $Type:ty;
        overrides: { $($overrides:tt)* };
        acc: { $($acc:tt)* };
        remaining: [ put_if_absent $($rest:ident)* ];
    ) => {
        $crate::delegate_to_inner_impl! {
            @munch
            ty: $Type;
            overrides: { $($overrides)* };
            acc: {
                $($acc)*
                async fn put_if_absent(
                    &self,
                    key: &str,
                    body: ::bytes::Bytes,
                ) -> ::std::result::Result<bool, $crate::object_store::ObjectStoreError> {
                    self.inner.put_if_absent(key, body).await
                }
            };
            remaining: [ $($rest)* ];
        }
    };

    (
        @munch
        ty: $Type:ty;
        overrides: { $($overrides:tt)* };
        acc: { $($acc:tt)* };
        remaining: [ head $($rest:ident)* ];
    ) => {
        $crate::delegate_to_inner_impl! {
            @munch
            ty: $Type;
            overrides: { $($overrides)* };
            acc: {
                $($acc)*
                async fn head(
                    &self,
                    key: &str,
                ) -> ::std::result::Result<
                    $crate::object_store::ObjectMeta,
                    $crate::object_store::ObjectStoreError,
                > {
                    self.inner.head(key).await
                }
            };
            remaining: [ $($rest)* ];
        }
    };

    (
        @munch
        ty: $Type:ty;
        overrides: { $($overrides:tt)* };
        acc: { $($acc:tt)* };
        remaining: [ copy $($rest:ident)* ];
    ) => {
        $crate::delegate_to_inner_impl! {
            @munch
            ty: $Type;
            overrides: { $($overrides)* };
            acc: {
                $($acc)*
                async fn copy(
                    &self,
                    src: &str,
                    dst: &str,
                ) -> ::std::result::Result<(), $crate::object_store::ObjectStoreError> {
                    self.inner.copy(src, dst).await
                }
            };
            remaining: [ $($rest)* ];
        }
    };

    (
        @munch
        ty: $Type:ty;
        overrides: { $($overrides:tt)* };
        acc: { $($acc:tt)* };
        remaining: [ delete $($rest:ident)* ];
    ) => {
        $crate::delegate_to_inner_impl! {
            @munch
            ty: $Type;
            overrides: { $($overrides)* };
            acc: {
                $($acc)*
                async fn delete(
                    &self,
                    key: &str,
                ) -> ::std::result::Result<(), $crate::object_store::ObjectStoreError> {
                    self.inner.delete(key).await
                }
            };
            remaining: [ $($rest)* ];
        }
    };

    (
        @munch
        ty: $Type:ty;
        overrides: { $($overrides:tt)* };
        acc: { $($acc:tt)* };
        remaining: [ presigned_get_url $($rest:ident)* ];
    ) => {
        $crate::delegate_to_inner_impl! {
            @munch
            ty: $Type;
            overrides: { $($overrides)* };
            acc: {
                $($acc)*
                async fn presigned_get_url(
                    &self,
                    key: &str,
                    ttl: ::std::time::Duration,
                ) -> ::std::result::Result<
                    ::std::string::String,
                    $crate::object_store::ObjectStoreError,
                > {
                    self.inner.presigned_get_url(key, ttl).await
                }
            };
            remaining: [ $($rest)* ];
        }
    };
}

#[cfg(test)]
mod tests {
    //! Smoke tests for [`delegate_to_inner_impl!`]. These exercise the
    //! macro's two intended shapes -- a decorator that overrides one
    //! method and forwards the rest, and a transparent pass-through
    //! that overrides nothing -- so a future change to the macro's
    //! token-tree pattern that compiles but produces wrong dispatch
    //! is caught here rather than in every downstream decorator.

    use bytes::Bytes;

    use crate::object_store::ObjectStore;
    use crate::object_store::mock::MockStore;

    /// One-method override: intercept `head` to record the key, forward
    /// every other method to `inner`.
    struct RecordingHead {
        inner: MockStore,
        seen: std::sync::Mutex<Vec<String>>,
    }

    crate::delegate_to_inner_impl! {
        impl ObjectStore for RecordingHead {
            forward: list, get_to_file, get_bytes, get_bytes_range,
                     put_bytes, put_path, put_if_absent,
                     copy, delete, presigned_get_url;

            async fn head(
                &self,
                key: &str,
            ) -> Result<
                crate::object_store::ObjectMeta,
                crate::object_store::ObjectStoreError,
            > {
                self.seen.lock().unwrap().push(key.to_owned());
                self.inner.head(key).await
            }
        }
    }

    #[tokio::test]
    async fn override_records_intercepted_method_and_forwards_others() {
        let inner = MockStore::new();
        inner.insert("k", Bytes::from_static(b"v"));
        let store = RecordingHead {
            inner,
            seen: std::sync::Mutex::new(Vec::new()),
        };

        // Forwarded method: must reach inner and return its bytes.
        let body = store.get_bytes("k").await.expect("forwarded get_bytes");
        assert_eq!(body.as_ref(), b"v");

        // Overridden method: records the key and still hits inner.
        let _ = store.head("k").await.expect("forwarded head");
        assert_eq!(store.seen.lock().unwrap().as_slice(), &["k".to_owned()]);
    }

    /// Transparent pass-through: forwards every method, overrides
    /// nothing.
    struct PassThrough {
        inner: MockStore,
    }

    crate::delegate_to_inner_impl! {
        impl ObjectStore for PassThrough {
            forward: list, get_to_file, get_bytes, get_bytes_range,
                     put_bytes, put_path, put_if_absent,
                     head, copy, delete, presigned_get_url;
        }
    }

    #[tokio::test]
    async fn no_overrides_forwards_every_method_to_inner() {
        let inner = MockStore::new();
        inner.insert("k", Bytes::from_static(b"v"));
        let store = PassThrough { inner };
        let body = store.get_bytes("k").await.expect("forwarded get_bytes");
        assert_eq!(body.as_ref(), b"v");
        let entries = store.list("").await.expect("forwarded list");
        assert_eq!(entries.len(), 1);
    }
}
