//! Headless backend.
//!
//! This backend is used in two ways:
//!
//! - In tests, where outputs are added manually and nothing is actually rendered.
//! - For running niri without any physical outputs (`niri --headless`), for example as a remote
//!   desktop session. Here, all outputs are virtual and get created on demand, e.g. when a remote
//!   desktop client asks for a virtual monitor through the Mutter ScreenCast D-Bus interface.

use std::cell::RefCell;
use std::collections::HashMap;
use std::mem;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Context as _;
use niri_config::OutputName;
use smithay::backend::allocator::dmabuf::Dmabuf;
use smithay::backend::drm::DrmNode;
use smithay::backend::egl::native::EGLSurfacelessDisplay;
use smithay::backend::egl::{EGLContext, EGLDevice, EGLDisplay};
use smithay::backend::renderer::damage::OutputDamageTracker;
use smithay::backend::renderer::element::RenderElementStates;
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::backend::renderer::{ImportDma, ImportEgl};
use smithay::desktop::utils::OutputPresentationFeedback;
use smithay::output::{Mode, Output, PhysicalProperties, Subpixel};
use smithay::reexports::calloop::timer::{TimeoutAction, Timer};
use smithay::reexports::wayland_protocols::wp::presentation_time::server::wp_presentation_feedback;
use smithay::utils::{Physical, Size};
use smithay::wayland::dmabuf::{DmabufFeedbackBuilder, DmabufGlobal};
use smithay::wayland::presentation::Refresh;

use super::{IpcOutputMap, OutputId, RenderResult};
use crate::niri::{Niri, RedrawState, State};
use crate::render_helpers::{resources, shaders, RenderCtx, RenderTarget};
use crate::utils::{get_monotonic_time, logical_output};

/// Refresh rate of virtual outputs, in mHz.
const VIRTUAL_OUTPUT_REFRESH: i32 = 60_000;

static VIRTUAL_OUTPUT_COUNTER: AtomicU32 = AtomicU32::new(0);

/// Returns a new unique name for a virtual output.
///
/// This can be called from any thread, so that the name (which also serves as the remote desktop
/// mapping ID) is known before the output itself gets created.
pub fn next_virtual_output_name() -> String {
    let n = VIRTUAL_OUTPUT_COUNTER.fetch_add(1, Ordering::Relaxed) + 1;
    format!("HEADLESS-{n}")
}

pub struct Headless {
    renderer: Option<GlesRenderer>,
    ipc_outputs: Arc<Mutex<IpcOutputMap>>,
    /// Whether this is a real headless session, as opposed to a test.
    is_session: bool,
    render_node: Option<DrmNode>,
    dmabuf_global: Option<DmabufGlobal>,
    #[cfg(feature = "xdp-gnome-screencast")]
    gbm_device: Option<smithay::backend::allocator::gbm::GbmDevice<smithay::utils::DeviceFd>>,
    /// Virtual outputs by name.
    virtual_outputs: HashMap<String, (Output, OutputId)>,
}

/// Per-output damage tracker, used to compute which elements are visible without rendering.
type HeadlessDamageTracker = RefCell<OutputDamageTracker>;

impl Headless {
    pub fn new() -> Self {
        Self {
            renderer: None,
            ipc_outputs: Default::default(),
            is_session: false,
            render_node: None,
            dmabuf_global: None,
            #[cfg(feature = "xdp-gnome-screencast")]
            gbm_device: None,
            virtual_outputs: HashMap::new(),
        }
    }

    pub fn init(&mut self, _niri: &mut Niri) {}

