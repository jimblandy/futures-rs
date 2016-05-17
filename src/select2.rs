use std::mem;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use {PollResult, Callback, Future, PollError};
use cell;
use slot::Slot;
use util;

pub struct Select2<A, B> where A: Future, B: Future<Item=A::Item, Error=A::Error> {
    state: State<A, B>,
}

pub struct Select2Next<A, B> where A: Future, B: Future<Item=A::Item, Error=A::Error> {
    state: Arc<Scheduled<A, B>>,
}

pub fn new<A, B>(a: A, b: B) -> Select2<A, B>
    where A: Future,
          B: Future<Item=A::Item, Error=A::Error>
{
    Select2 {
        state: State::Start(a, b),
    }
}

impl<A, B> Future for Select2<A, B>
    where A: Future,
          B: Future<Item=A::Item, Error=A::Error>,
{
    type Item = (A::Item, Select2Next<A, B>);
    type Error = (A::Error, Select2Next<A, B>);

    fn schedule<G>(&mut self, g: G)
        where G: FnOnce(PollResult<Self::Item, Self::Error>) + Send + 'static
    {
        // TODO: pretty unfortunate we gotta box this up
        self.schedule_boxed(Box::new(g))
    }

    fn schedule_boxed(&mut self, cb: Box<Callback<Self::Item, Self::Error>>) {
        let (mut a, mut b) = match mem::replace(&mut self.state, State::Canceled) {
            State::Start(a, b) => (a, b),
            State::Canceled => return cb.call(Err(PollError::Canceled)),
            State::Scheduled(s) => {
                self.state = State::Scheduled(s);
                return cb.call(Err(util::reused()))
            }
        };

        // TODO: optimize the case that either future is immediately done.
        let data1 = Arc::new(Scheduled {
            futures: cell::AtomicCell::new(None),
            state: AtomicUsize::new(0),
            cb: cell::AtomicCell::new(Some(cb)),
            data: Slot::new(None),
        });
        let data2 = data1.clone();
        let data3 = data2.clone();

        a.schedule(move |result| Scheduled::finish(data1, result));
        b.schedule(move |result| Scheduled::finish(data2, result));
        *data3.futures.borrow().expect("[s2] futures locked") = Some((a, b));

        // Inform our state flags that the futures are available to be canceled.
        // If the cancellation flag is set then we never turn SET on and instead
        // we just cancel the futures and go on our merry way.
        let mut state = data3.state.load(Ordering::SeqCst);
        loop {
            assert!(state & SET == 0);
            if state & CANCEL != 0 {
                assert!(state & DONE != 0);
                data3.cancel();
                break
            }
            let old = data3.state.compare_and_swap(state, state | SET,
                                                   Ordering::SeqCst);
            if old == state {
                break
            }
            state = old;
        }

        self.state = State::Scheduled(data3);
    }
}

enum State<A, B> where A: Future, B: Future<Item=A::Item, Error=A::Error> {
    Start(A, B),
    Scheduled(Arc<Scheduled<A, B>>),
    Canceled,
}

const DONE: usize = 1 << 0;
const CANCEL: usize = 1 << 1;
const SET: usize = 1 << 2;

struct Scheduled<A, B>
    where A: Future,
          B: Future<Item=A::Item, Error=A::Error>,
{
    futures: cell::AtomicCell<Option<(A, B)>>,
    state: AtomicUsize,
    cb: cell::AtomicCell<Option<Box<Callback<(A::Item, Select2Next<A, B>),
                                             (A::Error, Select2Next<A, B>)>>>>,
    data: Slot<PollResult<A::Item, A::Error>>,
}

impl<A, B> Scheduled<A, B>
    where A: Future,
          B: Future<Item=A::Item, Error=A::Error>,
{
    fn finish(me: Arc<Scheduled<A, B>>,
              val: PollResult<A::Item, A::Error>) {
        let old = me.state.fetch_or(DONE, Ordering::SeqCst);

        // if the other side finished before we did then we just drop our result
        // on the ground and let them take care of everything.
        if old & DONE != 0 {
            me.data.try_produce(val).ok().unwrap();
            return
        }

        let cb = me.cb.borrow().expect("[s2] done but cb is locked")
                      .take().expect("[s2] done done but cb not here");
        let next = Select2Next { state: me };
        cb.call(match val {
            Ok(v) => Ok((v, next)),
            Err(PollError::Other(e)) => Err(PollError::Other((e, next))),
            Err(PollError::Panicked(p)) => Err(PollError::Panicked(p)),
            Err(PollError::Canceled) => Err(PollError::Canceled),
        })
    }

    fn cancel(&self) {
        let pair = self.futures.borrow().expect("[s2] futures locked in cancel")
                               .take().expect("[s2] cancel but futures not here");
        drop(pair)
    }
}

impl<A, B> Drop for Select2<A, B>
    where A: Future,
          B: Future<Item=A::Item, Error=A::Error>
{
    fn drop(&mut self) {
        if let State::Scheduled(ref state) = self.state {
            // If the old state was "nothing has happened", then we cancel both
            // futures. Otherwise one future has finished which implies that the
            // future we returned to that closure is responsible for canceling
            // itself.
            let old = state.state.compare_and_swap(SET, 0, Ordering::SeqCst);
            if old == SET {
                state.cancel();
            }
        }
    }
}

impl<A, B> Future for Select2Next<A, B>
    where A: Future,
          B: Future<Item=A::Item, Error=A::Error>
{
    type Item = A::Item;
    type Error = A::Error;

    fn schedule<G>(&mut self, g: G)
        where G: FnOnce(PollResult<Self::Item, Self::Error>) + Send + 'static
    {
        self.state.data.on_full(|slot| {
            g(slot.try_consume().unwrap());
        });
    }

    fn schedule_boxed(&mut self, cb: Box<Callback<Self::Item, Self::Error>>) {
        self.schedule(|r| cb.call(r))
    }
}

impl<A, B> Drop for Select2Next<A, B>
    where A: Future,
          B: Future<Item=A::Item, Error=A::Error>
{
    fn drop(&mut self) {
        let mut state = self.state.state.load(Ordering::SeqCst);
        loop {
            // We should in theory only be here if one half is done and we
            // haven't canceled yet.
            assert!(state & CANCEL == 0);
            assert!(state & DONE != 0);

            // Our next state will indicate that we are canceled, and if the
            // futures are available to us we're gonna take them.
            let next = state | CANCEL & !SET;
            let old = self.state.state.compare_and_swap(state, next,
                                                        Ordering::SeqCst);
            if old == state {
                break
            }
            state = old
        }

        // If the old state indicated that we had the futures, then we just took
        // ownership of them so we cancel the futures here.
        if state & SET != 0 {
            self.state.cancel();
        }
    }
}