mod atlas;
#[cfg(debug_assertions)]
pub mod preview;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use anyhow::{anyhow, Result};
use bytemuck::{Pod, Zeroable};
use winit::window::{ResizeDirection, Window};

use crate::color::{Color, Palette, Rgb, ThemeId};
use crate::grid::CursorShape;
use crate::term::Terminal;
use atlas::{FontId, GlyphAtlas, GlyphKey};

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct Uniforms {
    screen: [f32; 2],
    _pad: [f32; 2],
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct Instance {
    pos: [f32; 2],
    size: [f32; 2],
    uv_min: [f32; 2],
    uv_max: [f32; 2],
    color: [f32; 4],
    kind: u32,
    _pad: [u32; 3],
}

const INSTANCE_ATTRS: [wgpu::VertexAttribute; 6] = wgpu::vertex_attr_array![
    0 => Float32x2,
    1 => Float32x2,
    2 => Float32x2,
    3 => Float32x2,
    4 => Float32x4,
    5 => Uint32,
];

/// build a full mip chain from a 32bpp RGBA base via 2x2 box downsample;
/// returns (dim, data) per level from `dim` down to 1. used so the icon badge
/// stays crisp when scaled far down in the title bar
fn build_mips(base: &[u8], dim: u32) -> Vec<(u32, Vec<u8>)> {
    let mut levels: Vec<(u32, Vec<u8>)> = vec![(dim, base.to_vec())];
    let mut d = dim;
    // 3 levels (128→64→32) is enough mip coverage for the ~20px title-bar badge;
    // smaller levels never get sampled, so building them is wasted startup work
    while d > 32 {
        let nd = d / 2;
        let pd = d;
        let prev = &levels.last().unwrap().1;
        let mut out = vec![0u8; (nd * nd * 4) as usize];
        for y in 0..nd {
            for x in 0..nd {
                for ch in 0..4 {
                    let mut sum = 0u32;
                    for dy in 0..2 {
                        for dx in 0..2 {
                            let sx = x * 2 + dx;
                            let sy = y * 2 + dy;
                            sum += prev[((sy * pd + sx) * 4 + ch) as usize] as u32;
                        }
                    }
                    out[((y * nd + x) * 4 + ch) as usize] = (sum / 4) as u8;
                }
            }
        }
        levels.push((nd, out));
        d = nd;
    }
    levels
}

/// every hoverable/clickable chrome target
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Hot {
    Minimize,
    Maximize,
    Close,
    Gear,
    SplitV,
    SplitH,
    PaneMode,
    NewTab,
    Tab(usize),
    TabClose(usize),
    PanelClose,
    // settings controls
    FontDec,
    FontInc,
    FontCycle,
    PadDec,
    PadInc,
    OpacityDec,
    OpacityInc,
    CursorCycle,
    CursorBlink,
    ThemeSet(ThemeId),
    ScrollbackDec,
    ScrollbackInc,
    CopyOnSelect,
    ShellCycle,
    LoadProfile,
    CloseActionCycle,
    BackendCycle,
    OpenPlugins,
    /// toggle the enabled state of installed plugin at this index
    PluginToggle(usize),
}

/// a terminal to draw at a pixel rect within the window
pub struct PaneView<'a> {
    pub term: &'a Terminal,
    pub rect: (f32, f32, f32, f32),
    pub focused: bool,
    /// active selection range (row, col) within this pane's viewport
    pub sel: Option<((usize, usize), (usize, usize))>,
    /// accent-border opacity after the shell rang the bell: 1 then eased to 0
    /// (0 = no flash) so the bell border fades out instead of snapping off
    pub flash: f32,
    /// hovered url to underline: (viewport row, col_start, col_end exclusive)
    pub link: Option<(usize, usize, usize)>,
}

/// command-palette display state
pub struct PaletteView {
    pub query: String,
    pub items: Vec<String>,
    pub selected: usize,
}

/// right-click pane context menu: a small overlay at (x, y). the item index
/// maps to a fixed action in main.rs's handler (kept in sync with PANE_MENU_ITEMS)
pub struct PaneMenuView {
    pub x: f32,
    pub y: f32,
    pub hovered: Option<usize>,
}

pub const PANE_MENU_ITEMS: [&str; 5] =
    ["split vertical", "split horizontal", "pop out to window", "close pane", "paste"];

/// find-in-scrollback overlay display state. `matches` are on-screen rects
/// (viewport row, col, len, is_current) for the focused pane
pub struct FindView {
    pub query: String,
    pub count: usize,
    pub current: usize,
    pub matches: Vec<(usize, usize, usize, bool)>,
}

/// a modal confirm overlay: a centered box with a prompt + a key hint, shown
/// until the user presses enter (confirm) or esc (cancel)
pub struct ConfirmView {
    pub prompt: String,
    pub hint: String,
}

/// the tab-rename text field overlay
pub struct RenameView {
    pub buf: String,
}

/// one row in the plugins marketplace overlay
pub struct MarketRowView {
    /// left text: plugin name + version
    pub label: String,
    /// right tag: "on" / "off" / "install" / "update"
    pub tag: String,
    /// dim secondary line (permissions etc.), empty for none
    pub sub: String,
}

/// plugins marketplace overlay display state
pub struct MarketView {
    pub rows: Vec<MarketRowView>,
    pub selected: usize,
    pub status: String,
}

/// a plugin-declared Tier-1 widget to draw in the side dock. render-side mirror
/// of the plugin protocol's Widget, so the renderer doesn't depend on the
/// plugin module
#[derive(Clone, Default)]
pub struct DockWidget {
    pub title: String,
    pub lines: Vec<String>,
}

pub enum Hit {
    Button(Hot),
    TitleBar,
    Resize(ResizeDirection),
    Content,
}

/// read-only mirror of the App-owned settings so the renderer can label them;
/// renderer-owned values (font/padding/cursor/blink/theme) live on `Renderer`
#[derive(Clone, Copy)]
pub struct SettingsView {
    pub scrollback: usize,
    pub copy_on_select: bool,
    pub load_profile: bool,
    pub shell_name: &'static str,
    pub close_action_name: &'static str,
    pub backend_name: &'static str,
}

impl Default for SettingsView {
    fn default() -> Self {
        SettingsView {
            scrollback: 10_000,
            copy_on_select: false,
            load_profile: false,
            shell_name: "auto",
            close_action_name: "quit",
            backend_name: "auto",
        }
    }
}

type Rect = (f32, f32, f32, f32);
/// a tab in the title bar: (session index, tab rect, close-icon rect)
type TabEntry = (usize, Rect, Rect);
/// resolved per-pane paint origin: (origin x, origin y, focused, pane rect)
type PaneInfo = (f32, f32, bool, Rect);
/// snapshot of a tab row for painting: index, tab rect, close rect, label,
/// active, hovered, close-hovered
type TabItem = (usize, Rect, Rect, String, bool, bool, bool);

/// GPU backend choice for compatibility; persisted + applied at startup
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BackendChoice {
    Auto,
    Dx12,
    Vulkan,
    Gl,
}

impl BackendChoice {
    pub fn next(self) -> Self {
        match self {
            BackendChoice::Auto => BackendChoice::Dx12,
            BackendChoice::Dx12 => BackendChoice::Vulkan,
            BackendChoice::Vulkan => BackendChoice::Gl,
            BackendChoice::Gl => BackendChoice::Auto,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            BackendChoice::Auto => "auto",
            BackendChoice::Dx12 => "dx12",
            BackendChoice::Vulkan => "vulkan",
            BackendChoice::Gl => "gl",
        }
    }

    pub fn from_label(s: &str) -> Self {
        match s {
            "dx12" => BackendChoice::Dx12,
            "vulkan" => BackendChoice::Vulkan,
            "gl" => BackendChoice::Gl,
            _ => BackendChoice::Auto,
        }
    }

    fn to_backends(self) -> wgpu::Backends {
        match self {
            // Auto = DX12 on Windows (Vulkan is slow under overlay layers)
            BackendChoice::Auto => {
                if cfg!(windows) {
                    wgpu::Backends::DX12
                } else {
                    wgpu::Backends::all()
                }
            }
            BackendChoice::Dx12 => wgpu::Backends::DX12,
            BackendChoice::Vulkan => wgpu::Backends::VULKAN,
            BackendChoice::Gl => wgpu::Backends::GL,
        }
    }
}

/// full geometry for the settings page; shared by `build_settings` (drawing)
/// and `hit_test` (the `controls` list), so the two never drift
struct SettingsGeom {
    // slide-in panel frame
    panel_x: f32,
    panel_w: f32,
    panel_top: f32,
    panel_h: f32,
    // scrollable body viewport + total content height (for scroll clamp)
    body_top: f32,
    body_bottom: f32,
    content_h: f32,
    // body content metrics
    content_x: f32,
    content_w: f32,
    bh: f32,
    val_w: f32,
    head_y: f32,
    close_btn: Rect,
    fontfam_y: f32,
    fontfam_btn: Rect,
    // section header baselines (absolute, scroll-adjusted)
    sec_app_y: f32,
    sec_beh_y: f32,
    sec_shell_y: f32,
    sec_plugins_y: f32,
    sec_keys_y: f32,
    sec_about_y: f32,
    // row label baselines (absolute, scroll-adjusted)
    font_y: f32,
    pad_y: f32,
    opacity_y: f32,
    cursor_y: f32,
    blink_y: f32,
    theme_label_y: f32,
    scrollback_y: f32,
    copysel_y: f32,
    shell_y: f32,
    profile_y: f32,
    close_y: f32,
    backend_y: f32,
    /// (name, enabled, toggle rect, row baseline) per installed plugin
    plugin_rows: Vec<(String, bool, Rect, f32)>,
    keys_start_y: f32,
    about_start_y: f32,
    // interactive rects (absolute, scroll-adjusted)
    font_dec: Rect,
    font_inc: Rect,
    pad_dec: Rect,
    pad_inc: Rect,
    op_dec: Rect,
    op_inc: Rect,
    cursor_btn: Rect,
    blink_btn: Rect,
    theme_chips: [Rect; 3],
    sb_dec: Rect,
    sb_inc: Rect,
    copysel_btn: Rect,
    shell_btn: Rect,
    profile_btn: Rect,
    close_action_btn: Rect,
    backend_btn: Rect,
    plugins_btn: Rect,
    /// body controls only (absolute); `close_btn` is handled separately
    controls: Vec<(Hot, Rect)>,
}

struct TabLayout {
    /// (session index, tab rect, close-icon rect)
    tabs: Vec<TabEntry>,
    newtab: (f32, f32, f32, f32),
}

fn in_rect(x: f32, y: f32, r: (f32, f32, f32, f32)) -> bool {
    x >= r.0 && x < r.0 + r.2 && y >= r.1 && y < r.1 + r.3
}

/// emit the rects (cell-local x,y,w,h) that draw an underline of the given
/// style; shared by the GPU renderer and the dev PNG preview so they match
fn underline_rects(
    style: crate::grid::UnderlineStyle,
    cell_w: f32,
    cell_h: f32,
    t: f32,
    mut emit: impl FnMut(f32, f32, f32, f32),
) {
    use crate::grid::UnderlineStyle as U;
    let yb = cell_h - t;
    match style {
        U::None => {}
        U::Single => emit(0.0, yb, cell_w, t),
        U::Double => {
            emit(0.0, yb, cell_w, t);
            emit(0.0, yb - t * 2.0, cell_w, t);
        }
        U::Dotted => {
            let step = (t * 2.0).max(2.0);
            let mut dx = 0.0;
            while dx < cell_w {
                emit(dx, yb, t.min(cell_w - dx), t);
                dx += step;
            }
        }
        U::Dashed => {
            let dash = (cell_w / 3.0).max(2.0);
            let mut dx = 0.0;
            while dx < cell_w {
                emit(dx, yb, dash.min(cell_w - dx), t);
                dx += dash * 2.0;
            }
        }
        U::Curly => {
            let amp = t;
            let cy = yb - amp;
            let cols = cell_w.max(1.0) as i32;
            for i in 0..cols {
                let dx = i as f32;
                let yoff = ((dx / cell_w) * std::f32::consts::TAU).sin() * amp;
                emit(dx, cy + yoff, 1.0, t);
            }
        }
    }
}


pub struct Renderer {
    /// None for a headless (offscreen) renderer used by the dev capture harness
    surface: Option<wgpu::Surface<'static>>,
    /// offscreen render target + readback buffer for the headless harness; None
    /// for the normal windowed renderer, which draws straight to the surface.
    /// only read by the debug-only capture path, so release sees it as unused
    #[cfg_attr(not(debug_assertions), allow(dead_code))]
    offscreen: Option<(wgpu::Texture, wgpu::Buffer)>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,

    pipeline: wgpu::RenderPipeline,
    uniform_buffer: wgpu::Buffer,
    uniform_bind_group: wgpu::BindGroup,

    atlas_texture: wgpu::Texture,
    color_texture: wgpu::Texture,
    atlas_bind_group: wgpu::BindGroup,
    /// kept alive for the icon badge texture referenced by atlas_bind_group
    _icon_texture: wgpu::Texture,
    /// samplers + icon view referenced by atlas_bind_group, kept so the bind
    /// group can be rebuilt when the atlas grows (the 1024 -> 2048 grow path)
    sampler: wgpu::Sampler,
    icon_view: wgpu::TextureView,
    icon_sampler: wgpu::Sampler,
    color_sampler: wgpu::Sampler,
    /// the dim the gpu atlas textures were created at; when atlas.dim outgrows it
    /// upload_atlas recreates the textures + bind group at the new size
    atlas_gpu_dim: u32,
    /// set by the device-lost callback; render() recreates the gpu on the next
    /// frame so a driver reset / TDR doesn't permanently freeze the window
    device_lost: Arc<AtomicBool>,
    /// the window, kept so recreate() can rebuild the surface (None headless)
    window: Option<Arc<Window>>,

    instance_buffer: wgpu::Buffer,
    instance_capacity: u64,
    /// persistent CPU instance buffer reused across frames (cleared, not
    /// reallocated, each build) to avoid per-frame heap churn on the hot path
    scratch: Vec<Instance>,
    /// persistent pane-origin buffer reused across frames, same idea as scratch
    pane_scratch: Vec<PaneInfo>,

    atlas: GlyphAtlas,
    palette: Palette,

    scale: f32,
    pad: f32,
    content_pt: f32,
    content_line_height: f32,
    chrome_pt: f32,
    pub title_bar_h: f32,
    pub status_bar_h: f32,
    bg_alpha: f32,
    /// whether the surface supports translucency, and the user's chosen window
    /// opacity (0..1) applied as bg_alpha when it does
    transparent: bool,
    opacity: f32,
    start: Instant,
    /// independent clock for the power-on reveal, restarted the moment the
    /// window is shown so the whole animation plays in view (the gpu-init wait
    /// would otherwise eat most of it before the first visible frame)
    reveal_start: Instant,
    hovered: Option<Hot>,
    /// when the current hovered target was entered, for the hover fade-in
    hover_since: Option<Instant>,
    /// (previous active tab index, start) so the active-tab accent rail slides
    /// to the newly selected tab instead of teleporting
    tab_slide: Option<(usize, Instant)>,
    /// when a centered overlay (palette/find/market/pane menu) opened, so it
    /// blooms in instead of popping; `overlay_shown` tracks last-frame presence
    overlay_since: Option<Instant>,
    overlay_shown: bool,
    settings_open: bool,
    settings_p: f32,
    settings_scroll: f32,
    /// (first body-instance index, clip rect) for the scissored panel scroll
    panel_clip: Option<(u32, [f32; 4])>,
    cursor_style: CursorShape,
    cursor_blink: bool,
    bold_as_bright: bool,
    pane_pad_px: f32,
    content_font: Option<&'static str>,
    fonts: Vec<&'static str>,
    font_idx: usize,
    /// the gpu backend actually resolved at init (for the settings ABOUT block)
    backend_label: &'static str,
    settings_view: SettingsView,
    theme: ThemeId,
    /// user color overrides loaded from disk, applied on top of the theme
    color_overrides: Vec<(String, Rgb)>,
    broadcast: bool,
    /// cached background gradient quads, rebuilt only on size/theme change
    gradient_cache: Vec<Instance>,
    gradient_key: (u32, u32, ThemeId),
    pane_mode: bool,
    tabs: Vec<String>,
    active_tab: usize,
    status_git: Option<String>,
    status_clock: String,
    status_sessions: usize,
    /// cached status-bar strings so the per-frame paint doesn't re-format them:
    /// (cols, rows, "W×H") and (sessions, "n")
    status_size: (usize, usize, String),
    status_tabs: (usize, String),
    /// installed plugins (display name, enabled) for the settings PLUGINS panel
    plugins_installed: Vec<(String, bool)>,
    palette_view: Option<PaletteView>,
    pane_menu_view: Option<PaneMenuView>,
    find_view: Option<FindView>,
    market_view: Option<MarketView>,
    confirm_view: Option<ConfirmView>,
    rename_view: Option<RenameView>,
    /// plugin-declared Tier-1 widgets shown in the right-side dock; when
    /// non-empty the dock carves width off content_rect so panes reflow
    dock: Vec<DockWidget>,
    /// per-widget clickable band (x, y, w, h), parallel to `dock`; rebuilt each
    /// frame by draw_dock so widget_at can route clicks to the owning plugin
    dock_hitboxes: Vec<(f32, f32, f32, f32)>,

    pub cols: usize,
    pub rows: usize,
}

/// the uniform bind group layout (group 0): the screen-size uniform buffer
fn build_uniform_bgl(device: &wgpu::Device) -> wgpu::BindGroupLayout {
    device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("uniform-bgl"),
        entries: &[wgpu::BindGroupLayoutEntry {
            binding: 0,
            visibility: wgpu::ShaderStages::VERTEX,
            ty: wgpu::BindingType::Buffer {
                ty: wgpu::BufferBindingType::Uniform,
                has_dynamic_offset: false,
                min_binding_size: None,
            },
            count: None,
        }],
    })
}

/// the atlas bind group layout (group 1): alpha glyph atlas (0/1), app icon
/// (2/3), and the color-emoji atlas (4/5). kept in sync with shader.wgsl
fn build_atlas_bgl(device: &wgpu::Device) -> wgpu::BindGroupLayout {
    let tex = |binding| wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::FRAGMENT,
        ty: wgpu::BindingType::Texture {
            sample_type: wgpu::TextureSampleType::Float { filterable: true },
            view_dimension: wgpu::TextureViewDimension::D2,
            multisampled: false,
        },
        count: None,
    };
    let samp = |binding| wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::FRAGMENT,
        ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
        count: None,
    };
    device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("atlas-bgl"),
        entries: &[tex(0), samp(1), tex(2), samp(3), tex(4), samp(5)],
    })
}

/// create the R8 coverage atlas + RGBA color atlas at `dim` and the 6-entry
/// atlas bind group over them (reusing the fixed icon view + the three
/// samplers). shared by from_parts and the 1024 -> 2048 grow so the layout can
/// never drift between the two
fn make_atlas_bind_group(
    device: &wgpu::Device,
    dim: u32,
    sampler: &wgpu::Sampler,
    icon_view: &wgpu::TextureView,
    icon_sampler: &wgpu::Sampler,
    color_sampler: &wgpu::Sampler,
) -> (wgpu::Texture, wgpu::Texture, wgpu::BindGroup) {
    let atlas_texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("glyph-atlas"),
        size: wgpu::Extent3d { width: dim, height: dim, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::R8Unorm,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    let atlas_view = atlas_texture.create_view(&wgpu::TextureViewDescriptor::default());
    let color_texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("color-glyph-atlas"),
        size: wgpu::Extent3d { width: dim, height: dim, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8UnormSrgb,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    let color_view = color_texture.create_view(&wgpu::TextureViewDescriptor::default());
    let bgl = build_atlas_bgl(device);
    let atlas_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("atlas-bg"),
        layout: &bgl,
        entries: &[
            wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&atlas_view) },
            wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::Sampler(sampler) },
            wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::TextureView(icon_view) },
            wgpu::BindGroupEntry { binding: 3, resource: wgpu::BindingResource::Sampler(icon_sampler) },
            wgpu::BindGroupEntry { binding: 4, resource: wgpu::BindingResource::TextureView(&color_view) },
            wgpu::BindGroupEntry { binding: 5, resource: wgpu::BindingResource::Sampler(color_sampler) },
        ],
    });
    (atlas_texture, color_texture, atlas_bind_group)
}

