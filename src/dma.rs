#![allow(dead_code)]

use core::marker::PhantomData;
use core::ops;

use rcc::AHB;

#[derive(Debug)]
pub enum Error {
    Overrun,
    BufferError,
    #[doc(hidden)]
    _Extensible,
}

pub enum Event {
    HalfTransfer,
    TransferComplete,
    TransferError,
}

#[derive(Clone, Copy, PartialEq)]
pub enum Half {
    First,
    Second,
}

pub struct CircBuffer<BUFFER, CHANNEL>
where
    BUFFER: 'static,
{
    buffer: &'static mut [BUFFER; 2],
    channel: CHANNEL,
    readable_half: Half,
    consumed_offset: usize,
}

impl<BUFFER, CHANNEL> CircBuffer<BUFFER, CHANNEL> {
    pub(crate) fn new(buf: &'static mut [BUFFER; 2], chan: CHANNEL) -> Self {
        CircBuffer {
            buffer: buf,
            channel: chan,
            readable_half: Half::Second,
            consumed_offset: 0,
        }
    }
}

pub trait Static<B> {
    fn borrow(&self) -> &B;
}

impl<B> Static<B> for &'static B {
    fn borrow(&self) -> &B {
        *self
    }
}

impl<B> Static<B> for &'static mut B {
    fn borrow(&self) -> &B {
        *self
    }
}

pub trait DmaExt {
    type Channels;

    fn split(self, ahb: &mut AHB) -> Self::Channels;
}

pub struct Transfer<MODE, BUFFER, CHANNEL, PAYLOAD> {
    _mode: PhantomData<MODE>,
    buffer: BUFFER,
    channel: CHANNEL,
    payload: PAYLOAD,
}

impl<BUFFER, CHANNEL, PAYLOAD> Transfer<R, BUFFER, CHANNEL, PAYLOAD> {
    pub(crate) fn r(buffer: BUFFER, channel: CHANNEL, payload: PAYLOAD) -> Self {
        Transfer {
            _mode: PhantomData,
            buffer,
            channel,
            payload,
        }
    }
}

impl<BUFFER, CHANNEL, PAYLOAD> Transfer<W, BUFFER, CHANNEL, PAYLOAD> {
    pub(crate) fn w(buffer: BUFFER, channel: CHANNEL, payload: PAYLOAD) -> Self {
        Transfer {
            _mode: PhantomData,
            buffer,
            channel,
            payload,
        }
    }
}

impl<BUFFER, CHANNEL, PAYLOAD> ops::Deref for Transfer<R, BUFFER, CHANNEL, PAYLOAD> {
    type Target = BUFFER;

    fn deref(&self) -> &BUFFER {
        &self.buffer
    }
}

/// Read transfer
pub struct R;

/// Write transfer
pub struct W;