    /// Sets up the backend for a real headless session (as opposed to tests).
    ///
    /// The session starts out with no outputs; virtual outputs are added on demand.
    pub fn start_session(&mut self, niri: &mut Niri) -> anyhow::Result<()> {
        self.is_session = true;

        self.add_renderer()?;
        let renderer = self.renderer.as_mut().unwrap();

        if let Err(err) = renderer.bind_wl_display(&niri.display_handle) {
            // wl_drm is on its way out so this is expected on most modern distros.
            trace!("error binding legacy EGL to wl_display: {err}");
        }

        let config = niri.config.borrow();
        if let Some(src) = config.animations.window_resize.custom_shader.as_deref() {
            shaders::set_custom_resize_program(renderer, Some(src));
        }
        if let Some(src) = config.animations.window_close.custom_shader.as_deref() {
            shaders::set_custom_close_program(renderer, Some(src));
        }
        if let Some(src) = config.animations.window_open.custom_shader.as_deref() {
            shaders::set_custom_open_program(renderer, Some(src));
        }
        drop(config);

        niri.update_shaders();

        self.render_node = match EGLDevice::device_for_display(renderer.egl_context().display())
            .context("error getting EGL device")
            .and_then(|device| {
                device
                    .try_get_render_node()
                    .context("error getting EGL device render node")?
                    .context("EGL device has no render node")
            }) {
            Ok(node) => {
                debug!("using as the render node: {node}");
                Some(node)
            }
            Err(err) => {
                warn!("couldn't find the render node, dmabufs will be unavailable: {err:?}");
                None
            }
        };

        self.create_dmabuf_global(niri);

        #[cfg(feature = "xdp-gnome-screencast")]
        if let Err(err) = self.create_gbm_device() {
            debug!("couldn't create GBM device for screencasting: {err:?}");
        }

        Ok(())
    }

    fn create_dmabuf_global(&mut self, niri: &mut Niri) {
        let renderer = self.renderer.as_mut().unwrap();
        let formats = renderer.dmabuf_formats();

        let default_feedback = self.render_node.map(|node| {
            DmabufFeedbackBuilder::new(node.dev_id(), formats.clone())
                .build()
                .context("error building dmabuf feedback")
        });

        let global = match default_feedback {
            Some(Ok(feedback)) => niri
                .dmabuf_state
                .create_global_with_default_feedback::<State>(&niri.display_handle, &feedback),
            default_feedback => {
                if let Some(Err(err)) = default_feedback {
                    debug!("failed building default dmabuf feedback, falling back to v3: {err:?}");
                }
                niri.dmabuf_state
                    .create_global::<State>(&niri.display_handle, formats)
            }
        };
        self.dmabuf_global = Some(global);
    }

    #[cfg(feature = "xdp-gnome-screencast")]
    fn create_gbm_device(&mut self) -> anyhow::Result<()> {
        use std::os::fd::OwnedFd;

        use smithay::backend::allocator::gbm::GbmDevice;
        use smithay::utils::DeviceFd;

        let node = self.render_node.context("no render node available")?;
        let path = node.dev_path().context("render node has no device path")?;
        let file = std::fs::File::options()
            .read(true)
            .write(true)
            .open(path)
            .context("error opening render node")?;

        let gbm_device = GbmDevice::new(DeviceFd::from(OwnedFd::from(file)))
            .context("error creating GBM device")?;

        self.gbm_device = Some(gbm_device);
        Ok(())
    }

    #[cfg(feature = "xdp-gnome-screencast")]
    pub fn gbm_device(
        &self,
    ) -> Option<smithay::backend::allocator::gbm::GbmDevice<smithay::utils::DeviceFd>> {
        self.gbm_device.clone()
    }

    pub fn add_renderer(&mut self) -> anyhow::Result<()> {
        if self.renderer.is_some() {
            error!("add_renderer: renderer must not already exist");
            return Ok(());
        }

        let mut renderer = unsafe {
            let display =
                EGLDisplay::new(EGLSurfacelessDisplay).context("error creating EGL display")?;
            let context = EGLContext::new(&display).context("error creating EGL context")?;
            GlesRenderer::new(context).context("error creating renderer")?
        };

        resources::init(&mut renderer);
        shaders::init(&mut renderer);

        self.renderer = Some(renderer);
        Ok(())
    }

