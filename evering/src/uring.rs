use crate::layout::{alloc, alloc_buffer, dealloc, dealloc_buffer};
use crate::queue::{Drain, Offsets, Queue};
use core::fmt;
use core::marker::PhantomData;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicU32, Ordering};

#[non_exhaustive]
pub struct DisposeError {}

impl fmt::Debug for DisposeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DisposeError").finish_non_exhaustive()
    }
}

impl fmt::Display for DisposeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Uring is still connected")
    }
}

impl core::error::Error for DisposeError {}

pub(crate) trait Uring {
    type A;
    type B;
    type Ext;

    fn header(&self) -> &Header<Self::Ext>;

    fn sender(&self) -> Queue<Self::A>;

    fn receiver(&self) -> Queue<Self::B>;

    fn ext(&self) -> &Self::Ext
    where
        Self::Ext: Sync,
    {
        &self.header().ext
    }

    /// Returns `true` if the remote [`Uring`] is not dropped.
    fn is_connected(&self) -> bool {
        self.header().rc.load(Ordering::Relaxed) > 1
    }

    fn send(&mut self, val: Self::A) -> Result<(), Self::A> {
        unsafe { self.sender().enqueue(val) }
    }

    fn send_bulk<I>(&mut self, vals: I) -> usize
    where
        I: Iterator<Item = Self::A>,
    {
        unsafe { self.sender().enqueue_bulk(vals) }
    }

    fn recv(&mut self) -> Option<Self::B> {
        unsafe { self.receiver().dequeue() }
    }

    fn recv_bulk(&mut self) -> Drain<Self::B> {
        unsafe { self.receiver().dequeue_bulk() }
    }
}

pub(crate) enum UringEither<T, Ext = ()> {
    A(UringA<T, T, Ext>),
    B(UringB<T, T, Ext>),
}

impl<T, Ext> Uring for UringEither<T, Ext> {
    type A = T;
    type B = T;
    type Ext = Ext;

    fn header(&self) -> &Header<Ext> {
        match self {
            UringEither::A(a) => a.header(),
            UringEither::B(b) => b.header(),
        }
    }

    fn sender(&self) -> Queue<T> {
        match self {
            UringEither::A(a) => a.sender(),
            UringEither::B(b) => b.sender(),
        }
    }

    fn receiver(&self) -> Queue<T> {
        match self {
            UringEither::A(a) => a.receiver(),
            UringEither::B(b) => b.receiver(),
        }
    }
}

pub type Sender<Sqe, Rqe, Ext = ()> = UringA<Sqe, Rqe, Ext>;
pub type Receiver<Sqe, Rqe, Ext = ()> = UringB<Sqe, Rqe, Ext>;

pub(crate) struct UringA<A, B, Ext = ()>(RawUring<A, B, Ext>);
pub(crate) struct UringB<A, B, Ext = ()>(RawUring<A, B, Ext>);

unsafe impl<A: Send, B: Send, Ext: Send> Send for UringA<A, B, Ext> {}
unsafe impl<A: Send, B: Send, Ext: Send> Send for UringB<A, B, Ext> {}

macro_rules! common_methods {
    ($A:ident, $B:ident, $Ext:ident) => {
        pub fn into_raw(self) -> RawUring<A, B, Ext> {
            let inner = RawUring {
                header: self.0.header,
                buf_a: self.0.buf_a,
                buf_b: self.0.buf_b,
                marker: PhantomData,
            };
            core::mem::forget(self);
            inner
        }

        /// Drops this [`Uring`] and all enqueued entries.
        ///
        /// It does nothing and returns an error if `self` is still connected.
        /// Otherwise, the returned [`RawUring`] is safe to deallocate without
        /// synchronization.
        pub fn dispose_raw(self) -> Result<RawUring<A, B, Ext>, DisposeError> {
            let mut raw = self.into_raw();
            unsafe {
                match raw.dispose() {
                    Ok(_) => Ok(raw),
                    Err(e) => Err(e),
                }
            }
        }

        /// # Safety
        ///
        /// The specified [`RawUring`] must be a valid value returned from
        /// [`into_raw`](Self::into_raw).
        pub unsafe fn from_raw(uring: RawUring<A, B, Ext>) -> Self {
            Self(uring)
        }
    };
}

impl<A, B, Ext> UringA<A, B, Ext> {
    common_methods!(A, B, Ext);
}

impl<A, B, Ext> UringB<A, B, Ext> {
    common_methods!(A, B, Ext);
}

impl<A, B, Ext> Uring for UringA<A, B, Ext> {
    type A = A;
    type B = B;
    type Ext = Ext;

    fn header(&self) -> &Header<Ext> {
        unsafe { self.0.header() }
    }
    fn sender(&self) -> Queue<Self::A> {
        unsafe { self.0.queue_a() }
    }
    fn receiver(&self) -> Queue<Self::B> {
        unsafe { self.0.queue_b() }
    }
}