/// the cell render pipeline (shader + layout + premultiplied-alpha blend),
/// shared by Renderer::new and the headless pipeline-validation test so the
/// test exercises the real layout-vs-shader binding match
fn build_cell_pipeline(
    device: &wgpu::Device,
    uniform_bgl: &wgpu::BindGroupLayout,
    atlas_bgl: &wgpu::BindGroupLayout,
    format: wgpu::TextureFormat,
) -> wgpu::RenderPipeline {
    let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("cell-shader"),
        source: wgpu::ShaderSource::Wgsl(include_str!("shader.wgsl").into()),
    });
    let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("pipeline-layout"),
        bind_group_layouts: &[Some(uniform_bgl), Some(atlas_bgl)],
        immediate_size: 0,
    });
    // premultiplied-alpha over operator
    let blend = wgpu::BlendState {
        color: wgpu::BlendComponent {
            src_factor: wgpu::BlendFactor::One,
            dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
            operation: wgpu::BlendOperation::Add,
        },
        alpha: wgpu::BlendComponent {
            src_factor: wgpu::BlendFactor::One,
            dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
            operation: wgpu::BlendOperation::Add,
        },
    };
    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("cell-pipeline"),
        layout: Some(&pipeline_layout),
        vertex: wgpu::VertexState {
            module: &shader,
            entry_point: Some("vs_main"),
            buffers: &[wgpu::VertexBufferLayout {
                array_stride: std::mem::size_of::<Instance>() as u64,
                step_mode: wgpu::VertexStepMode::Instance,
                attributes: &INSTANCE_ATTRS,
            }],
            compilation_options: Default::default(),
        },
        fragment: Some(wgpu::FragmentState {
            module: &shader,
            entry_point: Some("fs_main"),
            targets: &[Some(wgpu::ColorTargetState {
                format,
                blend: Some(blend),
                write_mask: wgpu::ColorWrites::ALL,
            })],
            compilation_options: Default::default(),
        }),
        primitive: wgpu::PrimitiveState::default(),
        depth_stencil: None,
        multisample: wgpu::MultisampleState::default(),
        multiview_mask: None,
        cache: None,
    })
}

/// label the resolved gpu backend for the settings ABOUT block
fn backend_label(b: wgpu::Backend) -> &'static str {
    match b {
        wgpu::Backend::Dx12 => "wgpu / DX12",
        wgpu::Backend::Vulkan => "wgpu / Vulkan",
        wgpu::Backend::Gl => "wgpu / GL",
        wgpu::Backend::Metal => "wgpu / Metal",
        wgpu::Backend::BrowserWebGpu => "wgpu / WebGPU",
        _ => "wgpu",
    }
}

/// the complete set of device-owned gpu handles. bundled so the windowed init
/// (from_parts) and a device-loss recreate rebuild exactly the same set with
/// nothing left dangling
struct GpuResources {
    uniform_buffer: wgpu::Buffer,
    uniform_bind_group: wgpu::BindGroup,
    pipeline: wgpu::RenderPipeline,
    instance_buffer: wgpu::Buffer,
    instance_capacity: u64,
    atlas_texture: wgpu::Texture,
    color_texture: wgpu::Texture,
    atlas_bind_group: wgpu::BindGroup,
    icon_texture: wgpu::Texture,
    sampler: wgpu::Sampler,
    icon_view: wgpu::TextureView,
    icon_sampler: wgpu::Sampler,
    color_sampler: wgpu::Sampler,
}

/// build every device-owned gpu handle from a device + queue. atlas/color
/// textures are sized to `atlas_dim`; the cpu glyph bitmaps upload separately
/// via upload_atlas (the caller flags the atlas dirty after a rebuild)
fn build_gpu_resources(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    atlas_dim: u32,
    format: wgpu::TextureFormat,
) -> GpuResources {
    let uniform_buffer = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("uniforms"),
        size: std::mem::size_of::<Uniforms>() as u64,
        usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    let uniform_bgl = build_uniform_bgl(device);
    let uniform_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("uniform-bg"),
        layout: &uniform_bgl,
        entries: &[wgpu::BindGroupEntry {
            binding: 0,
            resource: uniform_buffer.as_entire_binding(),
        }],
    });

    let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
        label: Some("atlas-sampler"),
        mag_filter: wgpu::FilterMode::Nearest,
        min_filter: wgpu::FilterMode::Nearest,
        ..Default::default()
    });

    // the app icon (">_<" master, pre-decoded to 128x128 RGBA) lives in a small
    // color texture drawn as a title-bar badge; a full mip chain keeps the ~20px
    // downscale crisp (a single level sampled 6x down looks fuzzy/aliased)
    const ICON_DIM: u32 = 128;
    let icon_rgba: &[u8] = include_bytes!("../../assets/icon_128.rgba");
    let icon_mips = build_mips(icon_rgba, ICON_DIM);
    let icon_texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("app-icon"),
        size: wgpu::Extent3d { width: ICON_DIM, height: ICON_DIM, depth_or_array_layers: 1 },
        mip_level_count: icon_mips.len() as u32,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8UnormSrgb,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    for (level, (dim, data)) in icon_mips.iter().enumerate() {
        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &icon_texture,
                mip_level: level as u32,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            data,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(dim * 4),
                rows_per_image: Some(*dim),
            },
            wgpu::Extent3d { width: *dim, height: *dim, depth_or_array_layers: 1 },
        );
    }
    let icon_view = icon_texture.create_view(&wgpu::TextureViewDescriptor::default());
    let icon_sampler = device.create_sampler(&wgpu::SamplerDescriptor {
        label: Some("icon-sampler"),
        mag_filter: wgpu::FilterMode::Linear,
        min_filter: wgpu::FilterMode::Linear,
        mipmap_filter: wgpu::MipmapFilterMode::Linear,
        ..Default::default()
    });

    let color_sampler = device.create_sampler(&wgpu::SamplerDescriptor {
        label: Some("color-atlas-sampler"),
        mag_filter: wgpu::FilterMode::Linear,
        min_filter: wgpu::FilterMode::Linear,
        ..Default::default()
    });

    let (atlas_texture, color_texture, atlas_bind_group) =
        make_atlas_bind_group(device, atlas_dim, &sampler, &icon_view, &icon_sampler, &color_sampler);

    let atlas_bgl = build_atlas_bgl(device);
    let pipeline = build_cell_pipeline(device, &uniform_bgl, &atlas_bgl, format);

    let instance_capacity = 8192u64;
    let instance_buffer = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("instances"),
        size: instance_capacity * std::mem::size_of::<Instance>() as u64,
        usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });

    GpuResources {
        uniform_buffer,
        uniform_bind_group,
        pipeline,
        instance_buffer,
        instance_capacity,
        atlas_texture,
        color_texture,
        atlas_bind_group,
        icon_texture,
        sampler,
        icon_view,
        icon_sampler,
        color_sampler,
    }
}