    /// Adds an output for tests.
    pub fn add_output(&mut self, niri: &mut Niri, n: u8, size: (u16, u16)) {
        let connector = format!("headless-{n}");
        let make = "niri".to_string();
        let model = "headless".to_string();
        let serial = n.to_string();

        let output = Output::new(
            connector.clone(),
            PhysicalProperties {
                size: (0, 0).into(),
                subpixel: Subpixel::Unknown,
                make: make.clone(),
                model: model.clone(),
                serial_number: serial.clone(),
            },
        );

        let mode = Mode {
            size: Size::from((i32::from(size.0), i32::from(size.1))),
            refresh: 60_000,
        };
        output.change_current_state(Some(mode), None, None, None);
        output.set_preferred(mode);

        output.user_data().insert_if_missing(|| OutputName {
            connector,
            make: Some(make),
            model: Some(model),
            serial: Some(serial),
        });

        let physical_properties = output.physical_properties();
        self.ipc_outputs.lock().unwrap().insert(
            OutputId::next(),
            niri_ipc::Output {
                name: output.name(),
                make: physical_properties.make,
                model: physical_properties.model,
                serial: None,
                physical_size: None,
                modes: vec![niri_ipc::Mode {
                    width: size.0,
                    height: size.1,
                    refresh_rate: 60_000,
                    is_preferred: true,
                }],
                current_mode: Some(0),
                is_custom_mode: true,
                vrr_supported: false,
                vrr_enabled: false,
                logical: Some(logical_output(&output)),
                max_bpc: None,
            },
        );

        niri.add_output(output, None, false);
    }

    /// Whether virtual outputs can be created.
    pub fn supports_virtual_outputs(&self) -> bool {
        self.is_session
    }

    /// Creates a virtual output with the given name and size, and adds it to the layout.
    pub fn add_virtual_output(
        &mut self,
        niri: &mut Niri,
        name: String,
        size: Size<i32, Physical>,
    ) -> anyhow::Result<Output> {
        anyhow::ensure!(self.is_session, "virtual outputs need a headless session");
        anyhow::ensure!(
            !self.virtual_outputs.contains_key(&name),
            "output {name} already exists"
        );

        let make = String::from("niri");
        let model = String::from("Virtual");

        let output = Output::new(
            name.clone(),
            PhysicalProperties {
                size: (0, 0).into(),
                subpixel: Subpixel::Unknown,
                make: make.clone(),
                model: model.clone(),
                serial_number: String::from("Unknown"),
            },
        );

        let mode = Mode {
            size,
            refresh: VIRTUAL_OUTPUT_REFRESH,
        };
        output.change_current_state(Some(mode), None, None, None);
        output.set_preferred(mode);

        output.user_data().insert_if_missing(|| OutputName {
            connector: name.clone(),
            make: Some(make.clone()),
            model: Some(model.clone()),
            serial: None,
        });

        let id = OutputId::next();
        self.ipc_outputs.lock().unwrap().insert(
            id,
            niri_ipc::Output {
                name: name.clone(),
                make,
                model,
                serial: None,
                physical_size: None,
                modes: vec![ipc_mode(mode)],
                current_mode: Some(0),
                is_custom_mode: false,
                vrr_supported: false,
                vrr_enabled: false,
                logical: None,
                max_bpc: None,
            },
        );

        debug!("adding virtual output {name} with size {size:?}");
        niri.add_output(output.clone(), Some(refresh_interval(mode)), false);
        niri.ipc_outputs_changed = true;

        self.virtual_outputs.insert(name, (output.clone(), id));
        Ok(output)
    }

    /// Changes the size of a virtual output.
    pub fn resize_virtual_output(
        &mut self,
        niri: &mut Niri,
        output: &Output,
        size: Size<i32, Physical>,
    ) {
        let Some((_, id)) = self.virtual_outputs.get(&output.name()) else {
            warn!("trying to resize unknown virtual output {}", output.name());
            return;
        };

        let Some(current) = output.current_mode() else {
            return;
        };
        if current.size == size {
            return;
        }

        debug!("resizing virtual output {} to {size:?}", output.name());

        let mode = Mode {
            size,
            refresh: current.refresh,
        };
        output.change_current_state(Some(mode), None, None, None);
        output.set_preferred(mode);
        // Keep only the current mode, so that clients don't see a growing list of old sizes.
        for old in output.modes() {
            if old != mode {
                output.delete_mode(old);
            }
        }

        if let Some(ipc_output) = self.ipc_outputs.lock().unwrap().get_mut(id) {
            ipc_output.modes = vec![ipc_mode(mode)];
            ipc_output.current_mode = Some(0);
        }

        niri.output_resized(output);
        niri.ipc_outputs_changed = true;
    }