impl<A, B, Ext> Uring for UringB<A, B, Ext> {
    type A = B;
    type B = A;
    type Ext = Ext;

    fn header(&self) -> &Header<Ext> {
        unsafe { self.0.header() }
    }
    fn sender(&self) -> Queue<Self::A> {
        unsafe { self.0.queue_b() }
    }
    fn receiver(&self) -> Queue<Self::B> {
        unsafe { self.0.queue_a() }
    }
}

impl<A, B, Ext> Drop for UringA<A, B, Ext> {
    fn drop(&mut self) {
        unsafe { self.0.drop_in_place() }
    }
}

impl<A, B, Ext> Drop for UringB<A, B, Ext> {
    fn drop(&mut self) {
        unsafe { self.0.drop_in_place() }
    }
}

pub struct Header<Ext = ()> {
    off_a: Offsets,
    off_b: Offsets,
    rc: AtomicU32,
    ext: Ext,
}

impl<Ext> Header<Ext> {
    pub fn size_a(&self) -> usize {
        self.off_a.ring_mask as usize + 1
    }

    pub fn size_b(&self) -> usize {
        self.off_b.ring_mask as usize + 1
    }
}

pub struct RawUring<A, B, Ext = ()> {
    pub header: NonNull<Header<Ext>>,
    pub buf_a: NonNull<A>,
    pub buf_b: NonNull<B>,
    marker: PhantomData<fn(A, B, Ext) -> (A, B, Ext)>,
}

impl<A, B, Ext> RawUring<A, B, Ext> {
    pub const fn dangling() -> Self {
        Self {
            header: NonNull::dangling(),
            buf_a: NonNull::dangling(),
            buf_b: NonNull::dangling(),
            marker: PhantomData,
        }
    }

    unsafe fn header(&self) -> &Header<Ext> {
        unsafe { self.header.as_ref() }
    }

    unsafe fn queue_a(&self) -> Queue<'_, A> {
        Queue {
            off: unsafe { &self.header().off_a },
            buf: self.buf_a,
        }
    }

    unsafe fn queue_b(&self) -> Queue<'_, B> {
        Queue {
            off: unsafe { &self.header().off_b },
            buf: self.buf_b,
        }
    }

    unsafe fn dispose(&mut self) -> Result<(), DisposeError> {
        let rc = unsafe { &self.header().rc };
        debug_assert!(rc.load(Ordering::Relaxed) >= 1);
        // `Release` enforeces any use of the data to happen before here.
        if rc.fetch_sub(1, Ordering::Release) != 1 {
            return Err(DisposeError {});
        }
        // `Acquire` enforces the deletion of the data to happen after here.
        core::sync::atomic::fence(Ordering::Acquire);

        unsafe {
            self.queue_a().drop_in_place();
            self.queue_b().drop_in_place();
        }
        Ok(())
    }

    unsafe fn drop_in_place(&mut self) {
        unsafe {
            if self.dispose().is_ok() {
                let h = self.header.as_ref();
                dealloc_buffer(self.buf_a, h.off_a.ring_mask as usize + 1);
                dealloc_buffer(self.buf_b, h.off_b.ring_mask as usize + 1);
                dealloc(self.header);
            }
        }
    }
}

struct Builder<S: Uring> {
    size_a: usize,
    size_b: usize,
    ext: S::Ext,
    _marker: PhantomData<(S::A, S::B)>,
}

impl<S:Uring> Builder<S> {
    pub fn new() -> Self
    where
        Ext: Default,
    {
        Self::new_ext(Ext::default())
    }

    pub fn new_ext(ext: Ext) -> Self {
        Self {
            size_a: 32,
            size_b: 32,
            ext,
            _marker: PhantomData,
        }
    }

    pub fn size_a(&mut self, size: usize) -> &mut Self {
        assert!(size.is_power_of_two());
        self.size_a = size;
        self
    }

    pub fn size_b(&mut self, size: usize) -> &mut Self {
        assert!(size.is_power_of_two());
        self.size_b = size;
        self
    }

    pub fn build_header(self) -> Header<Ext> {
        Header {
            off_a: Offsets::new(self.size_a as u32),
            off_b: Offsets::new(self.size_b as u32),
            rc: AtomicU32::new(2),
            ext: self.ext,
        }
    }

    pub fn build(self) -> (UringA<A, B, Ext>, UringB<A, B, Ext>) {
        let header;
        let buf_a;
        let buf_b;

        unsafe {
            header = alloc::<Header<Ext>>();
            buf_a = alloc_buffer(self.size_a);
            buf_b = alloc_buffer(self.size_b);

            header.write(self.build_header());
        }

        let ring_a = UringA(RawUring {
            header,
            buf_a,
            buf_b,
            marker: PhantomData,
        });
        let ring_b = UringB(RawUring {
            header,
            buf_a,
            buf_b,
            marker: PhantomData,
        });

        (ring_a, ring_b)
    }
}

