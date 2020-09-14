use futures_core::Stream;
use std::{future::Future, pin::Pin, task::Context, task::Poll};

#[macro_export]
macro_rules! pin {
    ($($x:ident),* $(,)?) => {
        $(
            let mut $x = $x;
            #[allow(unused_mut)]
            let mut $x = unsafe { ::core::pin::Pin::new_unchecked(&mut $x) };
        )*
    };
}

struct PollFn<F>(F);
impl<F> Unpin for PollFn<F> {}

impl<T, F> Future for PollFn<F>
where
    F: FnMut(&mut Context<'_>) -> Poll<T>,
{
    type Output = T;
    fn poll(mut self: Pin<&mut Self>, ctx: &mut Context<'_>) -> Poll<T> {
        (&mut self.0)(ctx)
    }
}

impl<T, F> Stream for PollFn<F>
where
    F: FnMut(&mut Context<'_>) -> Poll<Option<T>>,
{
    type Item = T;
    fn poll_next(mut self: Pin<&mut Self>, ctx: &mut Context<'_>) -> Poll<Option<T>> {
        (&mut self.0)(ctx)
    }
}

pub fn spin_on<T>(future: impl Future<Output = T>) -> T {
    use core::task::*;

    fn nothing_waker() -> RawWaker {
        static DATA: () = ();
        RawWaker::new(&DATA, &VTABLE)
    }

    unsafe fn clone(_p: *const ()) -> RawWaker {
        nothing_waker()
    }
    unsafe fn wake(_p: *const ()) {}
    unsafe fn wake_by_ref(_p: *const ()) {}
    unsafe fn wake_drop(_p: *const ()) {}

    static VTABLE: RawWakerVTable = RawWakerVTable::new(clone, wake, wake_by_ref, wake_drop);

    pin!(future);

    let waker = unsafe { Waker::from_raw(nothing_waker()) };
    let mut ctx = Context::from_waker(&waker);
    loop {
        if let Poll::Ready(output) = future.as_mut().poll(&mut ctx) {
            return output;
        }
        std::sync::atomic::spin_loop_hint()
    }
}

pub mod future {
    use futures_core::Stream;
    use std::{future::Future, task::Context, task::Poll};

    pub async fn ready<T>(value: T) -> T {
        value
    }

    pub async fn map<Fut, U, F>(fut: Fut, func: F) -> U
    where
        F: FnOnce(Fut::Output) -> U,
        Fut: Future,
    {
        func(fut.await)
    }

    pub async fn then<Fut, Then, F>(fut: Fut, func: F) -> Then::Output
    where
        F: FnOnce(Fut::Output) -> Then,
        Fut: Future,
        Then: Future,
    {
        func(fut.await).await
    }

    pub async fn and_then<Fut, And, F, T, U, E>(fut: Fut, func: F) -> Result<U, E>
    where
        F: FnOnce(T) -> And,
        Fut: Future<Output = Result<T, E>>,
        And: Future<Output = Result<U, E>>,
    {
        func(fut.await?).await
    }

    pub async fn or_else<Fut, Or, F, T, E, U>(fut: Fut, func: F) -> Result<T, U>
    where
        F: FnOnce(E) -> Or,
        Fut: Future<Output = Result<T, E>>,
        Or: Future<Output = Result<T, U>>,
    {
        match fut.await.map_err(func) {
            Ok(ok) => Ok(ok),
            Err(err) => err.await,
        }
    }

    pub async fn map_ok<Fut, F, T, U, E>(fut: Fut, func: F) -> Result<U, E>
    where
        F: FnOnce(T) -> U,
        Fut: Future<Output = Result<T, E>>,
    {
        fut.await.map(func)
    }

    pub async fn map_err<Fut, F, T, U, E>(fut: Fut, func: F) -> Result<T, U>
    where
        F: FnOnce(E) -> U,
        Fut: Future<Output = Result<T, E>>,
    {
        fut.await.map_err(func)
    }

    pub async fn flatten<Fut>(fut: Fut) -> <<Fut as Future>::Output as Future>::Output
    where
        Fut: Future,
        Fut::Output: Future,
    {
        fut.await.await
    }

    pub async fn inspect<Fut, F>(fut: Fut, func: F) -> Fut::Output
    where
        Fut: Future,
        F: FnOnce(&Fut::Output),
    {
        let res = fut.await;
        func(&res);
        res
    }

    pub async fn ok_into<Fut, U, T, E>(fut: Fut) -> Result<U, E>
    where
        Fut: Future<Output = Result<T, E>>,
        T: Into<U>,
    {
        fut.await.map(Into::into)
    }

    pub async fn err_into<Fut, U, T, E>(fut: Fut) -> Result<T, U>
    where
        Fut: Future<Output = Result<T, E>>,
        E: Into<U>,
    {
        fut.await.map_err(Into::into)
    }

    pub async fn unwrap_or_else<Fut, T, E, F>(fut: Fut, func: F) -> T
    where
        Fut: Future<Output = Result<T, E>>,
        F: FnOnce(E) -> T,
    {
        fut.await.unwrap_or_else(func)
    }

    pub fn poll_fn<F, T>(func: F) -> impl Future<Output = T>
    where
        F: FnMut(&mut Context<'_>) -> Poll<T>,
    {
        crate::PollFn(func)
    }

    pub fn into_stream<Fut>(fut: Fut) -> impl Stream<Item = Fut::Output>
    where
        Fut: Future,
    {
        crate::stream::once(fut)
    }
}

pub mod stream {
    use futures_core::Stream;
    use std::{future::Future, pin::Pin, task::Context, task::Poll};

    pub fn next<St>(mut stream: &mut St) -> impl Future<Output = Option<St::Item>> + '_
    where
        St: Stream + Unpin,
    {
        crate::future::poll_fn(move |ctx| Pin::new(&mut stream).poll_next(ctx))
    }

    pub fn once<Fut>(future: Fut) -> impl Stream<Item = Fut::Output>
    where
        Fut: Future,
    {
        unfold(Some(future), |f| async move { Some((f?.await, None)) })
    }

    pub async fn collect<St, B>(stream: St) -> B
    where
        St: Stream,
        B: Default + Extend<St::Item>,
    {
        pin!(stream);
        let mut target = B::default();
        while let Some(item) = next(&mut stream).await {
            target.extend(Some(item))
        }
        target
    }

    pub fn map<St, T, F>(stream: St, func: F) -> impl Stream<Item = T>
    where
        St: Stream,
        F: FnMut(St::Item) -> T,
    {
        let map_func = |(mut stream, mut func): (_, F)| async move {
            next(&mut stream)
                .await
                .map(|item| (func(item), (stream, func)))
        };
        unfold((Box::pin(stream), func), map_func)
    }

    pub fn filter<St, Fut, F>(stream: St, func: F) -> impl Stream<Item = St::Item>
    where
        St: Stream,
        F: FnMut(&St::Item) -> Fut,
        Fut: Future<Output = bool>,
    {
        let filter_func = |(mut stream, mut func): (_, F)| async move {
            loop {
                let item = next(&mut stream).await?;
                if func(&item).await {
                    break Some((item, (stream, func)));
                }
            }
        };
        unfold((Box::pin(stream), func), filter_func)
    }

    pub fn filter_map<St, Fut, F, T>(stream: St, func: F) -> impl Stream<Item = T>
    where
        St: Stream,
        F: FnMut(&St::Item) -> Fut,
        Fut: Future<Output = Option<T>>,
    {
        let filter_map_func = |(mut stream, mut func): (_, F)| async move {
            loop {
                let item = next(&mut stream).await?;
                if let Some(item) = func(&item).await {
                    break Some((item, (stream, func)));
                }
            }
        };
        unfold((Box::pin(stream), func), filter_map_func)
    }

    pub async fn into_future<St>(stream: St) -> (Option<St::Item>, impl Stream<Item = St::Item>)
    where
        St: Stream + Unpin,
    {
        let mut stream = stream;
        (next(&mut stream).await, stream)
    }

    pub fn from_iter<I>(iter: I) -> impl Stream<Item = I::Item>
    where
        I: IntoIterator,
    {
        poll_fn({
            let mut iter = iter.into_iter();
            move |_| iter.next().into()
        })
    }

    pub async fn concat<St>(stream: St) -> St::Item
    where
        St: Stream,
        St::Item: Extend<<St::Item as IntoIterator>::Item> + IntoIterator + Default,
    {
        pin!(stream);
        let mut target = <St::Item>::default();
        while let Some(item) = next(&mut stream).await {
            target.extend(item);
        }
        target
    }

    pub async fn try_for_each<St, Fut, F, E>(stream: St, func: F) -> Result<(), E>
    where
        St: Stream,
        F: FnMut(St::Item) -> Fut,
        Fut: Future<Output = Result<(), E>>,
    {
        pin!(stream);
        let mut func = func;
        while let Some(item) = next(&mut stream).await {
            if let Err(err) = func(item).await {
                return Err(err);
            }
        }
        Ok(())
    }

    pub async fn for_each<St, Fut, F>(stream: St, func: F)
    where
        St: Stream,
        F: FnMut(St::Item) -> Fut,
        Fut: Future<Output = ()>,
    {
        pin!(stream);
        let mut func = func;
        while let Some(item) = next(&mut stream).await {
            func(item).await
        }
    }

    pub fn take<St>(stream: St, limit: usize) -> impl Stream<Item = St::Item>
    where
        St: Stream,
    {
        let take_func = |(mut stream, limit)| async move {
            if limit == 0 {
                return None;
            }
            let item = next(&mut stream).await?;
            Some((item, (stream, limit - 1)))
        };
        unfold((Box::pin(stream), limit), take_func)
    }

    pub fn repeat<T>(item: T) -> impl Stream<Item = T>
    where
        T: Clone,
    {
        poll_fn(move |_| Some(item.clone()).into())
    }

    pub fn flatten<St, S, T>(stream: St) -> impl Stream<Item = T>
    where
        S: Stream<Item = T>,
        St: Stream<Item = S>,
    {
        type State<Z> = Option<Pin<Box<Z>>>;
        let flatten_func = |(mut stream, mut sub): (State<St>, State<S>)| async move {
            loop {
                if let Some(mut sub) = sub.take() {
                    if let Some(item) = next(&mut sub).await {
                        break Some((item, (stream, Some(sub))));
                    } else {
                        continue;
                    }
                }

                if let Some(mut left) = stream.take() {
                    if let Some(right) = next(&mut left).await {
                        stream.replace(left);
                        sub.replace(Box::pin(right));
                        continue;
                    }
                }

                break None;
            }
        };
        unfold((Some(Box::pin(stream)), None), flatten_func)
    }

    pub fn then<St, F, Fut>(stream: St, func: F) -> impl Stream<Item = St::Item>
    where
        St: Stream,
        F: FnMut(St::Item) -> Fut,
        Fut: Future<Output = St::Item>,
    {
        let then_func = |(mut stream, mut func): (_, F)| async move {
            let item = next(&mut stream).await?;
            Some((func(item).await, (stream, func)))
        };
        unfold((Box::pin(stream), func), then_func)
    }

    pub fn skip<St>(stream: St, limit: usize) -> impl Stream<Item = St::Item>
    where
        St: Stream,
    {
        let skip_func = |(mut stream, limit)| async move {
            for _ in 0..limit {
                next(&mut stream).await?;
            }
            Some((next(&mut stream).await?, (stream, 0)))
        };
        unfold((Box::pin(stream), limit), skip_func)
    }

    // TODO unzip

    pub fn zip<L, R>(left: L, right: R) -> impl Stream<Item = (L::Item, R::Item)>
    where
        L: Stream,
        R: Stream,
    {
        let zip_func = |(mut left, mut right)| async move {
            let l = next(&mut left).await?;
            let r = next(&mut right).await?;
            Some(((l, r), (left, right)))
        };
        unfold((Box::pin(left), Box::pin(right)), zip_func)
    }

    pub fn enumerate<St>(stream: St) -> impl Stream<Item = (usize, St::Item)>
    where
        St: Stream,
    {
        // TODO this boxes the counting stream, this could be done with just a simple unfold + counter
        zip(from_iter(0..), stream)
    }

    pub fn chain<St>(left: St, right: St) -> impl Stream<Item = St::Item>
    where
        St: Stream,
    {
        flatten(from_iter(Some(left).into_iter().chain(Some(right))))
    }

    pub fn take_while<St, F, Fut>(stream: St, func: F) -> impl Stream<Item = St::Item>
    where
        St: Stream,
        F: FnMut(&St::Item) -> Fut,
        Fut: Future<Output = bool>,
    {
        let take_func = |(mut stream, mut func): (_, F)| async move {
            let item = next(&mut stream).await?;
            if func(&item).await {
                return Some((item, (stream, func)));
            }
            None
        };
        unfold((Box::pin(stream), func), take_func)
    }

    pub fn skip_while<St, F, Fut>(stream: St, func: F) -> impl Stream<Item = St::Item>
    where
        St: Stream,
        F: FnMut(&St::Item) -> Fut,
        Fut: Future<Output = bool>,
    {
        let skip_func = |(mut stream, mut func, skip): (_, F, bool)| async move {
            if !skip {
                let item = next(&mut stream).await?;
                return Some((item, (stream, func, false)));
            }

            loop {
                let item = next(&mut stream).await?;
                if func(&item).await {
                    continue;
                }
                break Some((item, (stream, func, false)));
            }
        };
        unfold((Box::pin(stream), func, true), skip_func)
    }

    pub async fn fold<St, T, F, Fut>(stream: St, init: T, func: F) -> T
    where
        St: Stream,
        F: FnMut(T, St::Item) -> Fut,
        Fut: Future<Output = T>,
    {
        pin!(stream);
        let (mut func, mut acc) = (func, init);
        while let Some(item) = next(&mut stream).await {
            acc = func(acc, item).await;
        }
        acc
    }

    pub fn unfold<T, F, Fut, Item>(init: T, mut func: F) -> impl Stream<Item = Item>
    where
        F: FnMut(T) -> Fut,
        Fut: Future<Output = Option<(Item, T)>>,
    {
        let mut state = Some(init);
        let mut task = Box::pin(None);

        poll_fn(move |ctx| {
            if let Some(state) = state.take() {
                let fut = func(state);
                task.set(Some(fut));
            }

            let fut = Option::as_pin_mut(task.as_mut()) //
                .expect("this cannot be polled again");

            match futures_core::ready!(fut.poll(ctx)) {
                Some((item, next)) => {
                    state.replace(next);
                    task.set(None);
                    Poll::Ready(Some(item))
                }
                None => Poll::Ready(None),
            }
        })
    }

    pub fn poll_fn<T, F>(func: F) -> impl Stream<Item = T>
    where
        F: FnMut(&mut Context<'_>) -> Poll<Option<T>>,
    {
        crate::PollFn(func)
    }
}