    /// Removes a virtual output.
    pub fn remove_virtual_output(&mut self, niri: &mut Niri, output: &Output) {
        let Some((output, id)) = self.virtual_outputs.remove(&output.name()) else {
            return;
        };

        debug!("removing virtual output {}", output.name());

        self.ipc_outputs.lock().unwrap().remove(&id);
        niri.remove_output(&output);
        niri.ipc_outputs_changed = true;
    }

    pub fn seat_name(&self) -> String {
        "headless".to_owned()
    }

    pub fn with_primary_renderer<T>(
        &mut self,
        f: impl FnOnce(&mut GlesRenderer) -> T,
    ) -> Option<T> {
        self.renderer.as_mut().map(f)
    }

    pub fn primary_render_node(&mut self) -> Option<DrmNode> {
        self.render_node
    }

    pub fn render(
        &mut self,
        niri: &mut Niri,
        output: &Output,
        target_presentation_time: Duration,
    ) -> RenderResult {
        if !self.is_session {
            return render_for_tests(niri, output);
        }

        let _span = tracy_client::span!("Headless::render");

        let Some(renderer) = self.renderer.as_mut() else {
            return RenderResult::Skipped;
        };

        // Nobody looks at a headless output directly (screencasts render on their own), so only
        // compute what is visible and damaged, without drawing anything. This keeps frame
        // callbacks, presentation feedback and primary scanout outputs working like on a TTY.
        let ctx = RenderCtx {
            renderer,
            target: RenderTarget::Output,
            xray: None,
        };
        let elements = niri.render_to_vec(ctx, output, true);

        let damage_tracker = output
            .user_data()
            .get_or_insert(|| HeadlessDamageTracker::new(OutputDamageTracker::from_output(output)));
        let res = damage_tracker
            .borrow_mut()
            .damage_output(1, &elements)
            .map(|(damage, states)| (damage.is_some_and(|d| !d.is_empty()), states));
        let (has_damage, states) = match res {
            Ok(x) => x,
            Err(err) => {
                warn!("error computing damage for {}: {err:?}", output.name());
                return RenderResult::Skipped;
            }
        };

        niri.update_primary_scanout_output(output, &states);

        if !has_damage {
            super::queue_estimated_vblank_timer(niri, output.clone(), target_presentation_time);
            return RenderResult::NoDamage;
        }

        let feedbacks = niri.take_presentation_feedbacks(output, &states);

        let output_state = niri.output_state.get_mut(output).unwrap();
        let new_state = RedrawState::WaitingForVBlank {
            redraw_needed: false,
        };
        let token = match mem::replace(&mut output_state.redraw_state, new_state) {
            RedrawState::Queued => None,
            RedrawState::WaitingForEstimatedVBlankAndQueued(token) => Some(token),
            RedrawState::Idle
            | RedrawState::WaitingForVBlank { .. }
            | RedrawState::WaitingForEstimatedVBlank(_) => unreachable!(),
        };

        // The frame is "presented", so clients can start drawing the next one.
        output_state.frame_callback_sequence = output_state.frame_callback_sequence.wrapping_add(1);

        if let Some(token) = token {
            niri.event_loop.remove(token);
        }

        queue_vblank_timer(niri, output.clone(), feedbacks, target_presentation_time);

        RenderResult::Submitted
    }

    pub fn import_dmabuf(&mut self, dmabuf: &Dmabuf) -> bool {
        let Some(renderer) = self.renderer.as_mut() else {
            return false;
        };

        match renderer.import_dmabuf(dmabuf, None) {
            Ok(_texture) => {
                if dmabuf.node().is_none() {
                    dmabuf.set_node(self.render_node);
                }
                true
            }
            Err(err) => {
                debug!("error importing dmabuf: {err:?}");
                false
            }
        }
    }

