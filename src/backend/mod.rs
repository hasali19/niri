use std::collections::HashMap;
use std::mem;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use niri_config::{Config, ModKey};
use smithay::backend::allocator::dmabuf::Dmabuf;
use smithay::backend::drm::DrmNode;
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::output::Output;
use smithay::reexports::calloop::timer::{TimeoutAction, Timer};
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;

use crate::niri::{Niri, RedrawState};
use crate::utils::get_monotonic_time;
use crate::utils::id::IdCounter;

pub mod tty;
pub use tty::Tty;

pub mod winit;
pub use winit::Winit;

pub mod headless;
pub use headless::Headless;

/// The backend for input events and graphical output.
#[allow(clippy::large_enum_variant)]
pub enum Backend {
    Tty(Tty),
    Winit(Winit),
    Headless(Headless),
}

#[derive(PartialEq, Eq)]
pub enum RenderResult {
    /// The frame was submitted to the backend for presentation.
    Submitted,
    /// Rendering succeeded, but there was no damage.
    NoDamage,
    /// The frame was not rendered and submitted, due to an error or otherwise.
    Skipped,
}

pub type IpcOutputMap = HashMap<OutputId, niri_ipc::Output>;

static OUTPUT_ID_COUNTER: IdCounter = IdCounter::new();

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct OutputId(u64);

impl OutputId {
    fn next() -> OutputId {
        OutputId(OUTPUT_ID_COUNTER.next())
    }

    pub fn get(self) -> u64 {
        self.0
    }
}

impl Backend {
    pub fn init(&mut self, niri: &mut Niri) {
        let _span = tracy_client::span!("Backend::init");
        match self {
            Backend::Tty(tty) => tty.init(niri),
            Backend::Winit(winit) => winit.init(niri),
            Backend::Headless(headless) => headless.init(niri),
        }
    }

    pub fn seat_name(&self) -> String {
        match self {
            Backend::Tty(tty) => tty.seat_name(),
            Backend::Winit(winit) => winit.seat_name(),
            Backend::Headless(headless) => headless.seat_name(),
        }
    }

    pub fn with_primary_renderer<T>(
        &mut self,
        f: impl FnOnce(&mut GlesRenderer) -> T,
    ) -> Option<T> {
        match self {
            Backend::Tty(tty) => tty.with_primary_renderer(f),
            Backend::Winit(winit) => winit.with_primary_renderer(f),
            Backend::Headless(headless) => headless.with_primary_renderer(f),
        }
    }

    /// DRM render node of the primary renderer, if it has one.
    ///
    /// This is the node clients should allocate dma-bufs on for the primary renderer to import
    /// and render into them directly.
    pub fn primary_render_node(&mut self) -> Option<DrmNode> {
        match self {
            Backend::Tty(tty) => tty.primary_render_node(),
            Backend::Winit(winit) => winit.primary_render_node(),
            Backend::Headless(headless) => headless.primary_render_node(),
        }
    }

    pub fn render(
        &mut self,
        niri: &mut Niri,
        output: &Output,
        target_presentation_time: Duration,
    ) -> RenderResult {
        match self {
            Backend::Tty(tty) => tty.render(niri, output, target_presentation_time),
            Backend::Winit(winit) => winit.render(niri, output),
            Backend::Headless(headless) => headless.render(niri, output, target_presentation_time),
        }
    }

    pub fn mod_key(&self, config: &Config) -> ModKey {
        match self {
            Backend::Winit(_) => config.input.mod_key_nested.unwrap_or({
                if let Some(ModKey::Alt) = config.input.mod_key {
                    ModKey::Super
                } else {
                    ModKey::Alt
                }
            }),
            Backend::Tty(_) | Backend::Headless(_) => config.input.mod_key.unwrap_or(ModKey::Super),
        }
    }

    pub fn change_vt(&mut self, vt: i32) {
        match self {
            Backend::Tty(tty) => tty.change_vt(vt),
            Backend::Winit(_) => (),
            Backend::Headless(_) => (),
        }
    }

    pub fn suspend(&mut self) {
        match self {
            Backend::Tty(tty) => tty.suspend(),
            Backend::Winit(_) => (),
            Backend::Headless(_) => (),
        }
    }

    pub fn toggle_debug_tint(&mut self) {
        match self {
            Backend::Tty(tty) => tty.toggle_debug_tint(),
            Backend::Winit(winit) => winit.toggle_debug_tint(),
            Backend::Headless(_) => (),
        }
    }

    pub fn import_dmabuf(&mut self, dmabuf: &Dmabuf) -> bool {
        match self {
            Backend::Tty(tty) => tty.import_dmabuf(dmabuf),
            Backend::Winit(winit) => winit.import_dmabuf(dmabuf),
            Backend::Headless(headless) => headless.import_dmabuf(dmabuf),
        }
    }

    pub fn early_import(&mut self, surface: &WlSurface) {
        match self {
            Backend::Tty(tty) => tty.early_import(surface),
            Backend::Winit(_) => (),
            Backend::Headless(_) => (),
        }
    }

    pub fn ipc_outputs(&self) -> Arc<Mutex<IpcOutputMap>> {
        match self {
            Backend::Tty(tty) => tty.ipc_outputs(),
            Backend::Winit(winit) => winit.ipc_outputs(),
            Backend::Headless(headless) => headless.ipc_outputs(),
        }
    }

