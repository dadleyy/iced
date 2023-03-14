//! Create interactive, native cross-platform applications.
use crate::mouse;
use crate::{Error, Executor, Runtime};

pub use iced_winit::application::StyleSheet;
pub use iced_winit::Application;

use iced_graphics::window;
use iced_winit::application;
use iced_winit::conversion;
use iced_winit::futures;
use iced_winit::futures::channel::mpsc;
use iced_winit::renderer;
use iced_winit::time::Instant;
use iced_winit::user_interface;
use iced_winit::{Clipboard, Command, Debug, Event, Proxy, Settings};

use glutin::window::Window;
use std::mem::ManuallyDrop;

#[cfg(feature = "tracing")]
use tracing::{info_span, instrument::Instrument};

/// Runs an [`Application`] with an executor, compositor, and the provided
/// settings.
pub fn run<A, E, C>(
    settings: Settings<A::Flags>,
    compositor_settings: C::Settings,
) -> Result<(), Error>
where
    A: Application + 'static,
    E: Executor + 'static,
    C: window::GLCompositor<Renderer = A::Renderer> + 'static,
    <A::Renderer as iced_native::Renderer>::Theme: StyleSheet,
{
    use futures::task;
    use futures::Future;
    use glutin::event_loop::EventLoopBuilder;
    use glutin::platform::run_return::EventLoopExtRunReturn;
    use glutin::ContextBuilder;

    #[cfg(feature = "trace")]
    let _guard = iced_winit::Profiler::init();

    let mut debug = Debug::new();
    debug.startup_started();

    #[cfg(feature = "tracing")]
    let _ = info_span!("Application::Glutin", "RUN").entered();

    let mut event_loop = EventLoopBuilder::with_user_event().build();
    let proxy = event_loop.create_proxy();

    let runtime = {
        let executor = E::new().map_err(Error::ExecutorCreationFailed)?;
        let proxy = Proxy::new(event_loop.create_proxy());

        Runtime::new(executor, proxy)
    };

    let (application, init_command) = {
        let flags = settings.flags;

        runtime.enter(|| A::new(flags))
    };

    let context = {
        let builder = settings.window.into_builder(
            &application.title(),
            event_loop.primary_monitor(),
            settings.id,
        );

        log::info!("Window builder: {:#?}", builder);

        let opengl_builder = ContextBuilder::new()
            .with_vsync(true)
            .with_multisampling(C::sample_count(&compositor_settings) as u16);

        let opengles_builder = opengl_builder.clone().with_gl(
            glutin::GlRequest::Specific(glutin::Api::OpenGlEs, (2, 0)),
        );

        let (first_builder, second_builder) = if settings.try_opengles_first {
            log::debug!("attempting to use opengles before opengl");
            (opengles_builder, opengl_builder)
        } else {
            log::debug!("attempting to use opengl before opengles");
            (opengl_builder, opengles_builder)
        };

        log::info!("Trying first builder: {:#?}", first_builder);

        let context = first_builder
            .build_windowed(builder.clone(), &event_loop)
            .or_else(|_| {
                log::info!("Trying second builder: {:#?}", second_builder);
                second_builder.build_windowed(builder, &event_loop)
            })
            .map_err(|error| {
                use glutin::CreationError;
                use iced_graphics::Error as ContextError;

                match error {
                    CreationError::Window(error) => {
                        Error::WindowCreationFailed(error)
                    }
                    CreationError::OpenGlVersionNotSupported => {
                        Error::GraphicsCreationFailed(
                            ContextError::VersionNotSupported,
                        )
                    }
                    CreationError::NoAvailablePixelFormat => {
                        Error::GraphicsCreationFailed(
                            ContextError::NoAvailablePixelFormat,
                        )
                    }
                    error => Error::GraphicsCreationFailed(
                        ContextError::BackendError(error.to_string()),
                    ),
                }
            })?;

        #[allow(unsafe_code)]
        unsafe {
            log::debug!("attempting to make context current");
            context.make_current().expect("Make OpenGL context current")
        }
    };

    #[allow(unsafe_code)]
    let (compositor, renderer) = unsafe {
        log::debug!("getting proc address");
        C::new(compositor_settings, |address| {
            context.get_proc_address(address)
        })?
    };

    let (mut event_sender, event_receiver) = mpsc::unbounded();
    let (control_sender, mut control_receiver) = mpsc::unbounded();

    let mut instance = Box::pin({
        let run_instance = run_instance::<A, E, C>(
            application,
            compositor,
            renderer,
            runtime,
            proxy,
            debug,
            event_receiver,
            control_sender,
            context,
            init_command,
            settings.exit_on_close_request,
        );

        #[cfg(feature = "tracing")]
        let run_instance =
            run_instance.instrument(info_span!("Application", "LOOP"));

        run_instance
    });

    let mut context = task::Context::from_waker(task::noop_waker_ref());
    let mut last_event_time = None;

    let _ = event_loop.run_return(move |event, _, control_flow| {
        use glutin::event_loop::ControlFlow;

        if let ControlFlow::ExitWithCode(_) = control_flow {
            return;
        }

        let event = match event {
            glutin::event::Event::WindowEvent {
                event:
                    glutin::event::WindowEvent::ScaleFactorChanged {
                        new_inner_size,
                        ..
                    },
                window_id,
            } => Some(glutin::event::Event::WindowEvent {
                event: glutin::event::WindowEvent::Resized(*new_inner_size),
                window_id,
            }),
            _ => event.to_static(),
        };

        if let Some(event) = event {
            let now = std::time::Instant::now();
            if let Some(last) = &last_event_time {
                let is_mouse = matches!(
                    event,
                    glutin::event::Event::WindowEvent {
                        event: glutin::event::WindowEvent::MouseInput { .. },
                        ..
                    }
                );

                let is_start = matches!(
                    event,
                    glutin::event::Event::WindowEvent {
                        event: glutin::event::WindowEvent::Touch(glutin::event::Touch { phase: glutin::event::TouchPhase::Started, .. }),
                        ..
                    }
                );

                let is_finish = matches!(
                    event,
                    glutin::event::Event::WindowEvent {
                        event: glutin::event::WindowEvent::Touch(glutin::event::Touch { phase: glutin::event::TouchPhase::Ended, .. }),
                        ..
                    }
                );

                if is_mouse || is_start || is_finish {
                    log::debug!(
                        "[EVENTLOOP] time since last event in loop - {}ms (current: {event:?})",
                        now.duration_since(*last).as_millis()
                    );
            last_event_time = Some(now);
                } else {
            last_event_time = Some(now);
                }
            }

            event_sender.start_send(event).expect("Send event");

            let poll = instance.as_mut().poll(&mut context);

            match poll {
                task::Poll::Pending => {
                    if let Ok(Some(flow)) = control_receiver.try_next() {
                        *control_flow = flow;
                    }
                }
                task::Poll::Ready(_) => {
                    *control_flow = ControlFlow::Exit;
                }
            }
        }
    });

    Ok(())
}