    pub fn ipc_outputs(&self) -> Arc<Mutex<IpcOutputMap>> {
        self.ipc_outputs.clone()
    }
}

impl Default for Headless {
    fn default() -> Self {
        Self::new()
    }
}

fn ipc_mode(mode: Mode) -> niri_ipc::Mode {
    niri_ipc::Mode {
        width: mode.size.w.clamp(0, i32::from(u16::MAX)) as u16,
        height: mode.size.h.clamp(0, i32::from(u16::MAX)) as u16,
        refresh_rate: mode.refresh as u32,
        is_preferred: true,
    }
}

fn refresh_interval(mode: Mode) -> Duration {
    Duration::from_nanos(1_000_000_000_000 / mode.refresh.max(1) as u64)
}

/// Rendering for tests: nothing is rendered and frames are "presented" right away.
fn render_for_tests(niri: &mut Niri, output: &Output) -> RenderResult {
    let states = RenderElementStates::default();
    let mut presentation_feedbacks = niri.take_presentation_feedbacks(output, &states);
    presentation_feedbacks.presented::<_, smithay::utils::Monotonic>(
        get_monotonic_time(),
        Refresh::Unknown,
        0,
        wp_presentation_feedback::Kind::empty(),
    );

    let output_state = niri.output_state.get_mut(output).unwrap();
    match mem::replace(&mut output_state.redraw_state, RedrawState::Idle) {
        RedrawState::Idle => unreachable!(),
        RedrawState::Queued => (),
        RedrawState::WaitingForVBlank { .. } => unreachable!(),
        RedrawState::WaitingForEstimatedVBlank(_) => unreachable!(),
        RedrawState::WaitingForEstimatedVBlankAndQueued(_) => unreachable!(),
    }

    output_state.frame_callback_sequence = output_state.frame_callback_sequence.wrapping_add(1);

    // FIXME: request redraw on unfinished animations remain

    RenderResult::Submitted
}

/// Queues a timer standing in for the VBlank of a headless output.
fn queue_vblank_timer(
    niri: &mut Niri,
    output: Output,
    feedbacks: OutputPresentationFeedback,
    target_presentation_time: Duration,
) {
    let output_state = niri.output_state.get(&output).unwrap();
    let refresh_interval = output_state
        .frame_clock
        .refresh_interval()
        .unwrap_or(Duration::from_micros(16_667));

    let now = get_monotonic_time();
    let mut duration = target_presentation_time.saturating_sub(now);
    if duration.is_zero() {
        duration = refresh_interval;
    }

    let mut feedbacks = Some(feedbacks);
    let timer = Timer::from_duration(duration);
    niri.event_loop
        .insert_source(timer, move |_, _, data| {
            if let Some(feedbacks) = feedbacks.take() {
                on_vblank(&mut data.niri, &output, feedbacks);
            }
            TimeoutAction::Drop
        })
        .unwrap();
}

fn on_vblank(niri: &mut Niri, output: &Output, mut feedbacks: OutputPresentationFeedback) {
    let _span = tracy_client::span!("Headless::on_vblank");

    // The output may have been removed in the meantime.
    let Some(output_state) = niri.output_state.get_mut(output) else {
        return;
    };

    let redraw_needed = match mem::replace(&mut output_state.redraw_state, RedrawState::Idle) {
        RedrawState::WaitingForVBlank { redraw_needed } => redraw_needed,
        state => {
            error!(
                "unexpected redraw state for output {} (should be WaitingForVBlank): {state:?}",
                output.name()
            );
            true
        }
    };

    let now = get_monotonic_time();
    let refresh = output_state
        .frame_clock
        .refresh_interval()
        .map(Refresh::Fixed)
        .unwrap_or(Refresh::Unknown);
    feedbacks.presented::<_, smithay::utils::Monotonic>(
        now,
        refresh,
        0,
        wp_presentation_feedback::Kind::Vsync,
    );

    output_state.frame_clock.presented(now);

    if redraw_needed || output_state.unfinished_animations_remain {
        niri.queue_redraw(output);
    } else {
        niri.send_frame_callbacks(output);
    }
}