impl Renderer {
    pub fn new(window: Arc<Window>, content_pt: f32, chrome_pt: f32, backend: BackendChoice) -> Result<Renderer> {
        let size = window.inner_size();
        let scale = window.scale_factor() as f32;

        // build the bundled-font glyph atlas on a worker thread so its font load
        // overlaps the gpu adapter/device request below (mostly driver wait)
        // instead of running sequentially after it — shaves startup latency
        let atlas_handle = std::thread::spawn(move || GlyphAtlas::new(content_pt, chrome_pt, scale, None, 1.32));

        // build instance+surface+adapter for a backend set; DX12 is the Windows
        // default (Vulkan is slow under injected overlay layers — OBS/Overwolf)
        let try_init = |backends: wgpu::Backends, force_fallback: bool| -> Result<(wgpu::Instance, wgpu::Surface<'static>, wgpu::Adapter)> {
            let mut desc = wgpu::InstanceDescriptor::new_without_display_handle_from_env();
            desc.backends = backends;
            let instance = wgpu::Instance::new(desc);
            let surface = instance.create_surface(window.clone())?;
            let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
                // a 2D terminal doesn't need the discrete GPU; low-power picks the
                // integrated adapter, which inits faster and saves battery
                power_preference: wgpu::PowerPreference::LowPower,
                compatible_surface: Some(&surface),
                force_fallback_adapter: force_fallback,
            }))
            .map_err(|e| anyhow!("no suitable GPU adapter: {e}"))?;
            Ok((instance, surface, adapter))
        };

        // try the chosen backend; if it has no adapter, fall back to all backends
        // so a bad choice can never prevent launch
        let chosen = backend.to_backends();
        let fallback = if cfg!(windows) {
            wgpu::Backends::DX12 | wgpu::Backends::VULKAN | wgpu::Backends::GL
        } else {
            wgpu::Backends::all()
        };
        let (_instance, surface, adapter) = match try_init(chosen, false) {
            Ok(t) => t,
            Err(e) => {
                log::warn!("backend {chosen:?} unavailable ({e:#}); falling back");
                match try_init(fallback, false) {
                    Ok(t) => t,
                    Err(e2) => {
                        // last resort: a software/WARP adapter so termie still
                        // launches on a broken/updating driver, an RDP session, or
                        // a locked-down VM — degraded but running beats not at all
                        log::warn!("no hardware GPU adapter ({e2:#}); using software fallback");
                        try_init(fallback, true)?
                    }
                }
            }
        };
        crate::timing("  gpu: adapter+surface");

        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("termie-device"),
            required_features: wgpu::Features::empty(),
            required_limits: wgpu::Limits::default(),
            ..Default::default()
        }))?;
        crate::timing("  gpu: device");

        let caps = surface.get_capabilities(&adapter);
        let format = caps
            .formats
            .iter()
            .copied()
            .find(|f| f.is_srgb())
            .unwrap_or(caps.formats[0]);

        // transparent compositing if the surface supports premultiplied alpha
        let transparent = caps
            .alpha_modes
            .contains(&wgpu::CompositeAlphaMode::PreMultiplied);
        let alpha_mode = if transparent {
            wgpu::CompositeAlphaMode::PreMultiplied
        } else {
            wgpu::CompositeAlphaMode::Opaque
        };
        log::info!("surface format={format:?} alpha_mode={alpha_mode:?} transparent={transparent}");

        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width: size.width.max(1),
            height: size.height.max(1),
            present_mode: wgpu::PresentMode::Fifo,
            // a terminal renders far below a refresh of gpu work, so queue only
            // one frame ahead — cuts up to a frame of input-to-photon latency
            desired_maximum_frame_latency: 1,
            alpha_mode,
            view_formats: vec![],
        };
        surface.configure(&device, &config);
        crate::timing("  gpu: surface configured");

        let atlas = atlas_handle.join().expect("atlas build thread panicked");
        crate::timing("  gpu: atlas joined");
        let mut r = Self::from_parts(
            device, queue, Some(surface), format, config, atlas, scale, content_pt, chrome_pt, transparent,
        );
        r.backend_label = backend_label(adapter.get_info().backend);
        r.window = Some(window);
        Ok(r)
    }

    /// rebuild the gpu (instance + surface + adapter + device + queue + all
    /// device-owned handles) after a device loss, preserving the cpu-side glyph
    /// atlas and all ui state. a no-op for the headless renderer
    pub fn recreate(&mut self, window: Arc<Window>) -> Result<()> {
        if self.surface.is_none() {
            return Ok(());
        }
        // a surface is bound to its instance and a fresh device can't adopt a
        // stale one, so the whole chain is rebuilt. recovery takes any adapter,
        // including the software/WARP fallback
        let backends = if cfg!(windows) {
            wgpu::Backends::DX12 | wgpu::Backends::VULKAN | wgpu::Backends::GL
        } else {
            wgpu::Backends::all()
        };
        let try_init = |force_fallback: bool| -> Result<(wgpu::Instance, wgpu::Surface<'static>, wgpu::Adapter)> {
            let mut desc = wgpu::InstanceDescriptor::new_without_display_handle_from_env();
            desc.backends = backends;
            let instance = wgpu::Instance::new(desc);
            let surface = instance.create_surface(window.clone())?;
            let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::LowPower,
                compatible_surface: Some(&surface),
                force_fallback_adapter: force_fallback,
            }))
            .map_err(|e| anyhow!("no GPU adapter on recreate: {e}"))?;
            Ok((instance, surface, adapter))
        };
        let (_instance, surface, adapter) = match try_init(false) {
            Ok(t) => t,
            Err(_) => try_init(true)?,
        };
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("termie-device"),
            required_features: wgpu::Features::empty(),
            required_limits: wgpu::Limits::default(),
            ..Default::default()
        }))?;

        let caps = surface.get_capabilities(&adapter);
        let format = caps
            .formats
            .iter()
            .copied()
            .find(|f| f.is_srgb())
            .unwrap_or(caps.formats[0]);
        let transparent = caps
            .alpha_modes
            .contains(&wgpu::CompositeAlphaMode::PreMultiplied);
        let mut config = self.config.clone();
        config.format = format;
        config.alpha_mode = if transparent {
            wgpu::CompositeAlphaMode::PreMultiplied
        } else {
            wgpu::CompositeAlphaMode::Opaque
        };
        surface.configure(&device, &config);

        // re-arm the lost callback on the new device, reusing the same flag
        {
            let flag = self.device_lost.clone();
            device.set_device_lost_callback(move |reason, msg| {
                if reason != wgpu::DeviceLostReason::Destroyed {
                    log::error!("gpu device lost: {reason:?}: {msg}");
                    flag.store(true, Ordering::SeqCst);
                }
            });
        }

        let gpu = build_gpu_resources(&device, &queue, self.atlas.dim, format);
        // swap in the new device-owned handles; every cpu field (atlas, palette,
        // tabs, ...) is preserved so the warm glyph cache and ui state survive
        self.surface = Some(surface);
        self.device = device;
        self.queue = queue;
        self.config = config;
        // recovery may have landed on a different backend (incl. software/WARP);
        // keep the settings ABOUT panel honest rather than showing the old one
        self.backend_label = backend_label(adapter.get_info().backend);
        self.transparent = transparent;
        self.bg_alpha = if transparent { self.opacity } else { 1.0 };
        self.uniform_buffer = gpu.uniform_buffer;
        self.uniform_bind_group = gpu.uniform_bind_group;
        self.pipeline = gpu.pipeline;
        self.instance_buffer = gpu.instance_buffer;
        self.instance_capacity = gpu.instance_capacity;
        self.atlas_texture = gpu.atlas_texture;
        self.color_texture = gpu.color_texture;
        self.atlas_bind_group = gpu.atlas_bind_group;
        self._icon_texture = gpu.icon_texture;
        self.sampler = gpu.sampler;
        self.icon_view = gpu.icon_view;
        self.icon_sampler = gpu.icon_sampler;
        self.color_sampler = gpu.color_sampler;
        self.atlas_gpu_dim = self.atlas.dim;
        // re-upload the warm cpu glyph bitmaps onto the fresh textures
        self.atlas.dirty = true;
        self.atlas.dirty_y = None;
        self.atlas.color_dirty = true;
        self.atlas.color_dirty_y = None;
        self.device_lost.store(false, Ordering::SeqCst);
        log::info!("gpu device recreated");
        Ok(())
    }

    /// build the renderer from already-created gpu parts. shared by the windowed
    /// `new` and the headless capture constructor; `surface` is None offscreen
    #[allow(clippy::too_many_arguments)]
    fn from_parts(
        device: wgpu::Device,
        queue: wgpu::Queue,
        surface: Option<wgpu::Surface<'static>>,
        format: wgpu::TextureFormat,
        config: wgpu::SurfaceConfiguration,
        atlas: GlyphAtlas,
        scale: f32,
        content_pt: f32,
        chrome_pt: f32,
        transparent: bool,
    ) -> Renderer {
        // the bundled default plus any common monospace families present on the
        // system (initially just the bundled one — system fonts load lazily)
        let fonts = Self::detect_fonts(&atlas);

        let GpuResources {
            uniform_buffer,
            uniform_bind_group,
            pipeline,
            instance_buffer,
            instance_capacity,
            atlas_texture,
            color_texture,
            atlas_bind_group,
            icon_texture,
            sampler,
            icon_view,
            icon_sampler,
            color_sampler,
        } = build_gpu_resources(&device, &queue, atlas.dim, format);
        crate::timing("  gpu: resources built");

        let pad = (10.0 * scale).round();
        let chrome_h = atlas.metrics(FontId::Chrome).cell_h;
        let title_bar_h = (chrome_h + (14.0 * scale)).round();
        let status_bar_h = (chrome_h + (8.0 * scale)).round();

        // arm device-lost recovery before `device` moves into the struct; the
        // callback fires on a driver reset / TDR (not on our own teardown) and
        // render() rebuilds the gpu on the next frame
        let device_lost = Arc::new(AtomicBool::new(false));
        {
            let flag = device_lost.clone();
            device.set_device_lost_callback(move |reason, msg| {
                if reason != wgpu::DeviceLostReason::Destroyed {
                    log::error!("gpu device lost: {reason:?}: {msg}");
                    flag.store(true, Ordering::SeqCst);
                }
            });
        }

        let mut r = Renderer {
            surface,
            offscreen: None,
            device,
            queue,
            config,
            pipeline,
            uniform_buffer,
            uniform_bind_group,
            atlas_texture,
            color_texture,
            atlas_bind_group,
            _icon_texture: icon_texture,
            sampler,
            icon_view,
            icon_sampler,
            color_sampler,
            atlas_gpu_dim: atlas.dim,
            device_lost,
            window: None,
            instance_buffer,
            instance_capacity,
            scratch: Vec::new(),
            pane_scratch: Vec::new(),
            atlas,
            palette: Palette::from_theme(ThemeId::Instrument),
            theme: ThemeId::Instrument,
            color_overrides: Vec::new(),
            broadcast: false,
            gradient_cache: Vec::new(),
            gradient_key: (0, 0, ThemeId::Instrument),
            scale,
            pad,
            content_pt,
            content_line_height: 1.32,
            chrome_pt,
            title_bar_h,
            status_bar_h,
            bg_alpha: if transparent { 0.85 } else { 1.0 },
            transparent,
            opacity: 0.85,
            start: Instant::now(),
            reveal_start: Instant::now(),
            hovered: None,
            hover_since: None,
            tab_slide: None,
            overlay_since: None,
            overlay_shown: false,
            settings_open: false,
            settings_p: 0.0,
            settings_scroll: 0.0,
            panel_clip: None,
            cursor_style: CursorShape::Block,
            cursor_blink: true,
            bold_as_bright: true,
            pane_pad_px: 6.0,
            content_font: None,
            backend_label: "wgpu",
            fonts,
            font_idx: 0,
            settings_view: SettingsView::default(),
            pane_mode: false,
            tabs: Vec::new(),
            active_tab: 0,
            status_git: None,
            status_clock: String::new(),
            status_sessions: 1,
            status_size: (usize::MAX, usize::MAX, String::new()),
            status_tabs: (usize::MAX, String::new()),
            plugins_installed: Vec::new(),
            palette_view: None,
            pane_menu_view: None,
            find_view: None,
            market_view: None,
            confirm_view: None,
            rename_view: None,
            dock: Vec::new(),
            dock_hitboxes: Vec::new(),
            cols: 0,
            rows: 0,
        };
        r.recompute_grid_size();
        r
    }

    fn recompute_grid_size(&mut self) {
        let m = self.atlas.metrics(FontId::Content);
        let chrome = self.title_bar_h + self.status_bar_h;
        let usable_w = (self.config.width as f32 - self.pad * 2.0).max(m.cell_w);
        let usable_h = (self.config.height as f32 - chrome - self.pad).max(m.cell_h);
        self.cols = (usable_w / m.cell_w).floor().max(1.0) as usize;
        self.rows = (usable_h / m.cell_h).floor().max(1.0) as usize;
    }

    pub fn resize(&mut self, width: u32, height: u32) -> (usize, usize) {
        if width == 0 || height == 0 {
            return (self.cols, self.rows);
        }
        self.config.width = width;
        self.config.height = height;
        if let Some(surface) = &self.surface {
            surface.configure(&self.device, &self.config);
        }
        self.recompute_grid_size();
        // grow the GPU instance buffer eagerly here (off the render hot path) so
        // the first paint after a resize never reallocates mid-frame
        let needed = (self.cols * self.rows) as u64 + 1024;
        if needed > self.instance_capacity {
            self.instance_capacity = needed.next_power_of_two();
            self.instance_buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("instances"),
                size: self.instance_capacity * std::mem::size_of::<Instance>() as u64,
                usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
        }
        (self.cols, self.rows)
    }

    /// the pixel rect available for terminal panes (between the bars). the
    /// plugin dock, when present, carves its width off the right so panes reflow
    pub fn palette(&self) -> &crate::color::Palette {
        &self.palette
    }

    pub fn content_rect(&self) -> (f32, f32, f32, f32) {
        let x = self.pad;
        let y = self.title_bar_h;
        let w = (self.config.width as f32 - self.pad * 2.0 - self.dock_w()).max(1.0);
        let h = (self.config.height as f32 - self.title_bar_h - self.status_bar_h - self.pad)
            .max(1.0);
        (x, y, w, h)
    }

    /// width reserved for the right-side plugin dock (0 when no widgets). capped
    /// so the dock can never crowd out the terminal on a narrow window
    fn dock_w(&self) -> f32 {
        if self.dock.is_empty() {
            0.0
        } else {
            (224.0 * self.scale).round().min(self.config.width as f32 * 0.4)
        }
    }

    /// replace the dock's widget list. returns true if the dock's presence
    /// toggled (empty<->non-empty), since that changes content_rect and the
    /// caller must relayout panes
    pub fn set_dock(&mut self, widgets: Vec<DockWidget>) -> bool {
        let was = !self.dock.is_empty();
        self.dock = widgets;
        // draw_dock repopulates these each frame, but an emptied dock never
        // draws, so drop the stale bands now
        if self.dock.is_empty() {
            self.dock_hitboxes.clear();
        }
        was == self.dock.is_empty()
    }

    /// dock index of the widget whose clickable band contains (px, py), if any.
    /// the index is parallel to the widget set passed to set_dock, so the caller
    /// can map it back to the owning plugin
    pub fn widget_at(&self, px: f32, py: f32) -> Option<usize> {
        self.dock_hitboxes
            .iter()
            .position(|&(x, y, w, h)| px >= x && px < x + w && py >= y && py < y + h)
    }

    /// inner padding inside each pane rect (keeps text off the dividers)
    fn pane_pad(&self) -> f32 {
        (self.pane_pad_px * self.scale).round()
    }

    /// given a pane's pixel rect, the grid origin + cols/rows that fit inside it
    pub fn pane_metrics(&self, rect: (f32, f32, f32, f32)) -> (f32, f32, usize, usize) {
        let m = self.atlas.metrics(FontId::Content);
        let p = self.pane_pad();
        let ox = (rect.0 + p).round();
        let oy = (rect.1 + p).round();
        let cols = (((rect.2 - p * 2.0) / m.cell_w).floor()).max(1.0) as usize;
        let rows = (((rect.3 - p * 2.0) / m.cell_h).floor()).max(1.0) as usize;
        (ox, oy, cols, rows)
    }

    /// the (col, row) cell at a pixel position within a pane rect, clamped
    pub fn cell_at(&self, rect: (f32, f32, f32, f32), x: f32, y: f32) -> (usize, usize) {
        let m = self.atlas.metrics(FontId::Content);
        let (ox, oy, cols, rows) = self.pane_metrics(rect);
        let col = (((x - ox) / m.cell_w).floor().max(0.0) as usize).min(cols.saturating_sub(1));
        let row = (((y - oy) / m.cell_h).floor().max(0.0) as usize).min(rows.saturating_sub(1));
        (col, row)
    }

    pub fn set_hovered(&mut self, h: Option<Hot>) -> bool {
        let changed = self.hovered != h;
        if changed {
            // restart the fade-in when entering a target; clear it when leaving
            self.hover_since = h.is_some().then(Instant::now);
        }
        self.hovered = h;
        changed
    }

    /// hover fade-in factor (0..1) for the currently hovered chrome target
    fn hover_ease(&self) -> f32 {
        const DUR: f32 = 0.11;
        match self.hover_since {
            None => 1.0,
            Some(t) => {
                let e = (t.elapsed().as_secs_f32() / DUR).clamp(0.0, 1.0);
                1.0 - (1.0 - e).powi(3)
            }
        }
    }

    /// true while the hover fade-in is still in flight (drives redraws)
    pub fn hover_animating(&self) -> bool {
        self.hover_since.is_some_and(|t| t.elapsed().as_secs_f32() < 0.11)
    }

    pub fn set_pane_mode(&mut self, on: bool) {
        self.pane_mode = on;
    }

    pub fn set_broadcast(&mut self, on: bool) {
        self.broadcast = on;
    }

    pub fn cycle_cursor(&mut self) {
        self.cursor_style = match self.cursor_style {
            CursorShape::Bar => CursorShape::Block,
            CursorShape::Block => CursorShape::Underline,
            CursorShape::Underline => CursorShape::Bar,
        };
    }

    pub fn cursor_style_name(&self) -> &'static str {
        match self.cursor_style {
            CursorShape::Bar => "beam",
            CursorShape::Block => "block",
            CursorShape::Underline => "underline",
        }
    }

    pub fn cycle_theme(&mut self) {
        self.theme = self.theme.next();
        self.palette = self.themed_palette();
        self.atlas.dirty = true;
    }

    fn themed_palette(&self) -> Palette {
        let mut p = Palette::from_theme(self.theme);
        p.apply_overrides(&self.color_overrides);
        p
    }

    /// install user color overrides (from disk) and rebuild the active palette
    pub fn set_color_overrides(&mut self, overrides: Vec<(String, Rgb)>) {
        self.color_overrides = overrides;
        self.palette = self.themed_palette();
    }

    pub fn set_theme(&mut self, id: ThemeId) {
        if self.theme != id {
            self.theme = id;
            self.palette = self.themed_palette();
            self.atlas.dirty = true;
        }
    }

    pub fn toggle_cursor_blink(&mut self) {
        self.cursor_blink = !self.cursor_blink;
    }

    /// nudge the inner pane padding (px, pre-scale); returns true if it changed
    pub fn set_pane_pad(&mut self, delta: f32) -> bool {
        let next = (self.pane_pad_px + delta).clamp(0.0, 20.0);
        let changed = next != self.pane_pad_px;
        self.pane_pad_px = next;
        changed
    }

    pub fn set_settings(&mut self, v: SettingsView) {
        self.settings_view = v;
    }

    /// the installed plugins (display name, enabled) shown in the settings panel
    pub fn set_plugins(&mut self, list: Vec<(String, bool)>) {
        self.plugins_installed = list;
    }

    /// drive the slide-in panel: `open` = interactive, `p` = docked fraction (0..1)
    pub fn set_settings_panel(&mut self, open: bool, p: f32) {
        self.settings_open = open;
        self.settings_p = p;
    }

    pub fn reset_settings_scroll(&mut self) {
        self.settings_scroll = 0.0;
    }

    pub fn scroll_settings(&mut self, delta: f32) {
        let g = self.settings_geom();
        let max = (g.content_h - (g.body_bottom - g.body_top)).max(0.0);
        self.settings_scroll = (self.settings_scroll + delta).clamp(0.0, max);
    }

    pub fn in_settings_panel(&self, x: f32, y: f32) -> bool {
        let g = self.settings_geom();
        x >= g.panel_x && x < g.panel_x + g.panel_w && y >= g.panel_top && y < g.panel_top + g.panel_h
    }

    pub fn content_pt(&self) -> f32 {
        self.content_pt
    }

    /// re-measure the glyph atlas at a new content point size; returns new (cols, rows)
    pub fn set_content_pt(&mut self, pt: f32) -> (usize, usize) {
        let pt = pt.clamp(8.0, 32.0);
        // re-rasterizing the atlas is the expensive part (two cosmic-text shape
        // passes + clearing ~5MB of atlas buffers); skip it when the size is
        // unchanged — notably at boot, where the worker already built the atlas
        // at exactly this size. still recompute the grid (cols/rows start at 0)
        if pt != self.content_pt {
            self.content_pt = pt;
            self.atlas.reconfigure(self.content_pt, self.chrome_pt, self.scale, self.content_font, self.content_line_height);
        }
        self.recompute_grid_size();
        (self.cols, self.rows)
    }

    /// re-raster the atlas and recompute chrome/grid metrics for a new device
    /// scale (per-monitor dpi change), so a window dragged between monitors of
    /// different dpi stays crisp; mirrors the scale-derived geometry in from_parts
    pub fn set_scale(&mut self, scale: f32) {
        if (scale - self.scale).abs() < f32::EPSILON {
            return;
        }
        self.scale = scale;
        // re-raster glyphs at the new device scale (clears + repacks the atlas)
        self.atlas.reconfigure(self.content_pt, self.chrome_pt, scale, self.content_font, self.content_line_height);
        // pad + bar heights are all scale-derived; recompute from the new metrics
        self.pad = (10.0 * scale).round();
        let chrome_h = self.atlas.metrics(FontId::Chrome).cell_h;
        self.title_bar_h = (chrome_h + (14.0 * scale)).round();
        self.status_bar_h = (chrome_h + (8.0 * scale)).round();
        self.recompute_grid_size();
    }

    pub fn pane_pad_px(&self) -> f32 {
        self.pane_pad_px
    }

    pub fn set_pane_pad_px(&mut self, v: f32) {
        self.pane_pad_px = v.clamp(0.0, 20.0);
    }

    /// window opacity as a percentage (50..100) for the settings UI + persistence
    pub fn opacity_pct(&self) -> i32 {
        (self.opacity * 100.0).round() as i32
    }

    /// set window opacity from a percentage; only takes visible effect when the
    /// surface supports translucency (otherwise the window stays opaque)
    pub fn set_opacity_pct(&mut self, pct: i32) {
        self.opacity = (pct as f32 / 100.0).clamp(0.5, 1.0);
        self.bg_alpha = if self.transparent { self.opacity } else { 1.0 };
    }

    /// nudge opacity by a percentage delta; returns true if it changed
    pub fn nudge_opacity(&mut self, d: i32) -> bool {
        let before = self.opacity_pct();
        self.set_opacity_pct(before + d);
        self.opacity_pct() != before
    }

    pub fn cursor_blink(&self) -> bool {
        self.cursor_blink
    }

    pub fn set_cursor_blink(&mut self, on: bool) {
        self.cursor_blink = on;
    }

    pub fn set_bold_as_bright(&mut self, on: bool) {
        self.bold_as_bright = on;
    }

    pub fn bold_as_bright(&self) -> bool {
        self.bold_as_bright
    }

    /// set the content line-height multiplier; re-rasters the atlas and recomputes
    /// the grid since cell height (and thus the row count) changes
    pub fn set_line_height(&mut self, lh: f32) {
        let lh = lh.clamp(0.8, 3.0);
        if (lh - self.content_line_height).abs() < f32::EPSILON {
            return;
        }
        self.content_line_height = lh;
        self.atlas.reconfigure(self.content_pt, self.chrome_pt, self.scale, self.content_font, lh);
        self.recompute_grid_size();
    }

    pub fn line_height(&self) -> f32 {
        self.content_line_height
    }

    pub fn set_cursor_style(&mut self, s: CursorShape) {
        self.cursor_style = s;
    }

    pub fn theme(&self) -> ThemeId {
        self.theme
    }

    pub fn font_name(&self) -> &'static str {
        self.fonts[self.font_idx]
    }

    /// the bundled default plus any common monospace families present in the db
    fn detect_fonts(atlas: &GlyphAtlas) -> Vec<&'static str> {
        let mut fonts: Vec<&'static str> = vec![atlas.content_family()];
        for cand in [
            "Cascadia Code",
            "Cascadia Mono",
            "JetBrains Mono",
            "Consolas",
            "Lucida Console",
            "Courier New",
        ] {
            if !fonts.iter().any(|f| f.eq_ignore_ascii_case(cand)) && atlas.has_family(cand) {
                fonts.push(cand);
            }
        }
        fonts
    }

    /// rasterize printable ASCII into the atlas ahead of first content paint
    /// (deferred off the critical startup path) so shell output renders from a
    /// warm cache. the changed atlas rows upload on the next render
    pub fn prewarm_glyphs(&mut self) {
        self.atlas.prewarm_ascii();
    }

    /// scan system fonts once (deferred off startup) so the font picker can
    /// offer them and so non-Latin glyphs have fallbacks. cheap no-op after the
    /// first call. returns true if it scanned now
    pub fn ensure_system_fonts(&mut self) -> bool {
        if !self.atlas.load_system_fonts() {
            return false;
        }
        // any glyph cached as missing before the scan can now resolve via a
        // newly loaded fallback font, so drop those tofu entries
        self.atlas.invalidate_missing();
        let cur = self.fonts[self.font_idx];
        self.fonts = Self::detect_fonts(&self.atlas);
        self.font_idx = self.fonts.iter().position(|f| *f == cur).unwrap_or(0);
        true
    }

    /// switch to a content font by family name. resolves against the known list
    /// first; if the name isn't a built-in candidate but the family is actually
    /// installed, inject it so any user-configured font resolves (not just the
    /// six hardcoded ones). the leak is bounded: font switches are rare and only
    /// a handful of distinct families are ever set in a session
    pub fn set_font_by_name(&mut self, name: &str) -> (usize, usize) {
        let idx = match self.fonts.iter().position(|f| f.eq_ignore_ascii_case(name)) {
            Some(i) => Some(i),
            None if self.atlas.has_family(name) => {
                let leaked: &'static str = Box::leak(name.to_string().into_boxed_str());
                self.fonts.push(leaked);
                Some(self.fonts.len() - 1)
            }
            None => None,
        };
        if let Some(i) = idx
            && i != self.font_idx {
                self.font_idx = i;
                self.content_font = if i == 0 { None } else { Some(self.fonts[i]) };
                self.atlas.reconfigure(self.content_pt, self.chrome_pt, self.scale, self.content_font, self.content_line_height);
                self.recompute_grid_size();
            }
        (self.cols, self.rows)
    }

    /// switch to the next available content font; returns new (cols, rows)
    pub fn cycle_font(&mut self) -> (usize, usize) {
        if self.fonts.len() > 1 {
            self.font_idx = (self.font_idx + 1) % self.fonts.len();
            // index 0 is the bundled default (use None so the atlas picks it)
            self.content_font = if self.font_idx == 0 { None } else { Some(self.fonts[self.font_idx]) };
            self.atlas.reconfigure(self.content_pt, self.chrome_pt, self.scale, self.content_font, self.content_line_height);
            self.recompute_grid_size();
        }
        (self.cols, self.rows)
    }

    pub fn set_tabs(&mut self, tabs: Vec<String>, active: usize) {
        if tabs.len() != self.tabs.len() {
            // a tab opened/closed — every rect shifts, so don't slide across it
            self.tab_slide = None;
        } else if active != self.active_tab && !self.tabs.is_empty() {
            // a pure switch: slide the accent rail from the old tab to the new
            self.tab_slide = Some((self.active_tab, Instant::now()));
        }
        self.tabs = tabs;
        self.active_tab = active;
    }

    const TAB_SLIDE: f32 = 0.13;

    /// eased 0→1 progress of the active-tab rail slide, with the source index;
    /// None once settled (the rail just sits on the active tab)
    fn tab_slide_p(&self) -> Option<(usize, f32)> {
        let (old, t) = self.tab_slide?;
        let e = (t.elapsed().as_secs_f32() / Self::TAB_SLIDE).clamp(0.0, 1.0);
        if e >= 1.0 {
            None
        } else {
            Some((old, 1.0 - (1.0 - e).powi(3)))
        }
    }

    pub fn tab_animating(&self) -> bool {
        self.tab_slide
            .map(|(_, t)| t.elapsed().as_secs_f32() < Self::TAB_SLIDE)
            .unwrap_or(false)
    }

    const OVERLAY_FADE: f32 = 0.11;

    pub fn overlay_animating(&self) -> bool {
        self.overlay_shown
            && self
                .overlay_since
                .map(|t| t.elapsed().as_secs_f32() < Self::OVERLAY_FADE)
                .unwrap_or(false)
    }

    pub fn set_status(&mut self, git: Option<String>, clock: String, sessions: usize) {
        self.status_git = git;
        self.status_clock = clock;
        self.status_sessions = sessions;
    }

    pub fn status_clock(&self) -> &str {
        &self.status_clock
    }

    pub fn set_palette(&mut self, p: Option<PaletteView>) {
        self.palette_view = p;
    }

    pub fn set_pane_menu(&mut self, m: Option<PaneMenuView>) {
        self.pane_menu_view = m;
    }

    /// clamped (x, y, width, row_h, pad) of the pane context menu, shared by the
    /// renderer and the hit-test so the two never drift
    fn pane_menu_geom(&self, mx: f32, my: f32) -> (f32, f32, f32, f32, f32) {
        let s = self.scale;
        let chrome_h = self.atlas.metrics(FontId::Chrome).cell_h;
        let row_h = chrome_h + 10.0 * s;
        let pad = 8.0 * s;
        let mw = (172.0 * s).round();
        let mh = row_h * PANE_MENU_ITEMS.len() as f32 + pad * 2.0;
        let w = self.config.width as f32;
        let h = self.config.height as f32;
        let bx = mx.min(w - mw - 4.0 * s).max(0.0).round();
        let by = my.min(h - mh - 4.0 * s).max(self.title_bar_h).round();
        (bx, by, mw, row_h, pad)
    }

    /// the menu item under (px, py), or None if outside the menu's rows
    pub fn pane_menu_item_at(&self, px: f32, py: f32) -> Option<usize> {
        let v = self.pane_menu_view.as_ref()?;
        let (bx, by, mw, row_h, pad) = self.pane_menu_geom(v.x, v.y);
        let rows = PANE_MENU_ITEMS.len() as f32;
        if px < bx || px >= bx + mw || py < by + pad || py >= by + pad + row_h * rows {
            return None;
        }
        Some((((py - by - pad) / row_h) as usize).min(PANE_MENU_ITEMS.len() - 1))
    }

    pub fn set_market(&mut self, m: Option<MarketView>) {
        self.market_view = m;
    }

    pub fn set_find(&mut self, f: Option<FindView>) {
        self.find_view = f;
    }

    pub fn set_confirm(&mut self, c: Option<ConfirmView>) {
        self.confirm_view = c;
    }

    pub fn set_rename(&mut self, r: Option<RenameView>) {
        self.rename_view = r;
    }

    fn chrome_track(&self) -> f32 {
        (0.06 * self.atlas.metrics(FontId::Chrome).cell_w).max(0.5)
    }

    /// title-bar buttons, left→right: splitV, splitH, gear, minimize, maximize, close
    fn control_rects(&self) -> [(Hot, f32, f32); 7] {
        let cw = (46.0 * self.scale).round();
        let w = self.config.width as f32;
        [
            (Hot::SplitV, w - cw * 7.0, w - cw * 6.0),
            (Hot::SplitH, w - cw * 6.0, w - cw * 5.0),
            (Hot::PaneMode, w - cw * 5.0, w - cw * 4.0),
            (Hot::Gear, w - cw * 4.0, w - cw * 3.0),
            (Hot::Minimize, w - cw * 3.0, w - cw * 2.0),
            (Hot::Maximize, w - cw * 2.0, w - cw),
            (Hot::Close, w - cw, w),
        ]
    }

    /// where the wordmark ends and tabs begin
    fn tabs_start_x(&self) -> f32 {
        let m = self.atlas.metrics(FontId::Chrome);
        let s = self.scale;
        self.pad
            + m.cell_w                                       // logo mark
            + (10.0 * s)
            + self.text_w(FontId::Chrome, "termie", self.chrome_track())
            + (18.0 * s)
    }

    fn tab_layout(&self) -> TabLayout {
        let s = self.scale;
        let h = self.title_bar_h;
        let cw = (46.0 * s).round();
        let newtab_w = (40.0 * s).round();
        let start = self.tabs_start_x();
        // reserve all 7 title-bar control buttons (control_rects starts at w-7cw,
        // the SplitV slot) so the new-tab '+' never overruns the split icon
        let controls_start = self.config.width as f32 - cw * 7.0;
        let avail = (controls_start - start - newtab_w - 4.0 * s).max(0.0);
        let n = self.tabs.len();

        let mut tabs = Vec::new();
        let tab_w = if n == 0 {
            0.0
        } else {
            (avail / n as f32).min(200.0 * s).max(54.0 * s)
        };
        for i in 0..n {
            let x = start + i as f32 * tab_w;
            let rect = (x, 0.0, tab_w, h);
            let cc = (18.0 * s).round();
            let close = (x + tab_w - cc - 6.0 * s, (h - cc) / 2.0, cc, cc);
            tabs.push((i, rect, close));
        }
        let newtab_x = start + n as f32 * tab_w + 4.0 * s;
        TabLayout {
            tabs,
            newtab: (newtab_x, 0.0, newtab_w, h),
        }
    }

    /// single-column geometry for the slide-in settings panel. body baselines
    /// are absolute and scroll-adjusted, so `build_settings` and `hit_test`
    /// share them; the body is clipped to [body_top, body_bottom] when drawn
    fn settings_geom(&self) -> SettingsGeom {
        let s = self.scale;
        let w = self.config.width as f32;
        let h = self.config.height as f32;
        let scroll = self.settings_scroll;

        let panel_w = (0.6 * w).clamp(460.0 * s, 760.0 * s).min((w - 160.0 * s).max(360.0 * s));
        let panel_top = self.title_bar_h;
        let panel_h = (h - self.title_bar_h - self.status_bar_h).max(1.0);
        let panel_x = (w - panel_w * self.settings_p).round();

        let pad = 22.0 * s;
        let content_x = panel_x + pad;
        let content_w = panel_w - pad * 2.0;
        let val_x = content_x + 152.0 * s;
        let bw = (30.0 * s).round();
        let bh = (26.0 * s).round();
        let val_w = (60.0 * s).round();
        let cluster = bw * 2.0 + val_w;

        let head_y = panel_top + 22.0 * s;
        let close_sz = 26.0 * s;
        let close_btn = (panel_x + panel_w - pad - close_sz, head_y - 3.0 * s, close_sz, close_sz);
        let body_top = panel_top + 60.0 * s;
        let body_bottom = panel_top + panel_h - 14.0 * s;

        // body laid out in local space (y from 0), converted to absolute below
        let row = 38.0 * s;
        let lh = 30.0 * s;
        let sec_gap = 24.0 * s;
        let hdr_adv = 32.0 * s;
        let chip_h = 42.0 * s;
        let key_row = 22.0 * s;
        let mut y = 0.0;
        // PLUGINS first so the marketplace is the first thing in the gear menu
        let sec_plugins = y;
        y += hdr_adv;
        let plugins_n = self.plugins_installed.len();
        let plugin_first_l = y;
        y += if plugins_n == 0 { row } else { row * plugins_n as f32 };
        y += sec_gap;
        let sec_app = y;
        y += hdr_adv;
        let font_l = y;
        y += row;
        let fontfam_l = y;
        y += row;
        let pad_l = y;
        y += row;
        let cursor_l = y;
        y += row;
        let blink_l = y;
        y += row;
        let opacity_l = y;
        y += row;
        let theme_label_l = y;
        y += lh;
        let theme_chip_l = y;
        y += chip_h + 10.0 * s;
        y += sec_gap;
        let sec_beh = y;
        y += hdr_adv;
        let scrollback_l = y;
        y += row;
        let copysel_l = y;
        y += row;
        y += sec_gap;
        let sec_shell = y;
        y += hdr_adv;
        let shell_l = y;
        y += row;
        let profile_l = y;
        y += row;
        let close_l = y;
        y += row;
        let backend_l = y;
        y += row;
        y += sec_gap;
        let sec_keys = y;
        y += hdr_adv;
        let keys_start_l = y;
        y += key_row * 6.0;
        y += sec_gap;
        let sec_about = y;
        y += hdr_adv;
        let about_start_l = y;
        y += 28.0 * s * 3.0;
        let content_h = y + 12.0 * s;

        let ay = |yl: f32| body_top - scroll + yl;
        let stepper = |x: f32, yl: f32| {
            let yb = ay(yl);
            ((x, yb, bw, bh), (x + bw + val_w, yb, bw, bh))
        };
        let (font_dec, font_inc) = stepper(val_x, font_l);
        let (pad_dec, pad_inc) = stepper(val_x, pad_l);
        let (op_dec, op_inc) = stepper(val_x, opacity_l);
        let (sb_dec, sb_inc) = stepper(val_x, scrollback_l);
        let fontfam_btn = (val_x, ay(fontfam_l), cluster, bh);
        let cursor_btn = (val_x, ay(cursor_l), cluster, bh);
        let blink_btn = (val_x, ay(blink_l), cluster, bh);
        let copysel_btn = (val_x, ay(copysel_l), cluster, bh);
        let shell_btn = (val_x, ay(shell_l), cluster, bh);
        let profile_btn = (val_x, ay(profile_l), cluster, bh);
        let close_action_btn = (val_x, ay(close_l), cluster, bh);
        let backend_btn = (val_x, ay(backend_l), cluster, bh);
        // "browse" store button right-aligned in the PLUGINS header row
        let plugins_btn = (content_x + content_w - cluster, ay(sec_plugins) - 4.0 * s, cluster, bh);
        // one row per installed plugin: (name, enabled, toggle rect, row baseline)
        let plugin_rows: Vec<(String, bool, Rect, f32)> = self
            .plugins_installed
            .iter()
            .enumerate()
            .map(|(i, (name, on))| {
                let ry = ay(plugin_first_l + i as f32 * row);
                (name.clone(), *on, (val_x, ry, cluster, bh), ry)
            })
            .collect();

        let chip_gap = 8.0 * s;
        let chip_w = ((content_w - chip_gap * 2.0) / 3.0).floor();
        let chip_y = ay(theme_chip_l);
        let theme_chips = [
            (content_x, chip_y, chip_w, chip_h),
            (content_x + chip_w + chip_gap, chip_y, chip_w, chip_h),
            (content_x + (chip_w + chip_gap) * 2.0, chip_y, chip_w, chip_h),
        ];

        let mut controls = vec![
            (Hot::FontDec, font_dec),
            (Hot::FontInc, font_inc),
            (Hot::FontCycle, fontfam_btn),
            (Hot::PadDec, pad_dec),
            (Hot::PadInc, pad_inc),
            (Hot::OpacityDec, op_dec),
            (Hot::OpacityInc, op_inc),
            (Hot::CursorCycle, cursor_btn),
            (Hot::CursorBlink, blink_btn),
            (Hot::ThemeSet(ThemeId::Instrument), theme_chips[0]),
            (Hot::ThemeSet(ThemeId::Koi), theme_chips[1]),
            (Hot::ThemeSet(ThemeId::Paper), theme_chips[2]),
            (Hot::ScrollbackDec, sb_dec),
            (Hot::ScrollbackInc, sb_inc),
            (Hot::CopyOnSelect, copysel_btn),
            (Hot::ShellCycle, shell_btn),
            (Hot::LoadProfile, profile_btn),
            (Hot::CloseActionCycle, close_action_btn),
            (Hot::BackendCycle, backend_btn),
            (Hot::OpenPlugins, plugins_btn),
        ];
        for (i, (_, _, rect, _)) in plugin_rows.iter().enumerate() {
            controls.push((Hot::PluginToggle(i), *rect));
        }

        SettingsGeom {
            panel_x,
            panel_w,
            panel_top,
            panel_h,
            body_top,
            body_bottom,
            content_h,
            content_x,
            content_w,
            bh,
            val_w,
            head_y,
            close_btn,
            fontfam_y: ay(fontfam_l),
            fontfam_btn,
            sec_app_y: ay(sec_app),
            sec_beh_y: ay(sec_beh),
            sec_shell_y: ay(sec_shell),
            sec_plugins_y: ay(sec_plugins),
            sec_keys_y: ay(sec_keys),
            sec_about_y: ay(sec_about),
            font_y: ay(font_l),
            pad_y: ay(pad_l),
            opacity_y: ay(opacity_l),
            cursor_y: ay(cursor_l),
            blink_y: ay(blink_l),
            theme_label_y: ay(theme_label_l),
            scrollback_y: ay(scrollback_l),
            copysel_y: ay(copysel_l),
            shell_y: ay(shell_l),
            profile_y: ay(profile_l),
            close_y: ay(close_l),
            backend_y: ay(backend_l),
            plugin_rows,
            keys_start_y: ay(keys_start_l),
            about_start_y: ay(about_start_l),
            font_dec,
            font_inc,
            pad_dec,
            pad_inc,
            op_dec,
            op_inc,
            cursor_btn,
            blink_btn,
            theme_chips,
            sb_dec,
            sb_inc,
            copysel_btn,
            shell_btn,
            profile_btn,
            close_action_btn,
            backend_btn,
            plugins_btn,
            controls,
        }
    }

    pub fn hit_test(&self, x: f32, y: f32) -> Hit {
        // the open settings panel takes priority over the chrome beneath it
        if self.settings_open && self.settings_p > 0.99 {
            let g = self.settings_geom();
            if in_rect(x, y, g.close_btn) {
                return Hit::Button(Hot::PanelClose);
            }
            // body controls are only hittable within the scroll viewport
            if y >= g.body_top && y < g.body_bottom {
                for (hot, rect) in g.controls {
                    if in_rect(x, y, rect) {
                        return Hit::Button(hot);
                    }
                }
            }
        }

        let w = self.config.width as f32;
        let h = self.config.height as f32;

        // chrome buttons sit flush against the top/corner resize border, so they
        // must win over it — otherwise clicking the top-right X (or the top edge
        // of any control) grabs a resize handle instead of closing the window
        if y < self.title_bar_h {
            for (c, x0, x1) in self.control_rects() {
                if x >= x0 && x < x1 {
                    return Hit::Button(c);
                }
            }
            let tl = self.tab_layout();
            if in_rect(x, y, tl.newtab) {
                return Hit::Button(Hot::NewTab);
            }
            for (i, rect, close) in &tl.tabs {
                if in_rect(x, y, *close) {
                    return Hit::Button(Hot::TabClose(*i));
                }
                if in_rect(x, y, *rect) {
                    return Hit::Button(Hot::Tab(*i));
                }
            }
        }

        let e = (6.0 * self.scale).max(4.0);
        let left = x <= e;
        let right = x >= w - e;
        let top = y <= e;
        let bottom = y >= h - e;
        let dir = match (top, bottom, left, right) {
            (true, _, true, _) => Some(ResizeDirection::NorthWest),
            (true, _, _, true) => Some(ResizeDirection::NorthEast),
            (_, true, true, _) => Some(ResizeDirection::SouthWest),
            (_, true, _, true) => Some(ResizeDirection::SouthEast),
            (true, _, _, _) => Some(ResizeDirection::North),
            (_, true, _, _) => Some(ResizeDirection::South),
            (_, _, true, _) => Some(ResizeDirection::West),
            (_, _, _, true) => Some(ResizeDirection::East),
            _ => None,
        };
        if let Some(d) = dir {
            return Hit::Resize(d);
        }

        if y < self.title_bar_h {
            return Hit::TitleBar;
        }
        Hit::Content
    }

    fn upload_atlas(&mut self) {
        // the atlas grew (1024 -> 2048): recreate the gpu textures + bind group
        // at the new dim before uploading. repack_at already flagged a full
        // re-upload, so the bands below repopulate the fresh textures this call
        if self.atlas.dim != self.atlas_gpu_dim {
            let (at, ct, bg) = make_atlas_bind_group(
                &self.device,
                self.atlas.dim,
                &self.sampler,
                &self.icon_view,
                &self.icon_sampler,
                &self.color_sampler,
            );
            self.atlas_texture = at;
            self.color_texture = ct;
            self.atlas_bind_group = bg;
            self.atlas_gpu_dim = self.atlas.dim;
        }
        let dim = self.atlas.dim;
        // upload only the row band that changed; a freshly repacked atlas has no
        // band and uploads in full. width is the full atlas width (R8, so
        // bytes_per_row == dim, already 256-aligned for dim=1024)
        if self.atlas.dirty {
            let (y0, y1) = self.atlas.dirty_y.unwrap_or((0, dim));
            let (y0, y1) = (y0.min(dim), y1.min(dim));
            if y1 > y0 {
                let off = (y0 * dim) as usize;
                let end = (y1 * dim) as usize;
                self.queue.write_texture(
                    wgpu::TexelCopyTextureInfo {
                        texture: &self.atlas_texture,
                        mip_level: 0,
                        origin: wgpu::Origin3d { x: 0, y: y0, z: 0 },
                        aspect: wgpu::TextureAspect::All,
                    },
                    &self.atlas.data[off..end],
                    wgpu::TexelCopyBufferLayout {
                        offset: 0,
                        bytes_per_row: Some(dim),
                        rows_per_image: Some(y1 - y0),
                    },
                    wgpu::Extent3d {
                        width: dim,
                        height: y1 - y0,
                        depth_or_array_layers: 1,
                    },
                );
            }
            self.atlas.dirty = false;
            self.atlas.dirty_y = None;
        }
        // the color (emoji) atlas uploads independently; RGBA so bytes_per_row
        // is dim*4 (16384 for dim=1024, still 256-aligned)
        if self.atlas.color_dirty {
            let (y0, y1) = self.atlas.color_dirty_y.unwrap_or((0, dim));
            let (y0, y1) = (y0.min(dim), y1.min(dim));
            if y1 > y0 {
                let off = (y0 * dim * 4) as usize;
                let end = (y1 * dim * 4) as usize;
                self.queue.write_texture(
                    wgpu::TexelCopyTextureInfo {
                        texture: &self.color_texture,
                        mip_level: 0,
                        origin: wgpu::Origin3d { x: 0, y: y0, z: 0 },
                        aspect: wgpu::TextureAspect::All,
                    },
                    &self.atlas.color_data[off..end],
                    wgpu::TexelCopyBufferLayout {
                        offset: 0,
                        bytes_per_row: Some(dim * 4),
                        rows_per_image: Some(y1 - y0),
                    },
                    wgpu::Extent3d {
                        width: dim,
                        height: y1 - y0,
                        depth_or_array_layers: 1,
                    },
                );
            }
            self.atlas.color_dirty = false;
            self.atlas.color_dirty_y = None;
        }
    }

    fn push_rect(out: &mut Vec<Instance>, x: f32, y: f32, w: f32, h: f32, rgb: Rgb, alpha: f32) {
        let lin = rgb.to_linear_f32();
        out.push(Instance {
            pos: [x, y],
            size: [w, h],
            uv_min: [0.0, 0.0],
            uv_max: [0.0, 0.0],
            color: [lin[0], lin[1], lin[2], alpha],
            kind: 0,
            _pad: [0; 3],
        });
    }

    /// draw the full-color app-icon badge (kind 2) at (x,y), size x size px;
    /// `alpha` fades it for the startup reveal
    fn push_icon(out: &mut Vec<Instance>, x: f32, y: f32, size: f32, alpha: f32) {
        out.push(Instance {
            pos: [x, y],
            size: [size, size],
            uv_min: [0.0, 0.0],
            uv_max: [1.0, 1.0],
            color: [1.0, 1.0, 1.0, alpha],
            kind: 2,
            _pad: [0; 3],
        });
    }

    /// banded vertical gradient from `top` (at y) to `bottom` (at y+h)
    // a flat geometry helper; bundling the rect+colors into a struct would only
    // obscure the call sites
    #[allow(clippy::too_many_arguments)]
    fn push_vgradient(out: &mut Vec<Instance>, x: f32, y: f32, w: f32, h: f32, top: Rgb, bottom: Rgb, bands: usize) {
        let n = bands.max(1);
        let band_h = h / n as f32;
        let lerp = |a: u8, b: u8, t: f32| (a as f32 + (b as f32 - a as f32) * t).round() as u8;
        for i in 0..n {
            let t = (i as f32 + 0.5) / n as f32;
            let c = Rgb::new(lerp(top.r, bottom.r, t), lerp(top.g, bottom.g, t), lerp(top.b, bottom.b, t));
            // overlap by 1px so seams never show
            Self::push_rect(out, x, (y + i as f32 * band_h).floor(), w, band_h.ceil() + 1.0, c, 1.0);
        }
    }

    /// 1px outline around a rect
    fn stroke_rect(out: &mut Vec<Instance>, r: (f32, f32, f32, f32), t: f32, c: Rgb) {
        Self::stroke_rect_a(out, r, t, c, 1.0);
    }

    /// outline around a rect at a given opacity
    fn stroke_rect_a(out: &mut Vec<Instance>, r: (f32, f32, f32, f32), t: f32, c: Rgb, a: f32) {
        let (x, y, w, h) = r;
        Self::push_rect(out, x, y, w, t, c, a);
        Self::push_rect(out, x, y + h - t, w, t, c, a);
        Self::push_rect(out, x, y, t, h, c, a);
        Self::push_rect(out, x + w - t, y, t, h, c, a);
    }

    /// procedurally render a box-drawing / block char filling the exact cell so
    /// it tiles seamlessly with neighbours. returns false if `c` isn't one we draw
    fn draw_box(out: &mut Vec<Instance>, x: f32, y: f32, w: f32, h: f32, c: char, col: Rgb) -> bool {
        // strokes toward (left, right, up, down): 0 none, 1 light, 2 heavy, 3 double
        let (l, r, u, d): (u8, u8, u8, u8) = match c {
            '\u{2500}' => (1, 1, 0, 0),
            '\u{2501}' => (2, 2, 0, 0),
            '\u{2502}' => (0, 0, 1, 1),
            '\u{2503}' => (0, 0, 2, 2),
            '\u{250C}' | '\u{256D}' => (0, 1, 0, 1),
            '\u{2510}' | '\u{256E}' => (1, 0, 0, 1),
            '\u{2514}' | '\u{2570}' => (0, 1, 1, 0),
            '\u{2518}' | '\u{256F}' => (1, 0, 1, 0),
            '\u{251C}' => (0, 1, 1, 1),
            '\u{2524}' => (1, 0, 1, 1),
            '\u{252C}' => (1, 1, 0, 1),
            '\u{2534}' => (1, 1, 1, 0),
            '\u{253C}' => (1, 1, 1, 1),
            '\u{2550}' => (3, 3, 0, 0),
            '\u{2551}' => (0, 0, 3, 3),
            '\u{2554}' => (0, 3, 0, 3),
            '\u{2557}' => (3, 0, 0, 3),
            '\u{255A}' => (0, 3, 3, 0),
            '\u{255D}' => (3, 0, 3, 0),
            '\u{2560}' => (0, 3, 3, 3),
            '\u{2563}' => (3, 0, 3, 3),
            '\u{2566}' => (3, 3, 0, 3),
            '\u{2569}' => (3, 3, 3, 0),
            '\u{256C}' => (3, 3, 3, 3),
            _ => return Self::draw_block(out, x, y, w, h, c, col),
        };
        let thin = (h * 0.07).round().max(1.0);
        let mx = x + w / 2.0;
        let my = y + h / 2.0;
        let gap = thin + 1.0;
        // spans stay exact (x..mx..x+w, y..my..y+h) so adjacent cells join cleanly
        let hbar = |out: &mut Vec<Instance>, xa: f32, xb: f32, wt: u8| match wt {
            1 => Self::push_rect(out, xa, my - thin / 2.0, xb - xa, thin, col, 1.0),
            2 => Self::push_rect(out, xa, my - thin, xb - xa, thin * 2.0, col, 1.0),
            3 => {
                Self::push_rect(out, xa, my - gap - thin / 2.0, xb - xa, thin, col, 1.0);
                Self::push_rect(out, xa, my + gap - thin / 2.0, xb - xa, thin, col, 1.0);
            }
            _ => {}
        };
        let vbar = |out: &mut Vec<Instance>, ya: f32, yb: f32, wt: u8| match wt {
            1 => Self::push_rect(out, mx - thin / 2.0, ya, thin, yb - ya, col, 1.0),
            2 => Self::push_rect(out, mx - thin, ya, thin * 2.0, yb - ya, col, 1.0),
            3 => {
                Self::push_rect(out, mx - gap - thin / 2.0, ya, thin, yb - ya, col, 1.0);
                Self::push_rect(out, mx + gap - thin / 2.0, ya, thin, yb - ya, col, 1.0);
            }
            _ => {}
        };
        if l > 0 {
            hbar(out, x, mx, l);
        }
        if r > 0 {
            hbar(out, mx, x + w, r);
        }
        if u > 0 {
            vbar(out, y, my, u);
        }
        if d > 0 {
            vbar(out, my, y + h, d);
        }
        true
    }

    /// block elements (U+2580..U+259F): full/half/eighth blocks + shade fills
    fn draw_block(out: &mut Vec<Instance>, x: f32, y: f32, w: f32, h: f32, c: char, col: Rgb) -> bool {
        match c {
            '\u{2588}' => Self::push_rect(out, x, y, w, h, col, 1.0),
            '\u{2580}' => Self::push_rect(out, x, y, w, h / 2.0, col, 1.0), // upper half
            '\u{2584}' => Self::push_rect(out, x, y + h / 2.0, w, h / 2.0, col, 1.0), // lower half
            '\u{258C}' => Self::push_rect(out, x, y, w / 2.0, h, col, 1.0), // left half
            '\u{2590}' => Self::push_rect(out, x + w / 2.0, y, w / 2.0, h, col, 1.0), // right half
            '\u{2591}' => Self::push_rect(out, x, y, w, h, col, 0.25),
            '\u{2592}' => Self::push_rect(out, x, y, w, h, col, 0.5),
            '\u{2593}' => Self::push_rect(out, x, y, w, h, col, 0.75),
            // lower eighths ▁..▇
            '\u{2581}'..='\u{2587}' => {
                let frac = (c as u32 - 0x2580) as f32 / 8.0;
                let bh = h * frac;
                Self::push_rect(out, x, y + h - bh, w, bh, col, 1.0);
            }
            // left eighths ▉..▏ (▌ handled above as left half)
            '\u{2589}'..='\u{258F}' => {
                let frac = (8 - (c as u32 - 0x2588)) as f32 / 8.0;
                Self::push_rect(out, x, y, w * frac, h, col, 1.0);
            }
            '\u{2594}' => Self::push_rect(out, x, y, w, h / 8.0, col, 1.0), // upper eighth
            '\u{2595}' => Self::push_rect(out, x + w * 7.0 / 8.0, y, w / 8.0, h, col, 1.0), // right eighth
            // quadrant blocks (▖▗▘▙▚▛▜▝▞▟): 2x2 sub-cell fills, so low-res block
            // art (e.g. mosaic logos) tiles solid instead of leaving gaps
            '\u{2596}'..='\u{259F}' => {
                let (hw, hh) = (w / 2.0, h / 2.0);
                // bits: 1=upper-left 2=upper-right 4=lower-left 8=lower-right
                let mask: u8 = match c {
                    '\u{2598}' => 0b0001,
                    '\u{259D}' => 0b0010,
                    '\u{2596}' => 0b0100,
                    '\u{2597}' => 0b1000,
                    '\u{2599}' => 0b1101,
                    '\u{259A}' => 0b1001,
                    '\u{259B}' => 0b0111,
                    '\u{259C}' => 0b1011,
                    '\u{259E}' => 0b0110,
                    '\u{259F}' => 0b1110,
                    _ => 0,
                };
                if mask & 1 != 0 { Self::push_rect(out, x, y, hw, hh, col, 1.0); }
                if mask & 2 != 0 { Self::push_rect(out, x + hw, y, hw, hh, col, 1.0); }
                if mask & 4 != 0 { Self::push_rect(out, x, y + hh, hw, hh, col, 1.0); }
                if mask & 8 != 0 { Self::push_rect(out, x + hw, y + hh, hw, hh, col, 1.0); }
            }
            _ => return false,
        }
        true
    }

    /// draw one terminal grid at a pixel origin
    #[allow(clippy::too_many_arguments)]
    fn draw_grid(
        atlas: &mut GlyphAtlas,
        palette: &Palette,
        out: &mut Vec<Instance>,
        term: &Terminal,
        ox: f32,
        oy: f32,
        focused: bool,
        blink_on: bool,
        beam_w: f32,
        style: CursorShape,
        sel: Option<((usize, usize), (usize, usize))>,
        link: Option<(usize, usize, usize)>,
        matches: &[(usize, usize, usize, bool)],
        bold_as_bright: bool,
    ) {
        let sel_col = palette.sel;
        let m = atlas.metrics(FontId::Content);
        let (cell_w, cell_h, ascent) = (m.cell_w, m.cell_h, m.ascent);
        let grid = &term.grid;
        let scrolled = grid.view_offset > 0;
        let cur = grid.cursor;
        let show_cursor = cur.visible && !scrolled;
        let (crow, ccol) = (cur.row, cur.col.min(grid.cols.saturating_sub(1)));
        let sel_norm = sel.map(|(a, b)| if a <= b { (a, b) } else { (b, a) });

        // find-match highlights drawn beneath glyphs; current match is brighter
        for &(mr, mc, mlen, cur) in matches {
            let (col, alpha) = if cur { (palette.cursor, 0.75) } else { (palette.sel, 0.45) };
            for k in 0..mlen {
                let cc = mc + k;
                if cc >= grid.cols {
                    break;
                }
                Self::push_rect(out, ox + cc as f32 * cell_w, oy + mr as f32 * cell_h, cell_w, cell_h, col, alpha);
            }
        }

        for r in 0..grid.rows {
            let line = grid.line_at(r);
            for c in 0..grid.cols {
                let cell = line.get(c).copied().unwrap_or_default();
                if cell.attrs.hidden {
                    continue;
                }
                let (mut fg_c, mut bg_c) = (cell.fg, cell.bg);
                if cell.attrs.inverse {
                    std::mem::swap(&mut fg_c, &mut bg_c);
                }
                let fg_c = Palette::bold_bright(fg_c, cell.attrs.bold, bold_as_bright);
                let mut fg = palette.resolve_fg(fg_c);
                let bg = palette.resolve_bg(bg_c);
                if cell.attrs.dim {
                    fg = Rgb::new(fg.r / 2, fg.g / 2, fg.b / 2);
                }
                // blinking cells hide their glyph + decorations on the off phase
                let blink_hidden = cell.attrs.blink && !blink_on;

                let x = ox + c as f32 * cell_w;
                let y = oy + r as f32 * cell_h;
                let is_cursor = show_cursor && r == crow && c == ccol;
                let selected = sel_norm
                    .map(|(s, e)| (r, c) >= s && (r, c) <= e)
                    .unwrap_or(false);

                if bg != palette.bg {
                    Self::push_rect(out, x, y, cell_w, cell_h, bg, 1.0);
                }
                if selected {
                    Self::push_rect(out, x, y, cell_w, cell_h, sel_col, 0.9);
                }
                if is_cursor {
                    let alpha = if focused {
                        if blink_on {
                            1.0
                        } else {
                            0.0
                        }
                    } else {
                        0.4
                    };
                    // an app's DECSCUSR shape overrides the configured default
                    let shape = if cur.shape_set { cur.shape } else { style };
                    if alpha > 0.0 {
                        match shape {
                            CursorShape::Bar => {
                                Self::push_rect(out, x, y, beam_w, cell_h, palette.cursor, alpha);
                            }
                            CursorShape::Underline => {
                                let t = beam_w.max(2.0);
                                Self::push_rect(out, x, y + cell_h - t, cell_w, t, palette.cursor, alpha);
                            }
                            CursorShape::Block => {
                                // cover both halves when sitting on a wide glyph
                                let cw = if line.get(c + 1).map(|n| n.c == '\0').unwrap_or(false) {
                                    cell_w * 2.0
                                } else {
                                    cell_w
                                };
                                Self::push_rect(out, x, y, cw, cell_h, palette.cursor, alpha);
                                if focused {
                                    fg = palette.bg;
                                }
                            }
                        }
                    }
                }
                // underline a hovered (ctrl) url so it reads as clickable
                if link.map(|(lr, a, b)| r == lr && c >= a && c < b).unwrap_or(false) {
                    let t = (cell_h * 0.06).max(1.0);
                    Self::push_rect(out, x, y + cell_h - t, cell_w, t, palette.cursor, 1.0);
                }
                // '\0' marks the second cell of a wide glyph — the lead cell's
                // glyph already covers it, so skip drawing here
                if !blink_hidden && cell.c != ' ' && cell.c != '\0' {
                    // box-drawing / block glyphs are drawn procedurally so they
                    // tile seamlessly (font glyphs leave gaps at cell edges)
                    if Self::draw_box(out, x, y, cell_w, cell_h, cell.c, fg) {
                        // handled
                    } else {
                        let gk = GlyphKey {
                            font: FontId::Content,
                            c: cell.c,
                            bold: cell.attrs.bold,
                            italic: cell.attrs.italic,
                        };
                        // a cell carrying combining marks composites its whole
                        // grapheme cluster; fall back to the base char if that
                        // yields nothing (e.g. an emoji ZWJ cluster)
                        let glyph = if cell.cluster != 0 {
                            let cg = atlas.get_cluster(
                                grid.cluster_str(cell.cluster),
                                cell.attrs.bold,
                                cell.attrs.italic,
                            );
                            if cg.is_some() { cg } else { atlas.get(gk) }
                        } else {
                            atlas.get(gk)
                        };
                        if let Some(g) = glyph {
                            let lin = fg.to_linear_f32();
                            out.push(Instance {
                                pos: [x + g.left, y + ascent - g.top],
                                size: [g.width, g.height],
                                uv_min: g.uv_min,
                                uv_max: g.uv_max,
                                color: [lin[0], lin[1], lin[2], 1.0],
                                kind: if g.color { 3 } else { 1 },
                                _pad: [0; 3],
                            });
                        }
                    }
                }
                // underline / strikethrough decorations, drawn in the cell's fg
                // so they also show on blank underlined cells
                if !blink_hidden {
                    let t = (cell_h * 0.06).max(1.0);
                    underline_rects(cell.attrs.underline, cell_w, cell_h, t, |rx, ry, rw, rh| {
                        Self::push_rect(out, x + rx, y + ry, rw, rh, fg, 1.0);
                    });
                    if cell.attrs.strike {
                        Self::push_rect(out, x, (y + cell_h * 0.5).round(), cell_w, t, fg, 1.0);
                    }
                }
            }
        }

        // scroll position indicator: a thin thumb on the pane's right edge,
        // shown only while scrolled into history, sized + positioned to the
        // viewport's slice of total (scrollback + screen) lines
        if scrolled {
            let total = grid.scrollback.len() + grid.rows;
            if total > grid.rows {
                let track_h = grid.rows as f32 * cell_h;
                let tw = (2.0 * beam_w).max(2.0);
                let track_x = ox + grid.cols as f32 * cell_w - tw;
                let thumb_h = (track_h * grid.rows as f32 / total as f32).max(cell_h);
                // viewport top line within total; (total-rows) = live bottom
                let top_line = (total - grid.rows - grid.view_offset) as f32;
                let thumb_y = oy + (track_h - thumb_h) * (top_line / (total - grid.rows) as f32);
                Self::push_rect(out, track_x, oy, tw, track_h, palette.mute, 0.18);
                Self::push_rect(out, track_x, thumb_y, tw, thumb_h, palette.mute, 0.8);
            }
        }
    }

    /// lay out a monospace string at a pixel baseline with optional tracking;
    /// returns the pen end-x
    // a low-level text helper called ~50 times; a params struct would add noise
    // at every call site without making any of them clearer
    #[allow(clippy::too_many_arguments)]
    fn draw_text(
        atlas: &mut GlyphAtlas,
        out: &mut Vec<Instance>,
        font: FontId,
        mut x: f32,
        y_top: f32,
        text: &str,
        rgb: Rgb,
        alpha: f32,
        track: f32,
    ) -> f32 {
        let m = atlas.metrics(font);
        let lin = rgb.to_linear_f32();
        for c in text.chars() {
            if c != ' '
                && let Some(g) = atlas.get(GlyphKey {
                    font,
                    c,
                    bold: false,
                    italic: false,
                }) {
                    out.push(Instance {
                        pos: [x + g.left, y_top + m.ascent - g.top],
                        size: [g.width, g.height],
                        uv_min: g.uv_min,
                        uv_max: g.uv_max,
                        color: [lin[0], lin[1], lin[2], alpha],
                        kind: if g.color { 3 } else { 1 },
                        _pad: [0; 3],
                    });
                }
            x += m.cell_w + track;
        }
        x
    }

    /// pixel width of a tracked monospace string in the given font
    fn text_w(&self, font: FontId, text: &str, track: f32) -> f32 {
        let m = self.atlas.metrics(font);
        text.chars().count() as f32 * (m.cell_w + track)
    }

    #[allow(non_snake_case)]
    fn build(&mut self, panes: &[PaneView], focused: bool, maximized: bool, focus_ease: f32, bare: bool) -> Vec<Instance> {
        // chrome colors come from the active theme's palette
        let INK_0 = self.palette.ink0;
        let INK_1 = self.palette.ink1;
        let INK_3 = self.palette.ink3;
        let INK_4 = self.palette.ink4;
        let RULE = self.palette.rule;
        let RULE_2 = self.palette.rule2;
        let MUTE = self.palette.mute;
        let TEXT_2 = self.palette.text2;
        let PAPER = self.palette.paper;

        let pad = self.pad;
        let w = self.config.width as f32;
        let h = self.config.height as f32;
        let hair = self.scale.max(1.0);
        let track = (0.06 * self.atlas.metrics(FontId::Chrome).cell_w).max(0.5);
        // slow blink ~1.06s period, on for the first ~600ms
        let blink_on = !self.cursor_blink || (self.start.elapsed().as_millis() % 1060) < 600;
        let beam_w = (2.0 * self.scale).round().max(1.0);

        // reuse the persistent scratch buffer: keeps its capacity across frames
        // so a steady-state paint does no heap allocation for the instance list
        let mut out: Vec<Instance> = std::mem::take(&mut self.scratch);
        out.clear();
        out.reserve(self.cols * self.rows + 256);

        // subtle per-theme vertical wash behind everything (bg → bg2); cached and
        // rebuilt only when the size or theme changes (not every frame)
        let grad_key = (self.config.width, self.config.height, self.theme);
        if self.gradient_key != grad_key || self.gradient_cache.is_empty() {
            self.gradient_cache.clear();
            Self::push_vgradient(&mut self.gradient_cache, 0.0, 0.0, w, h, self.palette.bg, self.palette.bg2, 48);
            self.gradient_key = grad_key;
        }
        out.extend_from_slice(&self.gradient_cache);

        // pre-resolve pane grid origins (immutable self) before borrowing the
        // atlas; reuse a persistent buffer like scratch to avoid a per-frame alloc
        let mut pane_info = std::mem::take(&mut self.pane_scratch);
        pane_info.clear();
        pane_info.extend(panes.iter().map(|p| {
            let (ox, oy, _, _) = self.pane_metrics(p.rect);
            (ox, oy, p.focused, p.rect)
        }));

        // ---- terminal content (one grid per pane) ----
        let cursor_style = self.cursor_style;
        let bold_as_bright = self.bold_as_bright;
        {
            let palette = &self.palette;
            let atlas = &mut self.atlas;
            let find_view = self.find_view.as_ref();
            for (pv, info) in panes.iter().zip(&pane_info) {
                let fmatches: &[(usize, usize, usize, bool)] = if pv.focused {
                    find_view.map(|f| f.matches.as_slice()).unwrap_or(&[])
                } else {
                    &[]
                };
                Self::draw_grid(
                    atlas,
                    palette,
                    &mut out,
                    pv.term,
                    info.0,
                    info.1,
                    pv.focused && focused,
                    blink_on,
                    beam_w,
                    cursor_style,
                    pv.sel,
                    pv.link,
                    fmatches,
                    bold_as_bright,
                );
            }
        }

        // dividers + focus border (only meaningful with more than one pane).
        // the focused pane gets a dim PAPER-accent outline so it reads at a
        // glance across a cockpit of panes; thinner than the bell flash
        // (hair vs hair*2) so the two never read as the same signal
        if panes.len() > 1 {
            for (_, _, _, rect) in &pane_info {
                Self::stroke_rect(&mut out, *rect, hair, RULE);
            }
            for (_, _, foc, rect) in &pane_info {
                if *foc {
                    // ease the accent border in on focus change (1.0 once settled)
                    Self::stroke_rect_a(&mut out, *rect, hair, PAPER, 0.55 * focus_ease);
                }
            }
        }
        // bell flash: accent border on any pane that just rang (even single
        // pane), its opacity eased out by the caller so it fades rather than snaps
        for (pv, info) in panes.iter().zip(&pane_info) {
            if pv.flash > 0.0 {
                Self::stroke_rect_a(&mut out, info.3, hair * 2.0, PAPER, pv.flash);
            }
        }
        // last use of pane_info — hand the buffer back so its capacity persists
        self.pane_scratch = pane_info;

        // a torn-off satellite window renders just its pane (the OS supplies the
        // title bar / close / move), so skip all the chrome and overlays below
        if bare {
            return out;
        }

        // ---- plugin dock (Tier-1 widgets) on the right of the content area ----
        if !self.dock.is_empty() {
            self.draw_dock(&mut out, track);
        }

        let chrome_h = self.atlas.metrics(FontId::Chrome).cell_h;
        let cw_c = self.atlas.metrics(FontId::Chrome).cell_w;
        let text_top = ((self.title_bar_h - chrome_h) / 2.0).round();

        // ---- title bar (flat opaque instrument) ----
        Self::push_rect(&mut out, 0.0, 0.0, w, self.title_bar_h, INK_1, 1.0);
        // two-tone trim under the bar: a brighter seam over a darker shadow line
        // reads as a machined edge between chrome and content (instrument depth)
        Self::push_rect(&mut out, 0.0, self.title_bar_h - hair * 2.0, w, hair, RULE_2, 1.0);
        Self::push_rect(&mut out, 0.0, self.title_bar_h - hair, w, hair, RULE, 1.0);

        // app-icon badge (the ">_<" mark) + wordmark
        let badge = (self.title_bar_h * 0.6).round();
        let badge_y = ((self.title_bar_h - badge) / 2.0).round();
        Self::push_icon(&mut out, pad, badge_y, badge, 1.0);
        let wx = pad + badge + (9.0 * self.scale).round();
        let _ = Self::draw_text(
            &mut self.atlas, &mut out, FontId::Chrome, wx, text_top, "termie", TEXT_2, 1.0, track,
        );

        // tabs — snapshot to owned data so the atlas can be borrowed mutably
        let tl = self.tab_layout();
        let active_tab = self.active_tab;
        let tab_items: Vec<TabItem> =
            tl.tabs
                .iter()
                .map(|(i, rect, close)| {
                    (
                        *i,
                        *rect,
                        *close,
                        self.tabs.get(*i).cloned().unwrap_or_default(),
                        *i == active_tab,
                        self.hovered == Some(Hot::Tab(*i)),
                        self.hovered == Some(Hot::TabClose(*i)),
                    )
                })
                .collect();
        let newtab_rect = tl.newtab;
        let newtab_hover = self.hovered == Some(Hot::NewTab);
        let he = self.hover_ease();

        for (_, rect, close, label, active, hov, close_hov) in &tab_items {
            let (tx, _ty, tw, _th) = *rect;
            if *active {
                Self::push_rect(&mut out, tx, hair, tw, self.title_bar_h - hair * 2.0, INK_4, 1.0);
                // the accent underline is drawn after the loop so it can slide
            } else if *hov {
                Self::push_rect(&mut out, tx, hair, tw, self.title_bar_h - hair * 2.0, INK_3, he);
            }
            Self::push_rect(&mut out, tx, hair, hair, self.title_bar_h - hair * 2.0, RULE, 1.0);

            // truncate the label to the space before the close icon
            let avail = (close.0 - (tx + 10.0 * self.scale)).max(0.0);
            let maxc = (avail / cw_c).floor().max(0.0) as usize;
            // borrow the already-owned snapshot label; only allocate when the
            // label actually needs truncating (the common case fits as-is)
            let truncated;
            let lab: &str = if label.chars().count() > maxc && maxc > 1 {
                truncated = label.chars().take(maxc.saturating_sub(1)).collect::<String>() + "\u{2026}";
                &truncated
            } else {
                label
            };
            let lc = if *active { TEXT_2 } else { MUTE };
            let _ = Self::draw_text(
                &mut self.atlas, &mut out, FontId::Chrome, tx + 10.0 * self.scale, text_top, lab, lc, 1.0, track,
            );
            // close icon (nerd-font times), easing brighter on hover
            let (cx, cy, ccw, _cch) = *close;
            let cbase = if *active { MUTE } else { RULE_2 };
            let cc = if *close_hov { cbase.lerp(PAPER, he) } else { cbase };
            let cgx = (cx + (ccw - cw_c) / 2.0).round();
            let _ = Self::draw_text(
                &mut self.atlas, &mut out, FontId::Chrome, cgx, cy.round(), "\u{f00d}", cc, 1.0, track,
            );
        }

        // active-tab accent rail: slides from the old tab to the new on a switch,
        // otherwise sits on the active tab
        if let Some(item) = tab_items.iter().find(|t| t.0 == active_tab) {
            let (ax, _, aw, _) = item.1;
            let (ux, uw) = match self
                .tab_slide_p()
                .and_then(|(old, e)| tab_items.iter().find(|t| t.0 == old).map(|o| (o.1, e)))
            {
                Some(((ox, _, ow, _), e)) => (ox + (ax - ox) * e, ow + (aw - ow) * e),
                None => (ax, aw),
            };
            Self::push_rect(&mut out, ux, self.title_bar_h - hair * 2.0, uw, hair * 2.0, PAPER, 1.0);
        }

        // new-tab button (nerd-font plus)
        {
            let (nx, _ny, nw, _nh) = newtab_rect;
            if newtab_hover {
                Self::push_rect(&mut out, nx, hair, nw, self.title_bar_h - hair * 2.0, INK_3, he);
            }
            let ngx = (nx + (nw - cw_c) / 2.0).round();
            let ncol = if newtab_hover { TEXT_2 } else { MUTE };
            let _ = Self::draw_text(
                &mut self.atlas, &mut out, FontId::Chrome, ngx, text_top, "\u{f067}", ncol, 1.0, track,
            );
        }

        // title-bar buttons: split right|left, split top/bottom, gear, min, max, close.
        // the split icons are drawn procedurally below (like pane-mode), not glyphs
        let glyphs = [
            (Hot::SplitV, ""),
            (Hot::SplitH, ""),
            (Hot::PaneMode, ""),
            (Hot::Gear, "\u{f013}"),
            (Hot::Minimize, "\u{f2d1}"),
            (Hot::Maximize, if maximized { "\u{f2d2}" } else { "\u{f2d0}" }),
            (Hot::Close, "\u{f00d}"),
        ];
        for ((c, x0, x1), (_, glyph)) in self.control_rects().into_iter().zip(glyphs) {
            Self::push_rect(&mut out, x0, hair, hair, self.title_bar_h - hair * 2.0, RULE, 1.0);
            let is_hover = self.hovered == Some(c);
            let active = is_hover
                || (c == Hot::Gear && self.settings_open)
                || (c == Hot::PaneMode && self.pane_mode);
            if active {
                // fade the hover fill in; a settings-pinned gear stays at full
                let ha = if is_hover { he } else { 1.0 };
                let hc = if c == Hot::Close { PAPER } else { INK_4 };
                Self::push_rect(&mut out, x0, hair, x1 - x0, self.title_bar_h - hair * 2.0, hc, ha);
            }
            let gx = (x0 + (x1 - x0 - cw_c) / 2.0).round();
            let color = if active && c == Hot::Close {
                INK_0
            } else if active {
                TEXT_2
            } else {
                MUTE
            };
            let _ = Self::draw_text(
                &mut self.atlas, &mut out, FontId::Chrome, gx, text_top, glyph, color, 1.0, track,
            );
            // a clean "split view" mark à la Konsole: a landscape window frame
            // divided into two panes, one pane tinted so it reads as a split
            // view at a glance. no plus, no clutter (new-tab "+" is its own button)
            if c == Hot::SplitV || c == Hot::SplitH {
                let s = self.scale;
                let th = hair;
                let iw = (16.0 * s).round();
                let ih = (12.0 * s).round();
                let bx = ((x0 + x1) / 2.0 - iw / 2.0).round();
                let by = (self.title_bar_h / 2.0 - ih / 2.0).round();
                if c == Hot::SplitV {
                    let half = (iw / 2.0).round();
                    Self::push_rect(&mut out, bx, by, half, ih, color, 0.5);
                    Self::push_rect(&mut out, (bx + half - th / 2.0).round(), by, th, ih, color, 1.0);
                } else {
                    let half = (ih / 2.0).round();
                    Self::push_rect(&mut out, bx, by, iw, half, color, 0.5);
                    Self::push_rect(&mut out, bx, (by + half - th / 2.0).round(), iw, th, color, 1.0);
                }
                Self::stroke_rect(&mut out, (bx, by, iw, ih), th, color);
            }
            // pane-mode toggle: a 2x2 grid of panes, lit while the mode is active
            if c == Hot::PaneMode {
                let sz = (10.0 * self.scale).round();
                let gap = (2.0 * self.scale).max(1.0);
                let cell = ((sz - gap) / 2.0).max(1.0);
                let gx0 = ((x0 + x1) / 2.0 - sz / 2.0).round();
                let gy0 = (self.title_bar_h / 2.0 - sz / 2.0).round();
                for (dx, dy) in [(0.0, 0.0), (1.0, 0.0), (0.0, 1.0), (1.0, 1.0)] {
                    Self::push_rect(&mut out, gx0 + dx * (cell + gap), gy0 + dy * (cell + gap), cell, cell, color, 1.0);
                }
            }
        }

        // ---- status bar (flat) ----
        let sb_y = h - self.status_bar_h;
        Self::push_rect(&mut out, 0.0, sb_y, w, self.status_bar_h, INK_0, 1.0);
        Self::push_rect(&mut out, 0.0, sb_y, w, hair, RULE, 1.0);
        let st_top = (sb_y + (self.status_bar_h - chrome_h) / 2.0).round();
        let wide = (0.14 * cw_c).max(1.0);

        let mut sx = pad;
        let gap = (14.0 * self.scale).round();
        let scale = self.scale;

        // rebuild the cached number strings only when they actually change, so
        // the steady-state paint reformats nothing
        if self.status_size.0 != self.cols || self.status_size.1 != self.rows {
            self.status_size = (self.cols, self.rows, format!("{}\u{00d7}{}", self.cols, self.rows));
        }
        if self.status_tabs.0 != self.status_sessions {
            self.status_tabs = (self.status_sessions, self.status_sessions.to_string());
        }

        // left cluster: SIZE · ENC · GIT · TABS — seg borrows only the atlas, so
        // the status strings can be passed by reference without per-frame clones
        sx = Self::seg(&mut self.atlas, &mut out, sx, st_top, "SIZE", &self.status_size.2, track, wide, scale, RULE_2, TEXT_2);
        sx += gap;
        sx = Self::seg(&mut self.atlas, &mut out, sx, st_top, "ENC", "utf-8", track, wide, scale, RULE_2, MUTE);
        if let Some(branch) = &self.status_git {
            let truncated;
            let b: &str = if branch.chars().count() > 24 {
                truncated = branch.chars().take(23).collect::<String>() + "\u{2026}";
                &truncated
            } else {
                branch
            };
            sx += gap;
            sx = Self::seg(&mut self.atlas, &mut out, sx, st_top, "\u{f126}", b, track, wide, scale, RULE_2, TEXT_2);
        }
        sx += gap;
        let _ = Self::seg(&mut self.atlas, &mut out, sx, st_top, "TABS", &self.status_tabs.1, track, wide, scale, RULE_2, MUTE);

        // right cluster (right→left): version · READY/PANE · clock
        let ver = "termie 0.1";
        let ver_w = self.text_w(FontId::Chrome, ver, track);
        let (ready, ready_col) = if self.broadcast {
            ("BROADCAST", PAPER)
        } else if self.pane_mode {
            ("PANE MODE", PAPER)
        } else {
            ("READY", TEXT_2)
        };
        let ready_w = self.text_w(FontId::Chrome, ready, wide);
        let rx_ver = w - pad - ver_w;
        let _ = Self::draw_text(
            &mut self.atlas, &mut out, FontId::Chrome, rx_ver, st_top, ver, RULE_2, 1.0, track,
        );
        let rx_ready = rx_ver - (16.0 * self.scale).round() - ready_w;
        let _ = Self::draw_text(
            &mut self.atlas, &mut out, FontId::Chrome, rx_ready, st_top, ready, ready_col, 1.0, wide,
        );
        if !self.status_clock.is_empty() {
            let clk_w = self.text_w(FontId::Chrome, &self.status_clock, track);
            let rx_clk = rx_ready - (16.0 * self.scale).round() - clk_w;
            let _ = Self::draw_text(
                &mut self.atlas, &mut out, FontId::Chrome, rx_clk, st_top, &self.status_clock, MUTE, 1.0, track,
            );
        }

        // ---- overlays ---- (bloom in: stamp the open, then scale the whole
        // overlay instance range's alpha by an eased 0→1 so scrim + box + text
        // fade together without threading the factor through each draw fn)
        let overlay_now = self.pane_menu_view.is_some()
            || self.palette_view.is_some()
            || self.market_view.is_some()
            || self.find_view.is_some()
            || self.confirm_view.is_some()
            || self.rename_view.is_some();
        if overlay_now && !self.overlay_shown {
            self.overlay_since = Some(Instant::now());
        }
        self.overlay_shown = overlay_now;
        let overlay_start = out.len();
        if self.pane_menu_view.is_some() {
            self.build_pane_menu(&mut out, track);
        }
        if self.palette_view.is_some() {
            self.build_palette(&mut out, track);
        }
        if self.market_view.is_some() {
            self.build_market(&mut out, track);
        }
        if self.find_view.is_some() {
            self.build_find(&mut out, track);
        }
        if self.confirm_view.is_some() {
            self.build_confirm(&mut out, track);
        }
        if self.rename_view.is_some() {
            self.build_rename(&mut out, track);
        }
        if overlay_now {
            let p = self
                .overlay_since
                .map(|t| {
                    let e = (t.elapsed().as_secs_f32() / Self::OVERLAY_FADE).clamp(0.0, 1.0);
                    1.0 - (1.0 - e).powi(3)
                })
                .unwrap_or(1.0);
            if p < 1.0 {
                for inst in &mut out[overlay_start..] {
                    inst.color[3] *= p;
                }
            }
        }
        // the settings panel draws last so its scrollable body is the final
        // instance range (clipped via scissor in render); build_settings sets
        // panel_clip when it draws the body
        self.panel_clip = None;
        if self.settings_p > 0.001 {
            self.build_settings(&mut out, track);
        }

        // startup reveal: a dim scrim eases up from the background and the
        // title-rule seam firms once — a quiet settle, not a power-on. purely
        // visual — input is live underneath the whole time. skipped while the
        // settings panel is open so these rects can't land in the scissored
        // panel range (they'd be clipped to it)
        if self.panel_clip.is_none() {
            let t = self.startup_t();
            if t < 1.0 {
                let fade = self.startup_fade();
                if fade > 0.0 {
                    Self::push_rect(&mut out, 0.0, 0.0, w, h, INK_0, fade);
                }
                // a single in-place hairline firms the title-rule seam as the
                // scrim clears, then fades — the chrome's own machined edge
                // reading a touch crisper for a beat, not a laser sweep
                let rise = t * (2.0 - t);
                let settle = 1.0 - (1.0 - rise).powi(3);
                let a = settle * (1.0 - t);
                if a > 0.0 {
                    let ay = self.title_bar_h - hair * 2.0;
                    Self::push_rect(&mut out, 0.0, ay, w, hair, RULE_2, 0.6 * a);
                }
            }
        }

        out
    }

    const STARTUP_FADE: f32 = 0.22;

    /// restart the power-on reveal; called the instant the window becomes
    /// visible so the whole animation plays in view rather than during gpu init
    pub fn begin_reveal(&mut self) {
        self.reveal_start = Instant::now();
    }

    /// normalized startup-reveal progress: 0 → 1 over STARTUP_FADE, then ≥ 1
    fn startup_t(&self) -> f32 {
        (self.reveal_start.elapsed().as_secs_f32() / Self::STARTUP_FADE).min(1.0)
    }

    /// dim-overlay alpha for the reveal: 1 → 0 over STARTUP_FADE (ease-out)
    fn startup_fade(&self) -> f32 {
        let t = self.startup_t();
        if t >= 1.0 {
            return 0.0;
        }
        let e = 1.0 - t;
        e * e * e
    }

    pub fn startup_fading(&self) -> bool {
        self.reveal_start.elapsed().as_secs_f32() < Self::STARTUP_FADE
    }

    #[allow(non_snake_case)]
    fn build_settings(&mut self, out: &mut Vec<Instance>, track: f32) {
        let INK_0 = self.palette.ink0;
        let INK_1 = self.palette.ink1;
        let RULE = self.palette.rule;
        let RULE_2 = self.palette.rule2;
        let MUTE = self.palette.mute;
        let TEXT_2 = self.palette.text2;
        let PAPER = self.palette.paper;
        let s = self.scale;
        let hair = s.max(1.0);
        let wide = (0.14 * self.atlas.metrics(FontId::Chrome).cell_w).max(1.0);
        let chrome_h = self.atlas.metrics(FontId::Chrome).cell_h;

        // snapshot Copy values so we can borrow self.atlas freely while drawing
        let sv = self.settings_view;
        let blink = self.cursor_blink;
        let theme = self.theme;
        let cur_name = self.cursor_style_name();
        let font_name = self.font_name();
        let font_size = self.content_pt as i32;
        let pad_px = self.pane_pad_px as i32;
        let opacity_pct = self.opacity_pct();
        let p = self.settings_p;
        let cw_c = self.atlas.metrics(FontId::Chrome).cell_w;
        let g = self.settings_geom();
        let bh = g.bh;
        let cx = g.content_x;
        let cw = g.content_w;
        let lbl = |y: f32| (y + (bh - chrome_h) / 2.0).round();

        // ── fixed chrome: scrim + panel body + header (drawn unclipped) ──
        Self::push_rect(out, 0.0, g.panel_top, self.config.width as f32, g.panel_h, INK_0, 0.32 * p);
        Self::push_rect(out, g.panel_x, g.panel_top, g.panel_w, g.panel_h, INK_1, 1.0);
        Self::push_rect(out, g.panel_x, g.panel_top, hair, g.panel_h, RULE, 1.0); // left edge
        // top accent runs the full window width so it reads as a continuous rail
        // the drawer slides in along; alpha tracks the slide so it fades in too
        Self::push_rect(out, 0.0, g.panel_top, self.config.width as f32, hair * 2.0, PAPER, p);

        let _ = Self::draw_text(
            &mut self.atlas, out, FontId::Chrome, cx, g.head_y, "\u{25B8} SETTINGS", PAPER, 1.0, wide,
        );
        // ✕ close button
        let (qx, qy, qw, qh) = g.close_btn;
        let q_hover = self.hovered == Some(Hot::PanelClose);
        if q_hover {
            Self::push_rect(out, qx, qy, qw, qh, PAPER, 1.0);
        }
        let qcol = if q_hover { INK_0 } else { MUTE };
        let qgx = (qx + (qw - cw_c) / 2.0).round();
        let qgy = (qy + (qh - chrome_h) / 2.0).round();
        let _ = Self::draw_text(&mut self.atlas, out, FontId::Chrome, qgx, qgy, "\u{f00d}", qcol, 1.0, track);
        Self::push_rect(out, cx, g.body_top - 12.0 * s, cw, hair, RULE, 1.0);

        // ── scrollable body: everything after this index is scissor-clipped ──
        let body_start = out.len() as u32;
        self.panel_clip = Some((
            body_start,
            [g.panel_x, g.body_top, g.panel_w, g.body_bottom - g.body_top],
        ));

        // PLUGINS (top of the panel: installed list with on/off + browse store)
        self.section_label(out, cx, g.sec_plugins_y, cw, "PLUGINS", wide, RULE_2, MUTE);
        self.cycle_btn(out, g.plugins_btn, "browse \u{25B8}", Hot::OpenPlugins, track);
        if g.plugin_rows.is_empty() {
            let _ = Self::draw_text(&mut self.atlas, out, FontId::Chrome, cx, lbl(g.sec_plugins_y + 32.0 * s), "no plugins installed", RULE_2, 1.0, track);
        } else {
            for (i, (name, on, rect, ry)) in g.plugin_rows.iter().enumerate() {
                let _ = Self::draw_text(&mut self.atlas, out, FontId::Chrome, cx, lbl(*ry), name, MUTE, 1.0, wide);
                self.toggle_btn(out, *rect, *on, Hot::PluginToggle(i), track);
            }
        }

        // APPEARANCE
        self.section_label(out, cx, g.sec_app_y, cw, "APPEARANCE", wide, RULE_2, MUTE);
        let _ = Self::draw_text(&mut self.atlas, out, FontId::Chrome, cx, lbl(g.font_y), "FONT SIZE", MUTE, 1.0, wide);
        self.stepper(out, g.font_dec, g.font_inc, &format!("{font_size}"), Hot::FontDec, Hot::FontInc, g.val_w, track);
        let _ = Self::draw_text(&mut self.atlas, out, FontId::Chrome, cx, lbl(g.fontfam_y), "FONT", MUTE, 1.0, wide);
        self.cycle_btn(out, g.fontfam_btn, font_name, Hot::FontCycle, track);
        let _ = Self::draw_text(&mut self.atlas, out, FontId::Chrome, cx, lbl(g.pad_y), "PADDING", MUTE, 1.0, wide);
        self.stepper(out, g.pad_dec, g.pad_inc, &format!("{pad_px}"), Hot::PadDec, Hot::PadInc, g.val_w, track);
        let _ = Self::draw_text(&mut self.atlas, out, FontId::Chrome, cx, lbl(g.opacity_y), "OPACITY", MUTE, 1.0, wide);
        self.stepper(out, g.op_dec, g.op_inc, &format!("{opacity_pct}%"), Hot::OpacityDec, Hot::OpacityInc, g.val_w, track);
        let _ = Self::draw_text(&mut self.atlas, out, FontId::Chrome, cx, lbl(g.cursor_y), "CURSOR", MUTE, 1.0, wide);
        self.cycle_btn(out, g.cursor_btn, cur_name, Hot::CursorCycle, track);
        let _ = Self::draw_text(&mut self.atlas, out, FontId::Chrome, cx, lbl(g.blink_y), "CURSOR BLINK", MUTE, 1.0, wide);
        self.toggle_btn(out, g.blink_btn, blink, Hot::CursorBlink, track);
        let _ = Self::draw_text(&mut self.atlas, out, FontId::Chrome, cx, lbl(g.theme_label_y), "THEME", MUTE, 1.0, wide);
        let themes = [ThemeId::Instrument, ThemeId::Koi, ThemeId::Paper];
        for (i, id) in themes.into_iter().enumerate() {
            self.theme_chip(out, g.theme_chips[i], id, theme == id, track);
        }

        // BEHAVIOR
        self.section_label(out, cx, g.sec_beh_y, cw, "BEHAVIOR", wide, RULE_2, MUTE);
        let _ = Self::draw_text(&mut self.atlas, out, FontId::Chrome, cx, lbl(g.scrollback_y), "SCROLLBACK", MUTE, 1.0, wide);
        self.stepper(out, g.sb_dec, g.sb_inc, &format!("{}", sv.scrollback), Hot::ScrollbackDec, Hot::ScrollbackInc, g.val_w, track);
        let _ = Self::draw_text(&mut self.atlas, out, FontId::Chrome, cx, lbl(g.copysel_y), "COPY ON SELECT", MUTE, 1.0, wide);
        self.toggle_btn(out, g.copysel_btn, sv.copy_on_select, Hot::CopyOnSelect, track);

        // SHELL
        self.section_label(out, cx, g.sec_shell_y, cw, "SHELL", wide, RULE_2, MUTE);
        let _ = Self::draw_text(&mut self.atlas, out, FontId::Chrome, cx, lbl(g.shell_y), "DEFAULT SHELL", MUTE, 1.0, wide);
        self.cycle_btn(out, g.shell_btn, sv.shell_name, Hot::ShellCycle, track);
        let _ = Self::draw_text(&mut self.atlas, out, FontId::Chrome, cx, lbl(g.profile_y), "LOAD PROFILE", MUTE, 1.0, wide);
        self.toggle_btn(out, g.profile_btn, sv.load_profile, Hot::LoadProfile, track);
        let _ = Self::draw_text(&mut self.atlas, out, FontId::Chrome, cx, lbl(g.close_y), "CLOSE BUTTON", MUTE, 1.0, wide);
        self.cycle_btn(out, g.close_action_btn, sv.close_action_name, Hot::CloseActionCycle, track);
        let _ = Self::draw_text(&mut self.atlas, out, FontId::Chrome, cx, lbl(g.backend_y), "BACKEND", MUTE, 1.0, wide);
        self.cycle_btn(out, g.backend_btn, sv.backend_name, Hot::BackendCycle, track);
        // backend can't swap live; hint that it applies next launch
        let (bbx, _, bbw, _) = g.backend_btn;
        let _ = Self::draw_text(&mut self.atlas, out, FontId::Chrome, bbx + bbw + 10.0 * s, lbl(g.backend_y), "(restart)", RULE_2, 1.0, track);

        // KEYBINDINGS (two sub-columns)
        self.section_label(out, cx, g.sec_keys_y, cw, "KEYBINDINGS", wide, RULE_2, MUTE);
        let keys: [(&str, &str); 11] = [
            ("Ctrl+T", "new tab"),
            ("Ctrl+P", "palette"),
            ("Ctrl+Shift+P", "pane mode"),
            ("Ctrl+Shift+E", "split V"),
            ("Ctrl+Shift+O", "split H"),
            ("Ctrl+Shift+W", "close pane"),
            ("Ctrl+Shift+C", "copy"),
            ("Ctrl+Shift+V", "paste"),
            ("Ctrl+Tab", "next tab"),
            ("Ctrl+,", "settings"),
            ("Esc", "close"),
        ];
        let key_row = 22.0 * s;
        let sub2 = cx + cw / 2.0;
        let desc_dx = 108.0 * s;
        for (i, (k, d)) in keys.into_iter().enumerate() {
            let (kx, ky) = if i < 6 {
                (cx, g.keys_start_y + i as f32 * key_row)
            } else {
                (sub2, g.keys_start_y + (i - 6) as f32 * key_row)
            };
            let _ = Self::draw_text(&mut self.atlas, out, FontId::Chrome, kx, ky, k, TEXT_2, 1.0, track);
            let _ = Self::draw_text(&mut self.atlas, out, FontId::Chrome, kx + desc_dx, ky, d, MUTE, 1.0, track);
        }

        // ABOUT
        self.section_label(out, cx, g.sec_about_y, cw, "ABOUT", wide, RULE_2, MUTE);
        let about: [(&str, &str); 3] = [
            ("FONT", font_name),
            ("VERSION", "termie 0.1"),
            ("RENDERER", self.backend_label),
        ];
        let about_dx = 120.0 * s;
        for (i, (k, v)) in about.into_iter().enumerate() {
            let ya = g.about_start_y + i as f32 * 28.0 * s;
            let _ = Self::draw_text(&mut self.atlas, out, FontId::Chrome, cx, ya, k, MUTE, 1.0, wide);
            let _ = Self::draw_text(&mut self.atlas, out, FontId::Chrome, cx + about_dx, ya, v, TEXT_2, 1.0, track);
        }
    }

    /// the right-side plugin dock: a flat instrument panel listing each Tier-1
    /// widget (title in the accent color, then its text lines). drawn in the
    /// content band, to the right of the reflowed panes
    #[allow(non_snake_case)]
    fn draw_dock(&mut self, out: &mut Vec<Instance>, track: f32) {
        let s = self.scale;
        let hair = s.max(1.0);
        let dock_w = self.dock_w();
        let (cx, cy, cw, ch) = self.content_rect();
        // the dock sits just right of the content rect, filling the carved band
        let dx = (cx + cw).round();
        let dw = (dock_w - self.pad).max(1.0);
        let chrome_h = self.atlas.metrics(FontId::Chrome).cell_h;

        let INK_1 = self.palette.ink1;
        let RULE = self.palette.rule;
        let TEXT_2 = self.palette.text2;
        let PAPER = self.palette.paper;

        // panel ground + a left hairline that reads as the seam to the terminal
        Self::push_rect(out, dx, cy, dw, ch, INK_1, 1.0);
        Self::push_rect(out, dx, cy, hair, ch, RULE, 1.0);

        let pad = (12.0 * s).round();
        let row = (chrome_h + 6.0 * s).round();
        let mut y = cy + pad;
        self.dock_hitboxes.clear();
        // snapshot widget data so the atlas can be borrowed mutably while drawing
        let widgets: Vec<(String, Vec<String>)> =
            self.dock.iter().map(|w| (w.title.clone(), w.lines.clone())).collect();
        for (i, (title, lines)) in widgets.iter().enumerate() {
            let band_top = if i == 0 { cy } else { y - pad * 0.5 };
            if i > 0 {
                let ry = (y - pad * 0.5).round();
                Self::push_rect(out, dx + pad, ry, dw - pad * 2.0, hair, RULE, 1.0);
            }
            let _ = Self::draw_text(&mut self.atlas, out, FontId::Chrome, dx + pad, y.round(), title, PAPER, 1.0, track);
            y += row;
            for line in lines {
                if y > cy + ch - row {
                    break; // clip to dock height; no scroll in v1
                }
                let _ = Self::draw_text(&mut self.atlas, out, FontId::Chrome, dx + pad, y.round(), line, TEXT_2, 1.0, track);
                y += row;
            }
            y += pad * 0.5;
            self.dock_hitboxes.push((dx, band_top, dw, (y - band_top).max(0.0)));
        }
    }

    /// right-click pane context menu: a small panel of pane actions at the click
    #[allow(non_snake_case)]
    fn build_pane_menu(&mut self, out: &mut Vec<Instance>, track: f32) {
        let Some(v) = self.pane_menu_view.as_ref() else {
            return;
        };
        let (mx, my, hovered) = (v.x, v.y, v.hovered);
        let (bx, by, mw, row_h, pad) = self.pane_menu_geom(mx, my);
        let INK_0 = self.palette.ink0;
        let INK_1 = self.palette.ink1;
        let INK_3 = self.palette.ink3;
        let RULE_2 = self.palette.rule2;
        let TEXT_2 = self.palette.text2;
        let PAPER = self.palette.paper;
        let s = self.scale;
        let hair = s.max(1.0);
        let chrome_h = self.atlas.metrics(FontId::Chrome).cell_h;
        let mh = row_h * PANE_MENU_ITEMS.len() as f32 + pad * 2.0;
        Self::push_rect(out, bx - 2.0 * s, by + 4.0 * s, mw + 4.0 * s, mh, INK_0, 0.5);
        Self::push_rect(out, bx, by, mw, mh, INK_1, 1.0);
        Self::stroke_rect(out, (bx, by, mw, mh), hair, RULE_2);
        for (i, lbl) in PANE_MENU_ITEMS.iter().enumerate() {
            let ry = by + pad + row_h * i as f32;
            if hovered == Some(i) {
                Self::push_rect(out, bx, ry, mw, row_h, INK_3, 1.0);
                Self::push_rect(out, bx, ry, 2.0 * s, row_h, PAPER, 1.0);
            }
            let ty = (ry + (row_h - chrome_h) / 2.0).round();
            let col = if hovered == Some(i) { PAPER } else { TEXT_2 };
            let _ = Self::draw_text(&mut self.atlas, out, FontId::Chrome, bx + pad + 4.0 * s, ty, lbl, col, 1.0, track);
        }
    }

    /// centered command-palette overlay: search input + filtered action list
    #[allow(non_snake_case)]
    fn build_palette(&mut self, out: &mut Vec<Instance>, track: f32) {
        let Some(pv) = self.palette_view.as_ref() else {
            return;
        };
        let query = pv.query.clone();
        let items: Vec<String> = pv.items.iter().take(9).cloned().collect();
        let selected = pv.selected;
        let INK_0 = self.palette.ink0;
        let INK_1 = self.palette.ink1;
        let INK_3 = self.palette.ink3;
        let RULE_2 = self.palette.rule2;
        let MUTE = self.palette.mute;
        let TEXT_2 = self.palette.text2;
        let PAPER = self.palette.paper;
        let s = self.scale;
        let hair = s.max(1.0);
        let w = self.config.width as f32;
        let h = self.config.height as f32;
        let chrome_h = self.atlas.metrics(FontId::Chrome).cell_h;

        Self::push_rect(out, 0.0, 0.0, w, h, INK_0, 0.55);
        let bw = (560.0 * s).min(w - 80.0 * s);
        let bx = ((w - bw) / 2.0).round();
        let by = (self.title_bar_h + 70.0 * s).round();
        let row_h = chrome_h + 14.0 * s;
        let rows = items.len().max(1) as f32 + 1.0;
        let bh = row_h * rows + 8.0 * s;
        // drop shadow + box + border
        Self::push_rect(out, bx - 2.0 * s, by + 5.0 * s, bw + 4.0 * s, bh, INK_0, 0.5);
        Self::push_rect(out, bx, by, bw, bh, INK_1, 1.0);
        Self::stroke_rect(out, (bx, by, bw, bh), hair, RULE_2);

        let pad = 16.0 * s;
        // search input row
        let iy = (by + 8.0 * s + (row_h - chrome_h) / 2.0).round();
        let prompt = format!("\u{f002}  {}", query);
        let _ = Self::draw_text(&mut self.atlas, out, FontId::Chrome, bx + pad, iy, &prompt, TEXT_2, 1.0, track);
        let cwid = self.text_w(FontId::Chrome, &prompt, track);
        Self::push_rect(out, bx + pad + cwid + 2.0 * s, iy, 2.0 * s, chrome_h, PAPER, 1.0);
        Self::push_rect(out, bx, by + row_h, bw, hair, RULE_2, 1.0);

        if items.is_empty() {
            let ty = (by + row_h + (row_h - chrome_h) / 2.0).round();
            let _ = Self::draw_text(&mut self.atlas, out, FontId::Chrome, bx + pad, ty, "no matches", MUTE, 1.0, track);
        }
        for (idx, lbl) in items.iter().enumerate() {
            let ry = by + row_h * (idx as f32 + 1.0);
            if idx == selected {
                Self::push_rect(out, bx, ry, bw, row_h, INK_3, 1.0);
                Self::push_rect(out, bx, ry, 2.0 * s, row_h, PAPER, 1.0);
            }
            let ty = (ry + (row_h - chrome_h) / 2.0).round();
            let col = if idx == selected { PAPER } else { TEXT_2 };
            let _ = Self::draw_text(&mut self.atlas, out, FontId::Chrome, bx + pad, ty, lbl, col, 1.0, track);
        }
    }

    /// find-in-scrollback overlay: a single search box pinned below the title
    /// bar showing the query and match position; matches are highlighted in the
    /// grid by draw_grid, this only draws the input box
    #[allow(non_snake_case)]
    fn build_find(&mut self, out: &mut Vec<Instance>, track: f32) {
        let Some(fv) = self.find_view.as_ref() else {
            return;
        };
        let query = fv.query.clone();
        let count = fv.count;
        let current = fv.current;
        let INK_0 = self.palette.ink0;
        let INK_1 = self.palette.ink1;
        let RULE_2 = self.palette.rule2;
        let TEXT_2 = self.palette.text2;
        let MUTE = self.palette.mute;
        let s = self.scale;
        let hair = s.max(1.0);
        let w = self.config.width as f32;
        let chrome_h = self.atlas.metrics(FontId::Chrome).cell_h;
        let bw = (560.0 * s).min(w - 80.0 * s);
        let bx = ((w - bw) / 2.0).round();
        let by = (self.title_bar_h + 12.0 * s).round();
        let row_h = chrome_h + 14.0 * s;
        let bh = row_h + 8.0 * s;
        Self::push_rect(out, bx - 2.0 * s, by + 5.0 * s, bw + 4.0 * s, bh, INK_0, 0.5);
        Self::push_rect(out, bx, by, bw, bh, INK_1, 1.0);
        Self::stroke_rect(out, (bx, by, bw, bh), hair, RULE_2);
        let pad = 16.0 * s;
        let iy = (by + 4.0 * s + (row_h - chrome_h) / 2.0).round();
        let prompt = format!("\u{f002}  {}", query);
        let _ = Self::draw_text(&mut self.atlas, out, FontId::Chrome, bx + pad, iy, &prompt, TEXT_2, 1.0, track);
        let info = if count == 0 {
            if query.is_empty() {
                String::new()
            } else {
                "no matches".to_string()
            }
        } else {
            format!("{}/{}", current + 1, count)
        };
        if !info.is_empty() {
            let iw = self.text_w(FontId::Chrome, &info, track);
            let _ = Self::draw_text(&mut self.atlas, out, FontId::Chrome, bx + bw - pad - iw, iy, &info, MUTE, 1.0, track);
        }
    }

    /// modal confirm box: centered panel with the prompt on top and a key hint
    /// below. blocking is enforced by the app's key-capture, not here; shared by
    /// the paste guard and other yes/no prompts
    fn build_confirm(&mut self, out: &mut Vec<Instance>, track: f32) {
        let Some(cv) = self.confirm_view.as_ref() else {
            return;
        };
        let prompt = cv.prompt.clone();
        let hint = cv.hint.clone();
        let ink0 = self.palette.ink0;
        let ink1 = self.palette.ink1;
        let rule2 = self.palette.rule2;
        let paper = self.palette.paper;
        let mute = self.palette.mute;
        let s = self.scale;
        let hair = s.max(1.0);
        let w = self.config.width as f32;
        let h = self.config.height as f32;
        let chrome_h = self.atlas.metrics(FontId::Chrome).cell_h;
        let row_h = chrome_h + 10.0 * s;
        let pad = 18.0 * s;
        let bw = (520.0 * s).min(w - 80.0 * s);
        let bh = (row_h * 2.0 + pad * 2.0).round();
        let bx = ((w - bw) / 2.0).round();
        let by = ((h - bh) / 2.0).round().max(self.title_bar_h + 12.0 * s);
        Self::push_rect(out, bx - 2.0 * s, by + 6.0 * s, bw + 4.0 * s, bh, ink0, 0.5);
        Self::push_rect(out, bx, by, bw, bh, ink1, 1.0);
        Self::stroke_rect(out, (bx, by, bw, bh), hair, rule2);
        let tx = bx + pad;
        let ty = (by + pad).round();
        let _ = Self::draw_text(&mut self.atlas, out, FontId::Chrome, tx, ty, &prompt, paper, 1.0, track);
        let _ = Self::draw_text(&mut self.atlas, out, FontId::Chrome, tx, ty + row_h, &hint, mute, 1.0, track);
    }

    /// the tab-rename field: a centered box with the editable name + a caret,
    /// modeled on build_confirm
    fn build_rename(&mut self, out: &mut Vec<Instance>, track: f32) {
        let Some(rv) = self.rename_view.as_ref() else {
            return;
        };
        let label = format!("rename tab: {}\u{2588}", rv.buf);
        let hint = "enter: rename \u{b7} esc: cancel".to_string();
        let ink0 = self.palette.ink0;
        let ink1 = self.palette.ink1;
        let rule2 = self.palette.rule2;
        let paper = self.palette.paper;
        let mute = self.palette.mute;
        let s = self.scale;
        let hair = s.max(1.0);
        let w = self.config.width as f32;
        let h = self.config.height as f32;
        let chrome_h = self.atlas.metrics(FontId::Chrome).cell_h;
        let row_h = chrome_h + 10.0 * s;
        let pad = 18.0 * s;
        let bw = (520.0 * s).min(w - 80.0 * s);
        let bh = (row_h * 2.0 + pad * 2.0).round();
        let bx = ((w - bw) / 2.0).round();
        let by = ((h - bh) / 2.0).round().max(self.title_bar_h + 12.0 * s);
        Self::push_rect(out, bx - 2.0 * s, by + 6.0 * s, bw + 4.0 * s, bh, ink0, 0.5);
        Self::push_rect(out, bx, by, bw, bh, ink1, 1.0);
        Self::stroke_rect(out, (bx, by, bw, bh), hair, rule2);
        let tx = bx + pad;
        let ty = (by + pad).round();
        let _ = Self::draw_text(&mut self.atlas, out, FontId::Chrome, tx, ty, &label, paper, 1.0, track);
        let _ = Self::draw_text(&mut self.atlas, out, FontId::Chrome, tx, ty + row_h, &hint, mute, 1.0, track);
    }

    /// plugins marketplace overlay: a centered panel listing installed + catalog
    /// plugins, each as name+version with a state tag and a permissions subline,
    /// plus a status footer. modeled on the command palette
    #[allow(non_snake_case)]
    fn build_market(&mut self, out: &mut Vec<Instance>, track: f32) {
        let Some(mv) = self.market_view.as_ref() else {
            return;
        };
        // snapshot so the atlas can be borrowed mutably while drawing
        let rows: Vec<(String, String, String)> = mv
            .rows
            .iter()
            .map(|r| (r.label.clone(), r.tag.clone(), r.sub.clone()))
            .collect();
        let selected = mv.selected;
        let status = mv.status.clone();

        let INK_0 = self.palette.ink0;
        let INK_1 = self.palette.ink1;
        let INK_3 = self.palette.ink3;
        let RULE_2 = self.palette.rule2;
        let MUTE = self.palette.mute;
        let TEXT_2 = self.palette.text2;
        let PAPER = self.palette.paper;
        let s = self.scale;
        let hair = s.max(1.0);
        let w = self.config.width as f32;
        let h = self.config.height as f32;
        let chrome_h = self.atlas.metrics(FontId::Chrome).cell_h;

        // dim the rest of the screen
        Self::push_rect(out, 0.0, 0.0, w, h, INK_0, 0.55);

        let bw = (620.0 * s).min(w - 80.0 * s);
        let bx = ((w - bw) / 2.0).round();
        let by = (self.title_bar_h + 56.0 * s).round();
        let head_h = chrome_h + 16.0 * s;
        let row_h = chrome_h * 2.0 + 16.0 * s; // two text lines per row
        // cap the visible rows so the panel never runs off-screen
        let max_visible = (((h - by - head_h - 40.0 * s) / row_h).floor().max(1.0)) as usize;
        let visible = rows.len().min(max_visible).max(1);
        // scroll so the selected row stays in view
        let first = if selected >= visible { selected + 1 - visible } else { 0 };
        let bh = head_h + row_h * visible as f32 + head_h; // header + rows + footer

        // shadow + body + border + top accent
        Self::push_rect(out, bx - 2.0 * s, by + 5.0 * s, bw + 4.0 * s, bh, INK_0, 0.5);
        Self::push_rect(out, bx, by, bw, bh, INK_1, 1.0);
        Self::stroke_rect(out, (bx, by, bw, bh), hair, RULE_2);
        Self::push_rect(out, bx, by, bw, hair * 2.0, PAPER, 1.0);

        let pad = 16.0 * s;
        // header
        let hy = (by + (head_h - chrome_h) / 2.0).round();
        let _ = Self::draw_text(&mut self.atlas, out, FontId::Chrome, bx + pad, hy, "\u{f487} PLUGINS", PAPER, 1.0, track);
        Self::push_rect(out, bx, by + head_h, bw, hair, RULE_2, 1.0);

        if rows.is_empty() {
            let ty = (by + head_h + (row_h - chrome_h) / 2.0).round();
            let _ = Self::draw_text(&mut self.atlas, out, FontId::Chrome, bx + pad, ty, "no plugins installed", MUTE, 1.0, track);
        }

        for vi in 0..visible {
            let idx = first + vi;
            let Some((label, tag, sub)) = rows.get(idx) else {
                break;
            };
            let ry = by + head_h + row_h * vi as f32;
            if idx == selected {
                Self::push_rect(out, bx, ry, bw, row_h, INK_3, 1.0);
                Self::push_rect(out, bx, ry, 2.0 * s, row_h, PAPER, 1.0);
            }
            let lc = if idx == selected { PAPER } else { TEXT_2 };
            // line 1: label (left) + tag (right)
            let ly = (ry + 6.0 * s).round();
            let _ = Self::draw_text(&mut self.atlas, out, FontId::Chrome, bx + pad, ly, label, lc, 1.0, track);
            let tag_w = self.text_w(FontId::Chrome, tag, track);
            let tag_col = if tag == "on" { PAPER } else { MUTE };
            let _ = Self::draw_text(&mut self.atlas, out, FontId::Chrome, bx + bw - pad - tag_w, ly, tag, tag_col, 1.0, track);
            // line 2: permissions subline (dim)
            if !sub.is_empty() {
                let sy = (ry + 6.0 * s + chrome_h + 2.0 * s).round();
                let _ = Self::draw_text(&mut self.atlas, out, FontId::Chrome, bx + pad, sy, sub, MUTE, 1.0, track);
            }
        }

        // footer status line
        let fy = (by + head_h + row_h * visible as f32 + (head_h - chrome_h) / 2.0).round();
        Self::push_rect(out, bx, by + head_h + row_h * visible as f32, bw, hair, RULE_2, 1.0);
        let _ = Self::draw_text(&mut self.atlas, out, FontId::Chrome, bx + pad, fy, &status, MUTE, 1.0, track);
    }

    /// `LABEL ─────────` section header with a rule filling the remaining width
    #[allow(clippy::too_many_arguments)]
    fn section_label(&mut self, out: &mut Vec<Instance>, x: f32, y: f32, w: f32, text: &str, wide: f32, rule: Rgb, mute: Rgb) {
        let tw = self.text_w(FontId::Chrome, text, wide);
        let _ = Self::draw_text(&mut self.atlas, out, FontId::Chrome, x, y, text, mute, 1.0, wide);
        let hair = self.scale.max(1.0);
        let ry = (y + self.atlas.metrics(FontId::Chrome).cell_h / 2.0).round();
        let rx = x + tw + 12.0 * self.scale;
        Self::push_rect(out, rx, ry, (x + w - rx).max(0.0), hair, rule, 1.0);
    }

    /// an outlined button with centered label that cycles a value on click
    fn cycle_btn(&mut self, out: &mut Vec<Instance>, rect: (f32, f32, f32, f32), text: &str, hot: Hot, track: f32) {
        let chrome_h = self.atlas.metrics(FontId::Chrome).cell_h;
        let (bx, by, bw, bh) = rect;
        // ease the bright fill in on hover (and cross-fade the label dark) to
        // match the title bar instead of snapping
        let he = if self.hovered == Some(hot) { self.hover_ease() } else { 0.0 };
        Self::stroke_rect(out, (bx, by, bw, bh), 1.0, self.palette.rule2);
        if he > 0.0 {
            Self::push_rect(out, bx, by, bw, bh, self.palette.paper, he);
        }
        let tw = self.text_w(FontId::Chrome, text, track);
        let col = self.palette.text2.lerp(self.palette.ink0, he);
        let _ = Self::draw_text(
            &mut self.atlas, out, FontId::Chrome,
            bx + (bw - tw) / 2.0, (by + (bh - chrome_h) / 2.0).round(), text, col, 1.0, track,
        );
    }

    /// an on/off pill: bright label = on, dim = off; fills on hover
    fn toggle_btn(&mut self, out: &mut Vec<Instance>, rect: (f32, f32, f32, f32), on: bool, hot: Hot, track: f32) {
        let chrome_h = self.atlas.metrics(FontId::Chrome).cell_h;
        let (bx, by, bw, bh) = rect;
        let he = if self.hovered == Some(hot) { self.hover_ease() } else { 0.0 };
        Self::stroke_rect(out, (bx, by, bw, bh), 1.0, self.palette.rule2);
        if he > 0.0 {
            Self::push_rect(out, bx, by, bw, bh, self.palette.paper, he);
        }
        let txt = if on { "on" } else { "off" };
        let tw = self.text_w(FontId::Chrome, txt, track);
        let base = if on { self.palette.paper } else { self.palette.mute };
        let col = base.lerp(self.palette.ink0, he);
        let _ = Self::draw_text(
            &mut self.atlas, out, FontId::Chrome,
            bx + (bw - tw) / 2.0, (by + (bh - chrome_h) / 2.0).round(), txt, col, 1.0, track,
        );
    }

    /// a theme chip: name on top + a live swatch strip of that theme's palette,
    /// filled when active, outlined otherwise, lit on hover
    fn theme_chip(&mut self, out: &mut Vec<Instance>, rect: (f32, f32, f32, f32), id: ThemeId, active: bool, track: f32) {
        let s = self.scale;
        let cw = self.atlas.metrics(FontId::Chrome).cell_w;
        let (bx, by, bw, bh) = rect;
        let hot = Hot::ThemeSet(id);
        let hov = self.hovered == Some(hot);
        let he = if hov { self.hover_ease() } else { 0.0 };
        if active {
            Self::push_rect(out, bx, by, bw, bh, self.palette.paper, 1.0);
        } else {
            Self::stroke_rect(out, (bx, by, bw, bh), 1.0, self.palette.rule2);
            if he > 0.0 {
                Self::push_rect(out, bx, by, bw, bh, self.palette.ink4, he);
            }
        }
        // theme name (truncated to fit), centered at the top
        let name = id.name();
        let maxc = (((bw - 8.0 * s) / (cw + track)).floor().max(1.0)) as usize;
        let t: String = if name.chars().count() > maxc {
            name.chars().take(maxc).collect()
        } else {
            name.to_string()
        };
        let tw = self.text_w(FontId::Chrome, &t, track);
        let col = if active {
            self.palette.ink0
        } else {
            self.palette.text2.lerp(self.palette.paper, he)
        };
        let _ = Self::draw_text(&mut self.atlas, out, FontId::Chrome, bx + (bw - tw) / 2.0, (by + 6.0 * s).round(), &t, col, 1.0, track);

        // swatch strip: bg · fg · accent · ansi red · ansi blue
        let pal = Palette::from_theme(id);
        let sw = [
            pal.bg,
            pal.fg,
            pal.paper,
            pal.resolve_fg(Color::Indexed(1)),
            pal.resolve_fg(Color::Indexed(4)),
        ];
        let n = sw.len() as f32;
        let gap = 3.0 * s;
        let inset = 9.0 * s;
        let strip_w = (bw - inset * 2.0).max(1.0);
        let cell_w = ((strip_w - gap * (n - 1.0)) / n).max(1.0);
        let sh = 11.0 * s;
        let sy = by + bh - sh - 6.0 * s;
        let dim = |c: Rgb| Rgb::new(
            (c.r as f32 * 0.72) as u8,
            (c.g as f32 * 0.72) as u8,
            (c.b as f32 * 0.72) as u8,
        );
        for (i, c) in sw.into_iter().enumerate() {
            let sx = bx + inset + i as f32 * (cell_w + gap);
            // each swatch is a small vertical gradient for depth; on hover the
            // (inactive) chip flips it bottom→top so the strip reads as lifted
            if hov && !active {
                Self::push_vgradient(out, sx, sy, cell_w, sh, dim(c), c, 6);
            } else {
                Self::push_vgradient(out, sx, sy, cell_w, sh, c, dim(c), 6);
            }
        }
    }

    /// draw a [−] value [+] stepper given the two button rects
    #[allow(clippy::too_many_arguments, non_snake_case)]
    fn stepper(
        &mut self,
        out: &mut Vec<Instance>,
        dec: (f32, f32, f32, f32),
        inc: (f32, f32, f32, f32),
        val: &str,
        hot_dec: Hot,
        hot_inc: Hot,
        val_w: f32,
        track: f32,
    ) {
        let INK_0 = self.palette.ink0;
        let RULE_2 = self.palette.rule2;
        let TEXT_2 = self.palette.text2;
        let PAPER = self.palette.paper;
        let chrome_h = self.atlas.metrics(FontId::Chrome).cell_h;

        for (rect, glyph, hot) in [(dec, "\u{2013}", hot_dec), (inc, "+", hot_inc)] {
            let (bx, by, bw, bh) = rect;
            let he = if self.hovered == Some(hot) { self.hover_ease() } else { 0.0 };
            // 1px outline button, with the fill easing in over it on hover
            Self::push_rect(out, bx, by, bw, 1.0, RULE_2, 1.0);
            Self::push_rect(out, bx, by + bh - 1.0, bw, 1.0, RULE_2, 1.0);
            Self::push_rect(out, bx, by, 1.0, bh, RULE_2, 1.0);
            Self::push_rect(out, bx + bw - 1.0, by, 1.0, bh, RULE_2, 1.0);
            if he > 0.0 {
                Self::push_rect(out, bx, by, bw, bh, PAPER, he);
            }
            let gx = (bx + (bw - self.atlas.metrics(FontId::Chrome).cell_w) / 2.0).round();
            let gy = (by + (bh - chrome_h) / 2.0).round();
            let col = TEXT_2.lerp(INK_0, he);
            let _ = Self::draw_text(&mut self.atlas, out, FontId::Chrome, gx, gy, glyph, col, 1.0, track);
        }
        // value centered between the buttons
        let (dx, dy, dw, _dh) = dec;
        let vx = dx + dw + (val_w - self.text_w(FontId::Chrome, val, track)) / 2.0;
        let _ = Self::draw_text(&mut self.atlas, out, FontId::Chrome, vx, dy + (4.0 * self.scale), val, TEXT_2, 1.0, track);
    }

    /// draw a `KEY value` status segment; returns the pen end-x
    #[allow(clippy::too_many_arguments)]
    #[allow(clippy::too_many_arguments)]
    fn seg(
        atlas: &mut GlyphAtlas,
        out: &mut Vec<Instance>,
        x: f32,
        y_top: f32,
        key: &str,
        val: &str,
        track: f32,
        wide: f32,
        scale: f32,
        key_c: Rgb,
        val_c: Rgb,
    ) -> f32 {
        let mut px = Self::draw_text(atlas, out, FontId::Chrome, x, y_top, key, key_c, 1.0, wide);
        px += (7.0 * scale).round();
        Self::draw_text(atlas, out, FontId::Chrome, px, y_top, val, val_c, 1.0, track)
    }

    pub fn render(&mut self, panes: &[PaneView], focused: bool, maximized: bool, focus_ease: f32, bare: bool) -> Result<()> {
        // a device-lost callback fired since the last frame: rebuild the gpu now,
        // before building/uploading anything against the dead device
        if self.device_lost.swap(false, Ordering::SeqCst)
            && let Some(w) = self.window.clone()
        {
            self.recreate(w)?;
        }
        let instances = self.build(panes, focused, maximized, focus_ease, bare);
        self.upload_atlas();

        let needed = instances.len() as u64;
        if needed > self.instance_capacity {
            self.instance_capacity = (needed * 2).next_power_of_two();
            self.instance_buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("instances"),
                size: self.instance_capacity * std::mem::size_of::<Instance>() as u64,
                usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
        }
        if !instances.is_empty() {
            self.queue
                .write_buffer(&self.instance_buffer, 0, bytemuck::cast_slice(&instances));
        }

        let uniforms = Uniforms {
            screen: [self.config.width as f32, self.config.height as f32],
            _pad: [0.0, 0.0],
        };
        self.queue
            .write_buffer(&self.uniform_buffer, 0, bytemuck::bytes_of(&uniforms));

        use wgpu::CurrentSurfaceTexture as Cst;
        if self.surface.is_none() {
            return Ok(());
        }
        // get_current_texture returns an owned enum, so the surface borrow ends
        // before each arm runs — letting the Lost arm call the &mut self recreate
        let frame = match self.surface.as_ref().unwrap().get_current_texture() {
            Cst::Success(f) | Cst::Suboptimal(f) => Some(f),
            Cst::Outdated => {
                // stale swapchain (resize / format change): cheap reconfigure
                let s = self.surface.as_ref().unwrap();
                s.configure(&self.device, &self.config);
                match s.get_current_texture() {
                    Cst::Success(f) | Cst::Suboptimal(f) => Some(f),
                    _ => None,
                }
            }
            Cst::Lost => {
                // swapchain/device lost: rebuild the gpu and SKIP this frame. the
                // per-frame uploads above already ran against the now-dead device,
                // so drawing now would use the fresh empty buffers (blank frame —
                // and an over-capacity instance range could trip a validation
                // abort). recreate clears device_lost + marks the atlas dirty, so
                // the next frame repaints correctly against the new device
                if let Some(w) = self.window.clone() {
                    self.recreate(w)?;
                }
                return Ok(());
            }
            _ => None,
        };
        let Some(frame) = frame else {
            return Ok(());
        };
        let view = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());

        // premultiplied clear: the empty field is the glassy warm-dark
        let bg = self.palette.bg.to_linear_f32();
        let a = self.bg_alpha as f64;
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("encoder"),
            });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("cell-pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: bg[0] as f64 * a,
                            g: bg[1] as f64 * a,
                            b: bg[2] as f64 * a,
                            a,
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                    depth_slice: None,
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });

            if !instances.is_empty() {
                pass.set_pipeline(&self.pipeline);
                pass.set_bind_group(0, &self.uniform_bind_group, &[]);
                pass.set_bind_group(1, &self.atlas_bind_group, &[]);
                pass.set_vertex_buffer(0, self.instance_buffer.slice(..));
                // clamp the drawn range to the live buffer capacity so a count vs
                // buffer-size desync can never feed an oversized range to wgpu
                // (which, with panic=abort, would take the process down)
                let total = (instances.len() as u32).min(self.instance_capacity as u32);
                match self.panel_clip {
                    // everything up to `start` is the terminal/chrome + fixed panel
                    // (full surface); the body after it is clipped to the panel
                    Some((start, clip)) if start < total => {
                        pass.draw(0..6, 0..start);
                        let (w, h) = (self.config.width as f32, self.config.height as f32);
                        let sx = clip[0].clamp(0.0, w);
                        let sy = clip[1].clamp(0.0, h);
                        let sw = ((clip[0] + clip[2]).min(w) - sx).max(0.0);
                        let sh = ((clip[1] + clip[3]).min(h) - sy).max(0.0);
                        if sw >= 1.0 && sh >= 1.0 {
                            pass.set_scissor_rect(sx as u32, sy as u32, sw as u32, sh as u32);
                            pass.draw(0..6, start..total);
                            pass.set_scissor_rect(0, 0, self.config.width, self.config.height);
                        } else {
                            pass.draw(0..6, start..total);
                        }
                    }
                    _ => pass.draw(0..6, 0..total),
                }
            }
        }

        self.queue.submit(std::iter::once(encoder.finish()));
        frame.present();
        // hand the buffer back so its capacity is reused next frame
        self.scratch = instances;
        Ok(())
    }

    /// dev capture harness: a surfaceless renderer that draws into an offscreen
    /// texture so the full chrome (tab strip, buttons, menus, panes) can be
    /// rendered to a PNG without a window. compiled out of release
    #[cfg(debug_assertions)]
    pub fn new_headless(width: u32, height: u32, content_pt: f32, chrome_pt: f32, scale: f32) -> Renderer {
        let mut desc = wgpu::InstanceDescriptor::new_without_display_handle_from_env();
        desc.backends = wgpu::Backends::all();
        let instance = wgpu::Instance::new(desc);
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::LowPower,
            compatible_surface: None,
            force_fallback_adapter: false,
        }))
        .expect("no gpu adapter for headless render");
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("termie-headless"),
            required_features: wgpu::Features::empty(),
            required_limits: wgpu::Limits::default(),
            ..Default::default()
        }))
        .expect("headless device");

        let format = wgpu::TextureFormat::Rgba8UnormSrgb;
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width: width.max(1),
            height: height.max(1),
            present_mode: wgpu::PresentMode::Fifo,
            desired_maximum_frame_latency: 1,
            alpha_mode: wgpu::CompositeAlphaMode::Opaque,
            view_formats: vec![],
        };
        let atlas = GlyphAtlas::new(content_pt, chrome_pt, scale, None, 1.32);
        let mut r = Self::from_parts(device, queue, None, format, config, atlas, scale, content_pt, chrome_pt, false);

        // offscreen target (COPY_SRC for readback) + a row-aligned readback buffer
        let target = r.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("offscreen-target"),
            size: wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let bpr = padded_bytes_per_row(width);
        let readback = r.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("readback"),
            size: bpr as u64 * height as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        r.offscreen = Some((target, readback));
        r
    }

    /// dev capture harness: render the scene into the offscreen target and write
    /// it to `path` as a PNG. only valid on a renderer from `new_headless`
    #[cfg(debug_assertions)]
    pub fn render_png(
        &mut self,
        panes: &[PaneView],
        focused: bool,
        maximized: bool,
        bare: bool,
        path: &str,
    ) -> std::io::Result<()> {
        use std::io::Error;
        let instances = self.build(panes, focused, maximized, 1.0, bare);
        self.upload_atlas();

        let needed = instances.len() as u64;
        if needed > self.instance_capacity {
            self.instance_capacity = (needed * 2).next_power_of_two();
            self.instance_buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("instances"),
                size: self.instance_capacity * std::mem::size_of::<Instance>() as u64,
                usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
        }
        if !instances.is_empty() {
            self.queue
                .write_buffer(&self.instance_buffer, 0, bytemuck::cast_slice(&instances));
        }
        let uniforms = Uniforms {
            screen: [self.config.width as f32, self.config.height as f32],
            _pad: [0.0, 0.0],
        };
        self.queue
            .write_buffer(&self.uniform_buffer, 0, bytemuck::bytes_of(&uniforms));

        let (width, height) = (self.config.width, self.config.height);
        let Some((target, readback)) = self.offscreen.as_ref() else {
            return Err(Error::other("render_png needs a headless renderer"));
        };
        let view = target.create_view(&wgpu::TextureViewDescriptor::default());
        let bg = self.palette.bg.to_linear_f32();
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("offscreen-encoder") });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("offscreen-pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: bg[0] as f64,
                            g: bg[1] as f64,
                            b: bg[2] as f64,
                            a: 1.0,
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                    depth_slice: None,
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            if !instances.is_empty() {
                pass.set_pipeline(&self.pipeline);
                pass.set_bind_group(0, &self.uniform_bind_group, &[]);
                pass.set_bind_group(1, &self.atlas_bind_group, &[]);
                pass.set_vertex_buffer(0, self.instance_buffer.slice(..));
                pass.draw(0..6, 0..instances.len() as u32);
            }
        }
        let bpr = padded_bytes_per_row(width);
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: target,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: readback,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(bpr),
                    rows_per_image: Some(height),
                },
            },
            wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
        );
        self.queue.submit(std::iter::once(encoder.finish()));

        // map the readback buffer (block until the gpu finishes) and un-pad rows
        let slice = readback.slice(..);
        slice.map_async(wgpu::MapMode::Read, |_| {});
        let _ = self.device.poll(wgpu::PollType::wait_indefinitely());
        let data = slice.get_mapped_range();
        let row = (width * 4) as usize;
        let mut rgba = Vec::with_capacity(row * height as usize);
        for y in 0..height as usize {
            let start = y * bpr as usize;
            rgba.extend_from_slice(&data[start..start + row]);
        }
        drop(data);
        readback.unmap();
        self.scratch = instances;
        crate::render::preview::write_png(path, width, height, &rgba)
    }
}