async fn run_instance<A, E, C>(
    mut application: A,
    mut compositor: C,
    mut renderer: A::Renderer,
    mut runtime: Runtime<E, Proxy<A::Message>, A::Message>,
    mut proxy: glutin::event_loop::EventLoopProxy<A::Message>,
    mut debug: Debug,
    mut event_receiver: mpsc::UnboundedReceiver<
        glutin::event::Event<'_, A::Message>,
    >,
    mut control_sender: mpsc::UnboundedSender<glutin::event_loop::ControlFlow>,
    mut context: glutin::ContextWrapper<glutin::PossiblyCurrent, Window>,
    init_command: Command<A::Message>,
    exit_on_close_request: bool,
) where
    A: Application + 'static,
    E: Executor + 'static,
    C: window::GLCompositor<Renderer = A::Renderer> + 'static,
    <A::Renderer as iced_native::Renderer>::Theme: StyleSheet,
{
    use glutin::event;
    use glutin::event_loop::ControlFlow;
    use iced_winit::futures::stream::StreamExt;

    let mut clipboard = Clipboard::connect(context.window());
    let mut cache = user_interface::Cache::default();
    let mut state = application::State::new(&application, context.window());
    let mut viewport_version = state.viewport_version();
    let mut should_exit = false;

    application::run_command(
        &application,
        &mut cache,
        &state,
        &mut renderer,
        init_command,
        &mut runtime,
        &mut clipboard,
        &mut should_exit,
        &mut proxy,
        &mut debug,
        context.window(),
        || compositor.fetch_information(),
    );
    runtime.track(application.subscription());

    let mut user_interface =
        ManuallyDrop::new(application::build_user_interface(
            &application,
            user_interface::Cache::default(),
            &mut renderer,
            state.logical_size(),
            &mut debug,
        ));

    let mut mouse_interaction = mouse::Interaction::default();
    let mut events = Vec::new();
    let mut messages = Vec::new();
    let mut redraw_pending = false;

    debug.startup_finished();

    let mut last_clear = None;

    while let Some(event) = event_receiver.next().await {
        match event {
            event::Event::NewEvents(start_cause) => {
                redraw_pending = matches!(
                    start_cause,
                    event::StartCause::Init
                        | event::StartCause::Poll
                        | event::StartCause::ResumeTimeReached { .. }
                );
            }
            event::Event::MainEventsCleared => {
                if !redraw_pending && events.is_empty() && messages.is_empty() {
                    continue;
                }

                let full_start = std::time::Instant::now();

                log::debug!(
                    "----- 'MainEventsCleared' w/ {} events",
                    events.len()
                );
                let now = std::time::Instant::now();
                if let Some(last) = &last_clear {
                    log::debug!(
                        "-- time since last maineventscleared - {}ms",
                        now.duration_since(*last).as_millis()
                    );
                }
                last_clear = Some(now);

                debug.event_processing_started();

                let start = std::time::Instant::now();
                let (interface_state, statuses) = user_interface.update(
                    &events,
                    state.cursor_position(),
                    &mut renderer,
                    &mut clipboard,
                    &mut messages,
                );
                log::debug!(
                    "-- 'user_interface' updated in {} millis",
                    std::time::Instant::now().duration_since(start).as_millis()
                );

                debug.event_processing_finished();

                log::debug!("======= event log start ========");
                let start = std::time::Instant::now();
                for event in events.drain(..).zip(statuses.into_iter()) {
                    if matches!(
                        event.0,
                        Event::Touch(
                            iced_winit::touch::Event::FingerPressed { .. }
                        )
                    ) || matches!(
                        event.0,
                        Event::Touch(
                            iced_winit::touch::Event::FingerLifted { .. }
                        )
                    ) {
                        log::debug!("{event:?}");
                    }
                    runtime.broadcast(event);
                }
                log::debug!("======= event log end ========");
                log::debug!(
                    "-- 'broadcast' finished in {}ms; has messages? {}",
                    std::time::Instant::now().duration_since(start).as_millis(),
                    !messages.is_empty()
                );

                if !messages.is_empty()
                    || matches!(
                        interface_state,
                        user_interface::State::Outdated
                    )
                {
                    let mut cache =
                        ManuallyDrop::into_inner(user_interface).into_cache();

                    log::debug!("-- applying application update");
                    let start = std::time::Instant::now();
                    // Update application
                    application::update(
                        &mut application,
                        &mut cache,
                        &state,
                        &mut renderer,
                        &mut runtime,
                        &mut clipboard,
                        &mut should_exit,
                        &mut proxy,
                        &mut debug,
                        &mut messages,
                        context.window(),
                        || compositor.fetch_information(),
                    );
                    log::debug!(
                        "-- 'application' updated in {}ms",
                        std::time::Instant::now()
                            .duration_since(start)
                            .as_millis()
                    );

                    // Update window
                    state.synchronize(&application, context.window());

                    user_interface =
                        ManuallyDrop::new(application::build_user_interface(
                            &application,
                            cache,
                            &mut renderer,
                            state.logical_size(),
                            &mut debug,
                        ));

                    if should_exit {
                        break;
                    }
                } else {
                    log::warn!("-- skipping application update");
                }

                // TODO: Avoid redrawing all the time by forcing widgets to
                // request redraws on state changes
                //
                // Then, we can use the `interface_state` here to decide if a redraw
                // is needed right away, or simply wait until a specific time.
                let redraw_event = Event::Window(
                    crate::window::Event::RedrawRequested(Instant::now()),
                );

                let (interface_state, _) = user_interface.update(
                    &[redraw_event.clone()],
                    state.cursor_position(),
                    &mut renderer,
                    &mut clipboard,
                    &mut messages,
                );

                debug.draw_started();
                let new_mouse_interaction = user_interface.draw(
                    &mut renderer,
                    state.theme(),
                    &renderer::Style {
                        text_color: state.text_color(),
                    },
                    state.cursor_position(),
                );
                debug.draw_finished();

                if new_mouse_interaction != mouse_interaction {
                    context.window().set_cursor_icon(
                        conversion::mouse_interaction(new_mouse_interaction),
                    );

                    mouse_interaction = new_mouse_interaction;
                }

                context.window().request_redraw();
                runtime
                    .broadcast((redraw_event, crate::event::Status::Ignored));

                let _ = control_sender.start_send(match interface_state {
                    user_interface::State::Updated {
                        redraw_request: Some(redraw_request),
                    } => match redraw_request {
                        crate::window::RedrawRequest::NextFrame => {
                            ControlFlow::Poll
                        }
                        crate::window::RedrawRequest::At(at) => {
                            ControlFlow::WaitUntil(at)
                        }
                    },
                    _ => ControlFlow::Wait,
                });

                let finished_time = std::time::Instant::now();
                log::debug!(
                    "~~~ maineventscleared finished in {}ms",
                    finished_time.duration_since(full_start).as_millis()
                );
                redraw_pending = false;
            }
            event::Event::UserEvent(message) => {
                messages.push(message);
            }
            event::Event::RedrawRequested(_) => {
                let redraw_start = std::time::Instant::now();
                #[cfg(feature = "tracing")]
                let _ = info_span!("Application", "FRAME").entered();

                debug.render_started();

                #[allow(unsafe_code)]
                unsafe {
                    if !context.is_current() {
                        context = context
                            .make_current()
                            .expect("Make OpenGL context current");
                    }
                }

                let current_viewport_version = state.viewport_version();

                if viewport_version != current_viewport_version {
                    let physical_size = state.physical_size();
                    let logical_size = state.logical_size();

                    let start_relayout = std::time::Instant::now();
                    debug.layout_started();
                    user_interface = ManuallyDrop::new(
                        ManuallyDrop::into_inner(user_interface)
                            .relayout(logical_size, &mut renderer),
                    );
                    log::debug!(
                        "__ relayout took {}ms",
                        std::time::Instant::now()
                            .duration_since(start_relayout)
                            .as_millis(),
                    );
                    debug.layout_finished();

                    debug.draw_started();
                    let start_draw = std::time::Instant::now();
                    let new_mouse_interaction = user_interface.draw(
                        &mut renderer,
                        state.theme(),
                        &renderer::Style {
                            text_color: state.text_color(),
                        },
                        state.cursor_position(),
                    );
                    log::debug!(
                        "__ draw took {}ms",
                        std::time::Instant::now()
                            .duration_since(start_draw)
                            .as_millis(),
                    );

                    debug.draw_finished();

                    let mouse_resize = std::time::Instant::now();
                    if new_mouse_interaction != mouse_interaction {
                        context.window().set_cursor_icon(
                            conversion::mouse_interaction(
                                new_mouse_interaction,
                            ),
                        );

                        mouse_interaction = new_mouse_interaction;
                    }

                    context.resize(glutin::dpi::PhysicalSize::new(
                        physical_size.width,
                        physical_size.height,
                    ));

                    compositor.resize_viewport(physical_size);

                    viewport_version = current_viewport_version;
                    log::debug!(
                        "__ resize/mouse took {}ms",
                        std::time::Instant::now()
                            .duration_since(mouse_resize)
                            .as_millis(),
                    );
                }

                let start_present = std::time::Instant::now();
                compositor.present(
                    &mut renderer,
                    state.viewport(),
                    state.background_color(),
                    &debug.overlay(),
                );
                log::debug!(
                    "__ present took {}ms",
                    std::time::Instant::now()
                        .duration_since(start_present)
                        .as_millis(),
                );

                context.swap_buffers().expect("Swap buffers");

                debug.render_finished();
                log::debug!(
                    "[REDRAW] redraw event finished in {}ms",
                    std::time::Instant::now()
                        .duration_since(redraw_start)
                        .as_millis()
                );

                // TODO: Handle animations!
                // Maybe we can use `ControlFlow::WaitUntil` for this.
            }
            event::Event::WindowEvent {
                event: window_event,
                ..
            } => {
                if application::requests_exit(&window_event, state.modifiers())
                    && exit_on_close_request
                {
                    break;
                }

                state.update(context.window(), &window_event, &mut debug);

                if let Some(event) = conversion::window_event(
                    &window_event,
                    state.scale_factor(),
                    state.modifiers(),
                ) {
                    events.push(event);
                }
            }
            _ => {}
        }
    }

    // Manually drop the user interface
    drop(ManuallyDrop::into_inner(user_interface));
}
