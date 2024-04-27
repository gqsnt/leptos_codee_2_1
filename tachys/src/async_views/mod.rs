use crate::{
    hydration::Cursor,
    renderer::Renderer,
    ssr::StreamBuilder,
    view::{
        either::EitherState, Mountable, Position, PositionState, Render,
        RenderHtml,
    },
};
use any_spawner::Executor;
use either_of::Either;
use futures::FutureExt;
use parking_lot::RwLock;
use std::{cell::RefCell, fmt::Debug, future::Future, rc::Rc, sync::Arc};

pub struct SuspenseBoundary<const TRANSITION: bool, Fal, Chil> {
    in_fallback: bool,
    fallback: Option<Fal>,
    children: Chil,
}

impl<const TRANSITION: bool, Fal, Chil>
    SuspenseBoundary<TRANSITION, Fal, Chil>
{
    pub fn new(
        in_fallback: bool,
        fallback: Option<Fal>,
        children: Chil,
    ) -> Self {
        Self {
            in_fallback,
            fallback,
            children,
        }
    }
}

pub trait FutureViewExt: Sized {
    fn suspend(self) -> Suspend<false, (), Self>
    where
        Self: Future,
    {
        Suspend {
            fallback: (),
            fut: self,
        }
    }
}

impl<F> FutureViewExt for F where F: Future + Sized {}

pub struct Suspend<const TRANSITION: bool, Fal, Fut> {
    pub fallback: Fal,
    pub fut: Fut,
}

impl<const TRANSITION: bool, Fal, Fut> Suspend<TRANSITION, Fal, Fut> {
    pub fn with_fallback<Fal2>(
        self,
        fallback: Fal2,
    ) -> Suspend<TRANSITION, Fal2, Fut> {
        let fut = self.fut;
        Suspend { fallback, fut }
    }

    pub fn transition(self) -> Suspend<true, Fal, Fut> {
        let Suspend { fallback, fut } = self;
        Suspend { fallback, fut }
    }
}

impl<const TRANSITION: bool, Fal, Fut> Debug for Suspend<TRANSITION, Fal, Fut> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SuspendedFuture")
            .field("transition", &TRANSITION)
            .finish()
    }
}

// TODO make this cancelable
impl<const TRANSITION: bool, Fal, Fut, Rndr> Render<Rndr>
    for Suspend<TRANSITION, Fal, Fut>
where
    Fal: Render<Rndr> + 'static,
    Fut: Future + 'static,
    Fut::Output: Render<Rndr>,
    Rndr: Renderer + 'static,
{
    type State = Arc<
        RwLock<
            EitherState<Fal::State, <Fut::Output as Render<Rndr>>::State, Rndr>,
        >,
    >;
    // TODO fallible state/error

    fn build(self) -> Self::State {
        // poll the future once immediately
        // if it's already available, start in the ready state
        // otherwise, start with the fallback
        let mut fut = Box::pin(self.fut);
        let initial = match fut.as_mut().now_or_never() {
            Some(resolved) => Either::Right(resolved),
            None => Either::Left(self.fallback),
        };

        // store whether this was pending at first
        // by the time we need to know, we will have consumed `initial`
        let initially_pending = matches!(initial, Either::Left(_));

        // now we can build the initial state
        let state = Arc::new(RwLock::new(initial.build()));

        // if the initial state was pending, spawn a future to wait for it
        // spawning immediately means that our now_or_never poll result isn't lost
        // if it wasn't pending at first, we don't need to poll the Future again
        if initially_pending {
            Executor::spawn_local({
                let state = Arc::clone(&state);
                async move {
                    let value = fut.as_mut().await;
                    Either::<Fal, Fut::Output>::Right(value)
                        .rebuild(&mut *state.write());
                }
            });
        }

        state
    }

    fn rebuild(self, state: &mut Self::State) {
        if !TRANSITION {
            // fall back to fallback state
            Either::<Fal, Fut::Output>::Left(self.fallback)
                .rebuild(&mut *state.write());
        }

        // spawn the future, and rebuild the state when it resolves
        Executor::spawn_local({
            let state = Arc::clone(state);
            async move {
                let value = self.fut.await;
                Either::<Fal, Fut::Output>::Right(value)
                    .rebuild(&mut *state.write());
            }
        });
    }
}

impl<const TRANSITION: bool, Fal, Fut, Rndr> RenderHtml<Rndr>
    for Suspend<TRANSITION, Fal, Fut>
