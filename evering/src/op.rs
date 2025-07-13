use alloc::boxed::Box;
use core::any::Any;
use core::pin::Pin;
use core::task::{Context, Poll};

use crate::driver::{DriverHandle, OpId};

/// # Safety
///
/// All submitted resources must be recycled.
pub unsafe trait Completable: 'static + Unpin {
    type Output;
    type Driver: DriverHandle;

    /// Transforms the received payload to the corresponding output.
    ///
    /// This function is called when the operation is completed, and the output
    /// is then returned as [`Poll::Ready`].
    fn complete(
        self,
        driver: &Self::Driver,
        payload: <Self::Driver as DriverHandle>::Payload,
    ) -> Self::Output;

    /// Completes this operation with the submitted extension.
    ///
    /// For more information, see [`complete`](Self::complete).
    fn complete_ext(
        self,
        driver: &Self::Driver,
        payload: <Self::Driver as DriverHandle>::Payload,
        ext: <Self::Driver as DriverHandle>::Ext,
    ) -> Self::Output
    where
        Self: Sized,
    {
        _ = ext;
        self.complete(driver, payload)
    }

    /// Cancels this operation.
    fn cancel(self, driver: &Self::Driver) -> Cancellation;
}

pub struct Cancellation(#[allow(dead_code)] Option<Box<dyn Any>>);

impl Cancellation {
    pub const fn noop() -> Self {
        Self(None)
    }

    pub fn recycle<T: 'static>(resource: T) -> Self {
        Self(Some(Box::new(resource)))
    }
}

pub struct Op<T: Completable> {
    driver: T::Driver,
    id: OpId,
    data: Option<T>,
}

impl<T: Completable> Op<T> {
    pub fn new(driver: T::Driver, id: OpId, data: T) -> Self {
        Self {
            driver,
            id,
            data: Some(data),
        }
    }
}

impl<T: Completable> Future for Op<T> {
    type Output = T::Output;
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.driver.get().poll(self.id, cx).map(|(p, ext)| {
            self.data
                .take()
                .expect("invalid operation state")
                .complete_ext(&self.driver, p, ext)
        })
    }
}

impl<T: Completable> Drop for Op<T> {
    fn drop(&mut self) {
        self.driver.get().remove(self.id, || {
            self.data
                .take()
                .expect("invalid operation state")
                .cancel(&self.driver)
        })
    }
}