/// round a tightly-packed RGBA row up to wgpu's 256-byte copy alignment
#[cfg(debug_assertions)]
fn padded_bytes_per_row(width: u32) -> u32 {
    let unpadded = width * 4;
    let align = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    unpadded.div_ceil(align) * align
}

#[cfg(test)]
mod gpu_tests {
    // a surfaceless device, or None when no GPU adapter is present (e.g. CI),
    // in which case the validation tests skip rather than fail
    fn headless_device() -> Option<(wgpu::Device, wgpu::Queue)> {
        let mut desc = wgpu::InstanceDescriptor::new_without_display_handle_from_env();
        desc.backends = wgpu::Backends::all();
        let instance = wgpu::Instance::new(desc);
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::LowPower,
            compatible_surface: None,
            force_fallback_adapter: false,
        }))
        .ok()?;
        pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("headless-test-device"),
            required_features: wgpu::Features::empty(),
            required_limits: wgpu::Limits::default(),
            ..Default::default()
        }))
        .ok()
    }

    // validate shader.wgsl through naga's front-end without a window — catches
    // wgsl syntax/type errors (a malformed binding or fragment branch)
    #[test]
    fn shader_validates() {
        let Some((device, _queue)) = headless_device() else {
            return;
        };
        let scope = device.push_error_scope(wgpu::ErrorFilter::Validation);
        let _module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("cell-shader-test"),
            source: wgpu::ShaderSource::Wgsl(include_str!("shader.wgsl").into()),
        });
        let err = pollster::block_on(scope.pop());
        assert!(err.is_none(), "shader.wgsl failed validation: {err:?}");
    }

    // build the real bind group layouts + cell pipeline headlessly: catches a
    // shader/layout binding mismatch, e.g. if the color-emoji bindings 4/5 in
    // shader.wgsl and build_atlas_bgl ever drift apart
    #[test]
    fn pipeline_validates() {
        let Some((device, _queue)) = headless_device() else {
            return;
        };
        let scope = device.push_error_scope(wgpu::ErrorFilter::Validation);
        let uniform_bgl = super::build_uniform_bgl(&device);
        let atlas_bgl = super::build_atlas_bgl(&device);
        let _pipeline =
            super::build_cell_pipeline(&device, &uniform_bgl, &atlas_bgl, wgpu::TextureFormat::Bgra8UnormSrgb);
        let err = pollster::block_on(scope.pop());
        assert!(err.is_none(), "cell pipeline failed validation: {err:?}");
    }
}