where
    Fal: RenderHtml<Rndr> + 'static,
    Fut: Future + Send + 'static,
    Fut::Output: RenderHtml<Rndr> + Send,
    Rndr: Renderer + 'static,
{
    type AsyncOutput = Fut::Output;

    const MIN_LENGTH: usize = Fal::MIN_LENGTH;

    fn resolve(self) -> impl Future<Output = Self::AsyncOutput> + Send {
        self.fut
    }

    fn to_html_with_buf(self, buf: &mut String, position: &mut Position) {
        Either::<Fal, Fut::Output>::Left(self.fallback)
            .to_html_with_buf(buf, position);
    }

    fn to_html_async_with_buf<const OUT_OF_ORDER: bool>(
        self,
        buf: &mut StreamBuilder,
        position: &mut Position,
    ) where
        Self: Sized,
    {
        buf.next_id();

        let mut fut = Box::pin(self.fut);
        match fut.as_mut().now_or_never() {
            Some(resolved) => {
                Either::<Fal, Fut::Output>::Right(resolved)
                    .to_html_async_with_buf::<OUT_OF_ORDER>(buf, position);
            }
            None => {
                let id = buf.clone_id();

                // out-of-order streams immediately push fallback,
                // wrapped by suspense markers
                if OUT_OF_ORDER {
                    buf.push_fallback(self.fallback, position);
                    buf.push_async_out_of_order(
                        false, /* TODO should_block */ fut, position,
                    );
                } else {
                    buf.push_async(
                        false, // TODO should_block
                        {
                            let mut position = *position;
                            async move {
                                let value = fut.await;
                                let mut builder = StreamBuilder::new(id);
                                Either::<Fal, Fut::Output>::Right(value)
                                    .to_html_async_with_buf::<OUT_OF_ORDER>(
                                    &mut builder,
                                    &mut position,
                                );
                                builder.finish().take_chunks()
                            }
                        },
                    );
                    *position = Position::NextChild;
                }
            }
        };
    }

    fn hydrate<const FROM_SERVER: bool>(
        self,
        cursor: &Cursor<Rndr>,
        position: &PositionState,
    ) -> Self::State {
        // poll the future once immediately
        // if it's already available, start in the ready state
        // otherwise, start with the fallback
        let mut fut = Box::pin(self.fut);
        let initial = match fut.as_mut().now_or_never() {
            Some(resolved) => Either::Right(resolved),
            None => Either::Left(self.fallback),
        };

        // store whether this was pending at first
        // by the time we need to know, we will have consumed `initial`
        let initially_pending = matches!(initial, Either::Left(_));

        // now we can build the initial state
        let state = Arc::new(RwLock::new(
            initial.hydrate::<FROM_SERVER>(cursor, position),
        ));

        // if the initial state was pending, spawn a future to wait for it
        // spawning immediately means that our now_or_never poll result isn't lost
        // if it wasn't pending at first, we don't need to poll the Future again
        if initially_pending {
            Executor::spawn_local({
                let state = Arc::clone(&state);
                async move {
                    let value = fut.as_mut().await;
                    Either::<Fal, Fut::Output>::Right(value)
                        .rebuild(&mut *state.write());
                }
            });
        }

        state
    }
}

impl<Rndr, Fal, Output> Mountable<Rndr>
    for Arc<RwLock<EitherState<Fal, Output, Rndr>>>
where
    Fal: Mountable<Rndr>,
    Output: Mountable<Rndr>,
    Rndr: Renderer,
{
    fn unmount(&mut self) {
        self.write().unmount();
    }

    fn mount(
        &mut self,
        parent: &<Rndr as Renderer>::Element,
        marker: Option<&<Rndr as Renderer>::Node>,
    ) {
        self.write().mount(parent, marker);
    }

    fn insert_before_this(
        &self,
        parent: &<Rndr as Renderer>::Element,
        child: &mut dyn Mountable<Rndr>,
    ) -> bool {
        self.write().insert_before_this(parent, child)
    }
}

impl<Rndr, Fal, Output> Mountable<Rndr>
    for Rc<RefCell<EitherState<Fal, Output, Rndr>>>
where
    Fal: Mountable<Rndr>,
    Output: Mountable<Rndr>,
    Rndr: Renderer,
{
    fn unmount(&mut self) {
        self.borrow_mut().unmount();
    }

    fn mount(
        &mut self,
        parent: &<Rndr as Renderer>::Element,
        marker: Option<&<Rndr as Renderer>::Node>,
    ) {
        self.borrow_mut().mount(parent, marker);
    }

    fn insert_before_this(
        &self,
        parent: &<Rndr as Renderer>::Element,
        child: &mut dyn Mountable<Rndr>,
    ) -> bool {
        self.borrow_mut().insert_before_this(parent, child)
    }
}