macro_rules! dma {
    ($($DMAX:ident: ($dmaX:ident, $dmaXen:ident, $dmaXrst:ident, {
        $($CX:ident: (
            $ccrX:ident,
            $CCRX:ident,
            $cndtrX:ident,
            $CNDTRX:ident,
            $cparX:ident,
            $CPARX:ident,
            $cmarX:ident,
            $CMARX:ident,
            $htifX:ident,
            $tcifX:ident,
            $chtifX:ident,
            $ctcifX:ident,
            $cgifX:ident,
            $cXs:ident
        ),)+
    }),)+) => {
        $(
            pub mod $dmaX {
                use core::marker::Unsize;
                use core::sync::atomic::{self, Ordering};

                use stm32l4x6::{$DMAX, dma1};

                use dma::{CircBuffer, DmaExt, Error, Event, Half, Transfer, W};
                use rcc::AHB;

                pub struct Channels((), $(pub $CX),+);

                $(
                    pub struct $CX { _0: () }

                    impl $CX {
                        pub fn listen(&mut self, event: Event) {
                            match event {
                                Event::HalfTransfer => self.ccr().modify(|_, w| w.htie().set_bit()),
                                Event::TransferComplete => {
                                    self.ccr().modify(|_, w| w.tcie().set_bit())
                                },
                                Event::TransferError => { self.ccr().modify(|_, w| w.teie().set_bit()) },
                            }
                        }

                        pub fn unlisten(&mut self, event: Event) {
                            match event {
                                Event::HalfTransfer => {
                                    self.ccr().modify(|_, w| w.htie().clear_bit())
                                },
                                Event::TransferComplete => {
                                    self.ccr().modify(|_, w| w.tcie().clear_bit())
                                },
                                Event::TransferError => {
                                    self.ccr().modify(|_, w| w.teie().clear_bit())
                                },
                            }
                        }

                        pub(crate) fn isr(&self) -> dma1::isr::R {
                            // NOTE(unsafe) atomic read with no side effects
                            unsafe { (*$DMAX::ptr()).isr.read() }
                        }

                        pub(crate) fn ifcr(&self) -> &dma1::IFCR {
                            unsafe { &(*$DMAX::ptr()).ifcr }
                        }

                        pub(crate) fn ccr(&mut self) -> &dma1::$CCRX {
                            unsafe { &(*$DMAX::ptr()).$ccrX }
                        }

                        pub(crate) fn cndtr(&mut self) -> &dma1::$CNDTRX {
                            unsafe { &(*$DMAX::ptr()).$cndtrX }
                        }

                        pub(crate) fn cpar(&mut self) -> &dma1::$CPARX {
                            unsafe { &(*$DMAX::ptr()).$cparX }
                        }

                        pub(crate) fn cmar(&mut self) -> &dma1::$CMARX {
                            unsafe { &(*$DMAX::ptr()).$cmarX }
                        }

                        pub(crate) fn get_cndtr(&self) -> u32 {
                            // NOTE(unsafe) atomic read with no side effects
                            unsafe { (*$DMAX::ptr()).$cndtrX.read().bits() }
                        }

                        pub fn set_req_map(&self, bits: u8) {
                            unsafe { (*$DMAX::ptr()).cselr.modify(|_,w| w.$cXs().bits(bits)) }
                        }

                    }

                    impl<B> CircBuffer<B, $CX> {
                        /// Return the partial contents of the buffer half being written
                        pub fn partial_peek<R, F, T>(&mut self, f: F) -> Result<R, Error>
                            where
                            F: FnOnce(&[T], Half) -> Result<(usize, R), ()>,
                            B: Unsize<[T]>,
                        {
                            // this inverts expectation and returns the half being _written_
                            let buf = match self.readable_half {
                                Half::First => &self.buffer[1],
                                Half::Second => &self.buffer[0],
                            };

                            //                          ,- half-buffer
                            //    [ x x x x y y y y y z | z z z z z z z z z z ]
                            //                       ^- pending=11
                            let pending = self.channel.get_cndtr() as usize; // available bytes in _whole_ buffer

                            let slice: &[T] = buf;
                            let capacity = slice.len(); // capacity of _half_ a buffer
                            //     <--- capacity=10 --->
                            //    [ x x x x y y y y y z | z z z z z z z z z z ]

                            let pending = if pending > capacity {
                                pending - capacity
                            } else {
                                pending
                            };

                            //                          ,- half-buffer
                            //    [ x x x x y y y y y z | z z z z z z z z z z ]
                            //                       ^- pending=1

                            let end = capacity - pending;
                            //    [ x x x x y y y y y z | z z z z z z z z z z ]
                            //                       ^- end=9
                            //             ^- consumed_offset=4
                            //             [y y y y y] <-- slice
                            let slice = &slice[self.consumed_offset..end];

                            match f(slice, self.readable_half) {
                                Ok((l, r)) => { self.consumed_offset += l; Ok(r) },
                                Err(_) => Err(Error::BufferError),
                            }
                        }

                        /// Peeks into the readable half of the buffer
                        pub fn peek<R, F, T>(&mut self, f: F) -> Result<R, Error>
                            where
                            F: FnOnce(&[T], Half) -> R,
                            B: Unsize<[T]>,
                        {
                            let half_being_read = self.readable_half()?;

                            let buf = match half_being_read {
                                Half::First => &self.buffer[0],
                                Half::Second => &self.buffer[1],
                            };

                            let slice: &[T] = buf;
                            let slice = &slice[self.consumed_offset..];
                            self.consumed_offset = 0;
                            Ok(f(slice, half_being_read))
                        }

                        /// Returns the `Half` of the buffer that can be read
                        pub fn readable_half(&mut self) -> Result<Half, Error> {
                            let isr = self.channel.isr();
                            let first_half_is_done = isr.$htifX().bit_is_set();
                            let second_half_is_done = isr.$tcifX().bit_is_set();

                            if first_half_is_done && second_half_is_done {
                                return Err(Error::Overrun);
                            }

                            let last_read_half = self.readable_half;

                            Ok(match last_read_half {
                                Half::First => {
                                    if second_half_is_done {
                                        self.channel.ifcr().write(|w| w.$ctcifX().set_bit());

                                        self.readable_half = Half::Second;
                                        Half::Second
                                    } else {
                                        last_read_half
                                    }
                                }
                                Half::Second => {
                                    if first_half_is_done {
                                        self.channel.ifcr().write(|w| w.$chtifX().set_bit());

                                        self.readable_half = Half::First;
                                        Half::First
                                    } else {
                                        last_read_half
                                    }
                                }
                            })
                        }

                        pub fn answer_isr(&self) {
                            self.channel.ifcr().write(|w| w.$cgifX().set_bit());
                        }

                    }

                    impl<BUFFER, PAYLOAD, MODE> Transfer<MODE, BUFFER, $CX, PAYLOAD> {
                        pub fn is_done(&self) -> bool {
                            self.channel.isr().$tcifX().bit_is_set()
                        }

                        pub fn answer_isr(&self) {
                            self.channel.ifcr().write(|w| w.$cgifX().set_bit());
                        }

                        pub fn terminate(&mut self) {
                            if self.channel.ccr().read().en().bit_is_set() {
                                self.channel.ccr().modify(|_, w| w.en().clear_bit());
                                while self.channel.ccr().read().en().bit_is_set() {};
                            } else {
                                self.channel.ccr().modify(|_, w| w.en().set_bit());
                                while self.channel.ccr().read().en().bit_is_clear() {};
                            }
                        }

                        pub fn wait(mut self) -> (BUFFER, $CX, PAYLOAD) {
                            // XXX should we check for transfer errors here?
                            // The manual says "A DMA transfer error can be generated by reading
                            // from or writing to a reserved address space". I think it's impossible
                            // to get to that state with our type safe API and *safe* Rust.
                            while !self.is_done() {}

                            self.answer_isr();

                            self.terminate();

                            // TODO can we weaken this compiler barrier?
                            // NOTE(compiler_fence) operations on `buffer` should not be reordered
                            // before the previous statement, which marks the DMA transfer as done
                            atomic::compiler_fence(Ordering::SeqCst);

                            (self.buffer, self.channel, self.payload)
                        }

                    }

                    impl<BUFFER, PAYLOAD> Transfer<W, &'static mut BUFFER, $CX, PAYLOAD> {
                        pub fn peek<T>(&self) -> &[T]
                        where
                            BUFFER: Unsize<[T]>,
                        {
                            let pending = self.channel.get_cndtr() as usize;

                            let slice: &[T] = self.buffer;
                            let capacity = slice.len();

                            &slice[..(capacity - pending)]
                        }

                        /// Pause the transfer, read out the contents of the buffer, clear the
                        /// buffer, and restart the transfer
                        pub fn restart<T>(&mut self) -> Option<&[T]>
                        where
                            BUFFER: Unsize<[T]>
                        {
                            self.terminate();
                            //while !self.is_done() {}

                            atomic::compiler_fence(Ordering::SeqCst);

                            let pending = self.channel.get_cndtr() as usize;
                            let slice: &[T] = self.buffer;
                            let capacity = slice.len();

                            atomic::compiler_fence(Ordering::SeqCst);

                            self.channel.cndtr().write(|w| unsafe { w.ndt().bits(capacity as u16) });
                            self.channel.ccr().modify(|_,w| w.en().set_bit());

                            if pending != capacity {
                                Some(&slice[..(capacity - pending)])
                            } else {
                                None
                            }
                        }
                    }
                )+

                impl DmaExt for $DMAX {
                    type Channels = Channels;

                    fn split(self, ahb: &mut AHB) -> Channels {
                        ahb.enr1().modify(|_, w| w.$dmaXen().set_bit());

                        // reset the DMA control registers (stops all on-going transfers)
                        $(
                            self.$ccrX.reset();
                        )+

                        Channels((), $($CX { _0: () }),+)
                    }
                }
            }
        )+
    }
}