    #[cfg(feature = "xdp-gnome-screencast")]
    pub fn gbm_device(
        &self,
    ) -> Option<smithay::backend::allocator::gbm::GbmDevice<smithay::utils::DeviceFd>> {
        match self {
            Backend::Tty(tty) => tty.primary_gbm_device(),
            Backend::Winit(winit) => winit.gbm_device(),
            Backend::Headless(headless) => headless.gbm_device(),
        }
    }

    pub fn set_monitors_active(&mut self, active: bool) {
        match self {
            Backend::Tty(tty) => tty.set_monitors_active(active),
            Backend::Winit(_) => (),
            Backend::Headless(_) => (),
        }
    }

    pub fn set_output_on_demand_vrr(&mut self, niri: &mut Niri, output: &Output, enable_vrr: bool) {
        match self {
            Backend::Tty(tty) => tty.set_output_on_demand_vrr(niri, output, enable_vrr),
            Backend::Winit(_) => (),
            Backend::Headless(_) => (),
        }
    }

    pub fn update_ignored_nodes_config(&mut self, niri: &mut Niri) {
        match self {
            Backend::Tty(tty) => tty.update_ignored_nodes_config(niri),
            Backend::Winit(_) => (),
            Backend::Headless(_) => (),
        }
    }

    pub fn on_output_config_changed(&mut self, niri: &mut Niri) {
        match self {
            Backend::Tty(tty) => tty.on_output_config_changed(niri),
            Backend::Winit(_) => (),
            Backend::Headless(_) => (),
        }
    }

    pub fn set_host_cursor_visible(&mut self, visible: bool) {
        match self {
            Backend::Winit(winit) => winit.set_cursor_visible(visible),
            Backend::Tty(_) | Backend::Headless(_) => (),
        }
    }

    pub fn tty_checked(&mut self) -> Option<&mut Tty> {
        if let Self::Tty(v) = self {
            Some(v)
        } else {
            None
        }
    }

    pub fn tty(&mut self) -> &mut Tty {
        if let Self::Tty(v) = self {
            v
        } else {
            panic!("backend is not Tty");
        }
    }

    pub fn winit(&mut self) -> &mut Winit {
        if let Self::Winit(v) = self {
            v
        } else {
            panic!("backend is not Winit")
        }
    }

    pub fn headless(&mut self) -> &mut Headless {
        if let Self::Headless(v) = self {
            v
        } else {
            panic!("backend is not Headless")
        }
    }
}

/// Queues a timer to fire at the estimated next VBlank of an output.
///
/// Backends use this when they didn't submit a frame (for example because there was no damage),
/// so that frame callbacks keep getting sent at the output's refresh rate.
fn queue_estimated_vblank_timer(
    niri: &mut Niri,
    output: Output,
    target_presentation_time: Duration,
) {
    let output_state = niri.output_state.get_mut(&output).unwrap();
    match mem::take(&mut output_state.redraw_state) {
        RedrawState::Idle => unreachable!(),
        RedrawState::Queued => (),
        RedrawState::WaitingForVBlank { .. } => unreachable!(),
        RedrawState::WaitingForEstimatedVBlank(token)
        | RedrawState::WaitingForEstimatedVBlankAndQueued(token) => {
            output_state.redraw_state = RedrawState::WaitingForEstimatedVBlank(token);
            return;
        }
    }

    let now = get_monotonic_time();
    let mut duration = target_presentation_time.saturating_sub(now);

    // No use setting a zero timer, since we'll send frame callbacks anyway right after the call to
    // render(). This can happen for example with unknown presentation time from DRM.
    if duration.is_zero() {
        duration += output_state
            .frame_clock
            .refresh_interval()
            // Unknown refresh interval, i.e. winit backend. Would be good to estimate it somehow
            // but it's not that important for this code path.
            .unwrap_or(Duration::from_micros(16_667));
    }

    trace!("queueing estimated vblank timer to fire in {duration:?}");

    let timer = Timer::from_duration(duration);
    let token = niri
        .event_loop
        .insert_source(timer, move |_, _, data| {
            on_estimated_vblank_timer(&mut data.niri, output.clone());
            TimeoutAction::Drop
        })
        .unwrap();
    output_state.redraw_state = RedrawState::WaitingForEstimatedVBlank(token);
}

fn on_estimated_vblank_timer(niri: &mut Niri, output: Output) {
    let span = tracy_client::span!("on_estimated_vblank_timer");

    let name = output.name();
    span.emit_text(&name);

    let Some(output_state) = niri.output_state.get_mut(&output) else {
        error!("missing output state for {name}");
        return;
    };

    // We waited for the timer, now we can send frame callbacks again.
    output_state.frame_callback_sequence = output_state.frame_callback_sequence.wrapping_add(1);

    match mem::replace(&mut output_state.redraw_state, RedrawState::Idle) {
        RedrawState::Idle => unreachable!(),
        RedrawState::Queued => unreachable!(),
        RedrawState::WaitingForVBlank { .. } => unreachable!(),
        RedrawState::WaitingForEstimatedVBlank(_) => (),
        // The timer fired just in front of a redraw.
        RedrawState::WaitingForEstimatedVBlankAndQueued(_) => {
            output_state.redraw_state = RedrawState::Queued;
            return;
        }
    }

    if output_state.unfinished_animations_remain {
        niri.queue_redraw(&output);
    } else {
        niri.send_frame_callbacks(&output);
    }
}