impl<A, B, Ext: Default> Default for Builder<A, B, Ext> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, AtomicUsize};

    use super::*;

    #[test]
    fn queue_len() {
        let mut len_a = 0;
        let mut len_b = 0;
        let (mut pa, mut pb) = Builder::<(), ()>::new().build();
        for _ in 0..32 {
            match fastrand::u8(0..4) {
                0 => len_a += pa.send(()).map_or(0, |_| 1),
                1 => len_b += pb.send(()).map_or(0, |_| 1),
                2 => len_a -= pb.recv().map_or(0, |_| 1),
                3 => len_b -= pa.recv().map_or(0, |_| 1),
                _ => unreachable!(),
            }
            assert_eq!(pa.sender().len(), pb.receiver().len());
            assert_eq!(pa.receiver().len(), pb.sender().len());
            assert_eq!(pa.sender().len(), len_a);
            assert_eq!(pb.sender().len(), len_b);
        }
    }

    #[test]
    fn uring_drop() {
        static DROP_COUNT: AtomicUsize = AtomicUsize::new(0);

        #[derive(Debug)]
        struct DropCounter(char);
        impl Drop for DropCounter {
            fn drop(&mut self) {
                DROP_COUNT.fetch_add(1, Ordering::Relaxed);
            }
        }

        let input = std::iter::repeat_with(fastrand::alphabetic)
            .take(30)
            .collect::<Vec<_>>();

        let (mut pa, mut pb) = Builder::<DropCounter, DropCounter>::new().build();
        std::thread::scope(|cx| {
            cx.spawn(|| {
                for i in input.iter().copied().map(DropCounter) {
                    if i.0.is_uppercase() {
                        pa.send(i).unwrap();
                    } else {
                        _ = pa.recv();
                    }
                }
                drop(pa);
            });
            cx.spawn(|| {
                for i in input.iter().copied().map(DropCounter) {
                    if i.0.is_lowercase() {
                        pb.send(i).unwrap();
                    } else {
                        _ = pb.recv();
                    }
                }
                drop(pb);
            });
        });

        assert_eq!(DROP_COUNT.load(Ordering::Relaxed), input.len() * 2);
    }

    #[test]
    fn uring_threaded() {
        let input = std::iter::repeat_with(fastrand::alphabetic)
            .take(30)
            .collect::<Vec<_>>();

        let (mut pa, mut pb) = Builder::<char, char>::new().build();
        let (pa_finished, pb_finished) = (AtomicBool::new(false), AtomicBool::new(false));
        std::thread::scope(|cx| {
            cx.spawn(|| {
                let mut r = vec![];
                for i in input.iter().copied() {
                    pa.send(i).unwrap();
                    while let Some(i) = pa.recv() {
                        r.push(i);
                    }
                }
                pa_finished.store(true, Ordering::Release);
                while !pb_finished.load(Ordering::Acquire) {
                    std::thread::yield_now();
                }
                while let Some(i) = pa.recv() {
                    r.push(i);
                }
                assert_eq!(r, input);
            });
            cx.spawn(|| {
                let mut r = vec![];
                for i in input.iter().copied() {
                    pb.send(i).unwrap();
                    while let Some(i) = pb.recv() {
                        r.push(i);
                    }
                }
                pb_finished.store(true, Ordering::Release);
                while !pa_finished.load(Ordering::Acquire) {
                    std::thread::yield_now();
                }
                while let Some(i) = pb.recv() {
                    r.push(i);
                }
                assert_eq!(r, input);
            });
        });
    }

    #[test]
    fn uring_threaded_bulk() {
        let input = std::iter::repeat_with(fastrand::alphabetic)
            .take(30)
            .collect::<Vec<_>>();

        let (mut pa, mut pb) = Builder::<char, char>::new().build();
        let (pa_finished, pb_finished) = (AtomicBool::new(false), AtomicBool::new(false));
        std::thread::scope(|cx| {
            cx.spawn(|| {
                let mut r = vec![];
                pa.send_bulk(input.iter().copied());
                pa_finished.store(true, Ordering::Release);
                while !pb_finished.load(Ordering::Acquire) {
                    r.extend(pa.recv_bulk());
                    std::thread::yield_now();
                }
                r.extend(pa.recv_bulk());
                assert_eq!(r, input);
            });
            cx.spawn(|| {
                let mut r = vec![];
                pb.send_bulk(input.iter().copied());
                pb_finished.store(true, Ordering::Release);
                while !pa_finished.load(Ordering::Acquire) {
                    r.extend(pb.recv_bulk());
                    std::thread::yield_now();
                }
                r.extend(pb.recv_bulk());
                assert_eq!(r, input);
            });
        });
    }
}
