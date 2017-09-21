extern crate drm;
#[macro_use]
extern crate glium;
extern crate rand;
#[macro_use(define_roles)]
extern crate smithay;
extern crate wayland_server;

#[macro_use]
extern crate slog;
extern crate slog_async;
extern crate slog_term;

mod helpers;

use drm::control::{Device as ControlDevice, ResourceInfo};
use drm::control::connector::{Info as ConnectorInfo, State as ConnectorState};
use drm::control::encoder::Info as EncoderInfo;
use glium::Surface;
use helpers::{shell_implementation, surface_implementation, GliumDrawer, Roles, SurfaceData};
use slog::{Drain, Logger};
use smithay::backend::drm::{drm_device_bind, DrmBackend, DrmDevice, DrmHandler};
use smithay::backend::graphics::egl::EGLGraphicsBackend;
use smithay::compositor::{compositor_init, CompositorToken, SubsurfaceRole, TraversalAction};
use smithay::compositor::roles::Role;
use smithay::shell::{shell_init, ShellState};
use smithay::shm::init_shm_global;
use std::borrow::Borrow;
use std::fs::OpenOptions;
use std::io::Error as IoError;
use std::time::Duration;
use wayland_server::{EventLoopHandle, StateToken};

fn main() {
    // A logger facility, here we use the terminal for this example
    let log = Logger::root(
        slog_async::Async::default(slog_term::term_full().fuse()).fuse(),
        o!(),
    );

    // Initialize the wayland server
    let (mut display, mut event_loop) = wayland_server::create_display();

    /*
     * Initialize the drm backend
     */
    // "Find" a suitable drm device
    let mut options = OpenOptions::new();
    options.read(true);
    options.write(true);
    let mut device: DrmDevice<GliumDrawer<DrmBackend>> =
        DrmDevice::new_from_file(options.clone().open("/dev/dri/card0").unwrap(), log.clone()).unwrap();

    // Get a set of all modesetting resource handles (excluding planes):
    let res_handles = device.resource_handles().unwrap();

    // Use first connected connector
    let connector_info = res_handles
        .connectors()
        .iter()
        .map(|conn| {
            ConnectorInfo::load_from_device(&device, *conn).unwrap()
        })
        .find(|conn| conn.connection_state() == ConnectorState::Connected)
        .unwrap();

    // Use the first encoder
    let encoder_info = EncoderInfo::load_from_device(&device, connector_info.encoders()[0]).unwrap();

    // use the connected crtc if any
    let crtc = encoder_info.current_crtc()
        // or use the first one that is compatible with the encoder
        .unwrap_or_else(||
            *res_handles.crtcs()
            .iter()
            .find(|crtc| encoder_info.supports_crtc(**crtc))
            .unwrap());

    // Assuming we found a good connector and loaded the info into `connector_info`
    let mode = connector_info.modes()[0]; // Use first mode (usually highest resoltion, but in reality you should filter and sort and check and match with other connectors, if you use more then one.)

    // Initialize the hardware backend
    let renderer_token = device
        .create_backend(&mut event_loop, crtc, mode, vec![connector_info.handle()])
        .unwrap();

    /*
     * Initialize the globals
     */

    init_shm_global(&mut event_loop, vec![], log.clone());

    let (compositor_token, _, _) =
        compositor_init(&mut event_loop, surface_implementation(), (), log.clone());

    let (shell_state_token, _, _) = shell_init(
        &mut event_loop,
        compositor_token,
        shell_implementation(),
        compositor_token,
        log.clone(),
    );

    /*
     * Initialize glium
     */
    {
        let drawer = event_loop.state().get(&renderer_token);
        let mut frame = drawer.draw();
        frame.clear_color(0.8, 0.8, 0.9, 1.0);
        frame.finish().unwrap();
    }

    /*
     * Add a listening socket:
     */
    let name = display.add_socket_auto().unwrap().into_string().unwrap();
    println!("Listening on socket: {}", name);

    /*
     * Register the DrmDevice on the EventLoop
     */
    let _source = drm_device_bind(
        &mut event_loop,
        device,
        DrmHandlerImpl {
            shell_state_token,
            compositor_token,
            logger: log,
        },
    ).unwrap();

    event_loop.run().unwrap();
}

pub struct DrmHandlerImpl {
    shell_state_token: StateToken<ShellState<SurfaceData, Roles, (), ()>>,
    compositor_token: CompositorToken<SurfaceData, Roles, ()>,
    logger: ::slog::Logger,
}

impl DrmHandler<GliumDrawer<DrmBackend>> for DrmHandlerImpl {
    fn ready(&mut self, evlh: &mut EventLoopHandle, _device: &mut DrmDevice<GliumDrawer<DrmBackend>>,
             backend: &StateToken<GliumDrawer<DrmBackend>>, _frame: u32, _duration: Duration) {
        let state = evlh.state();
        let drawer = state.get(backend);
        let mut frame = drawer.draw();
        frame.clear_color(0.8, 0.8, 0.9, 1.0);
        // redraw the frame, in a simple but inneficient way
        {
            let screen_dimensions = (drawer.borrow() as &DrmBackend).get_framebuffer_dimensions();
            for toplevel_surface in state.get(&self.shell_state_token).toplevel_surfaces() {
                if let Some(wl_surface) = toplevel_surface.get_surface() {
                    // this surface is a root of a subsurface tree that needs to be drawn
                    let initial_place = self.compositor_token
                        .with_surface_data(wl_surface, |data| data.user_data.location.unwrap_or((0, 0)));
                    self.compositor_token
                        .with_surface_tree(
                            wl_surface,
                            initial_place,
                            |_surface, attributes, role, &(mut x, mut y)| {
                                if let Some((ref contents, (w, h))) = attributes.user_data.buffer {
                                    // there is actually something to draw !
                                    if let Ok(subdata) = Role::<SubsurfaceRole>::data(role) {
                                        x += subdata.x;
                                        y += subdata.y;
                                    }
                                    drawer.render(&mut frame, contents, (w, h), (x, y), screen_dimensions);
                                    TraversalAction::DoChildren((x, y))
                                } else {
                                    // we are not display, so our children are neither
                                    TraversalAction::SkipChildren
                                }
                            },
                        )
                        .unwrap();
                }
            }
        }
        frame.finish().unwrap();
    }

    fn error(&mut self, _evlh: &mut EventLoopHandle, _device: &mut DrmDevice<GliumDrawer<DrmBackend>>,
             error: IoError) {
        panic!("{:?}", error);
    }
}