dma! {
    DMA1: (dma1, dma1en, dma1rst, {
        C1: (
            ccr1, CCR1,
            cndtr1, CNDTR1,
            cpar1, CPAR1,
            cmar1, CMAR1,
            htif1, tcif1,
            chtif1, ctcif1, cgif1, c1s
        ),
        C2: (
            ccr2, CCR2,
            cndtr2, CNDTR2,
            cpar2, CPAR2,
            cmar2, CMAR2,
            htif2, tcif2,
            chtif2, ctcif2, cgif2, c2s
        ),
        C3: (
            ccr3, CCR3,
            cndtr3, CNDTR3,
            cpar3, CPAR3,
            cmar3, CMAR3,
            htif3, tcif3,
            chtif3, ctcif3, cgif3, c3s
        ),
        C4: (
            ccr4, CCR4,
            cndtr4, CNDTR4,
            cpar4, CPAR4,
            cmar4, CMAR4,
            htif4, tcif4,
            chtif4, ctcif4, cgif4, c4s
        ),
        C5: (
            ccr5, CCR5,
            cndtr5, CNDTR5,
            cpar5, CPAR5,
            cmar5, CMAR5,
            htif5, tcif5,
            chtif5, ctcif5, cgif5, c5s
        ),
        C6: (
            ccr6, CCR6,
            cndtr6, CNDTR6,
            cpar6, CPAR6,
            cmar6, CMAR6,
            htif6, tcif6,
            chtif6, ctcif6, cgif6, c6s
        ),
        C7: (
            ccr7, CCR7,
            cndtr7, CNDTR7,
            cpar7, CPAR7,
            cmar7, CMAR7,
            htif7, tcif7,
            chtif7, ctcif7, cgif7, c7s
        ),
    }),

    DMA2: (dma2, dma2en, dma2rst, {
        C1: (
            ccr1, CCR1,
            cndtr1, CNDTR1,
            cpar1, CPAR1,
            cmar1, CMAR1,
            htif1, tcif1,
            chtif1, ctcif1, cgif1, c1s
        ),
        C2: (
            ccr2, CCR2,
            cndtr2, CNDTR2,
            cpar2, CPAR2,
            cmar2, CMAR2,
            htif2, tcif2,
            chtif2, ctcif2, cgif2, c2s
        ),
        C3: (
            ccr3, CCR3,
            cndtr3, CNDTR3,
            cpar3, CPAR3,
            cmar3, CMAR3,
            htif3, tcif3,
            chtif3, ctcif3, cgif3, c3s
        ),
        C4: (
            ccr4, CCR4,
            cndtr4, CNDTR4,
            cpar4, CPAR4,
            cmar4, CMAR4,
            htif4, tcif4,
            chtif4, ctcif4, cgif4, c4s
        ),
        C5: (
            ccr5, CCR5,
            cndtr5, CNDTR5,
            cpar5, CPAR5,
            cmar5, CMAR5,
            htif5, tcif5,
            chtif5, ctcif5, cgif5, c5s
        ),
        C6: (
            ccr6, CCR6,
            cndtr6, CNDTR6,
            cpar6, CPAR6,
            cmar6, CMAR6,
            htif6, tcif6,
            chtif6, ctcif6, cgif6, c6s
        ),
        C7: (
            ccr7, CCR7,
            cndtr7, CNDTR7,
            cpar7, CPAR7,
            cmar7, CMAR7,
            htif7, tcif7,
            chtif7, ctcif7, cgif7, c7s
        ),
    }),
}
