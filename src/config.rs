use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use thiserror::Error;

static NEXT_TEMP_FILE: AtomicU64 = AtomicU64::new(0);
static CONFIG_WRITE_MUTEX: Mutex<()> = Mutex::new(());

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("failed to open config at {path}")]
    Open {
        path: std::path::PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to parse config at {path}")]
    Parse {
        path: std::path::PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("failed to write config at {path}")]
    Write {
        path: std::path::PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("config at {path} was replaced, but parent-directory sync failed; crash durability is unconfirmed")]
    Durability {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to lock config at {path}")]
    Lock {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to serialize config")]
    Serialize(#[from] serde_json::Error),
    #[error("invalid config at {path}: {reason}")]
    Invalid {
        path: std::path::PathBuf,
        reason: &'static str,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct LightZone {
    /// Authoritative Entertainment channel centre; absent on legacy/manual rectangles.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hue_channel_position: Option<[f32; 3]>,
    pub channel_id: u8,
    pub name: String,
    /// Hue V2 owner device for this channel. Empty on pre-V2 configurations.
    #[serde(default)]
    pub hue_device_id: Option<String>,
    /// Zero-based gradient segment index, when the light exposes segments.
    #[serde(default)]
    pub hue_segment_index: Option<u8>,
    #[serde(default)]
    pub hue_segment_count: Option<u8>,
    /// Hue service layout points in room coordinates, for preview orientation only.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hue_positions: Vec<[f32; 3]>,
    /// Normalized coordinates: 0.0 to 1.0
    pub x_min: f32,
    pub x_max: f32,
    pub y_min: f32,
    pub y_max: f32,
}

impl LightZone {
    /// Maps 3D room coordinates [X, Y, Z] from Philips Hue Entertainment API into a 2D screen sampling box.
    /// - X: -1.0 (left wall) to +1.0 (right wall)
    /// - Y: -1.0 (behind listening seat) to +1.0 (front TV wall)
    /// - Z: -1.0 (floor) to +1.0 (ceiling)
    pub fn from_3d_position(channel_id: u8, name: &str, pos: [f32; 3]) -> Self {
        let mut zone = Self {
            hue_channel_position: Some(pos),
            channel_id,
            name: name.to_string(),
            hue_device_id: None,
            hue_segment_index: None,
            hue_segment_count: None,
            hue_positions: Vec::new(),
            x_min: 0.0,
            x_max: 0.0,
            y_min: 0.0,
            y_max: 0.0,
        };
        zone.set_depth_influence(default_depth_influence());
        zone
    }

    pub(crate) fn uses_room_sampling(&self) -> bool {
        self.hue_device_id.is_some() && self.hue_segment_count == Some(1)
    }

    /// Gradient-strip segments sit behind the TV: they read the picture edge, like Ambilight.
    pub(crate) fn uses_edge_sampling(&self) -> bool {
        self.hue_channel_position.is_some()
            && self.hue_device_id.is_some()
            && self.hue_segment_count > Some(1)
    }

    /// Point on the left, top or right picture edge that an edge-sampled segment reads.
    pub(crate) fn edge_point(&self) -> Option<(f32, f32)> {
        let pos = self
            .hue_channel_position
            .filter(|_| self.uses_edge_sampling())?;
        let (u, v) = screen_point(pos);
        Some(project_to_edge(u, v))
    }

    pub fn set_depth_influence(&mut self, influence: f32) {
        let Some(pos) = self.hue_channel_position else {
            return; // Legacy/manual rectangles have no recoverable room-depth coordinate.
        };
        if let Some(point) = self.edge_point() {
            // The edge band has a fixed size: room depth does not move a strip behind the TV.
            [self.x_min, self.x_max, self.y_min, self.y_max] = edge_band(point);
            return;
        }
        let (x, y) = screen_point(pos);
        let depth = ((1.0 - pos[1]) * 0.5).clamp(0.0, 1.0);
        let curve = depth * depth * (3.0 - 2.0 * depth);
        let span = 0.25
            * (1.0 + 0.5 * influence.clamp(0.0, 1.0) * curve)
            * if self.uses_room_sampling() { 1.5 } else { 1.0 };
        self.x_min = (x - span * 0.5).clamp(0.0, 1.0);
        self.x_max = (x + span * 0.5).clamp(0.0, 1.0);
        self.y_min = (y - span * 0.5).clamp(0.0, 1.0);
        self.y_max = (y + span * 0.5).clamp(0.0, 1.0);
    }
}

pub fn default_depth_influence() -> f32 {
    0.5
}

/// Gaussian sigma along the picture edge, as a fraction of the active side. About a tenth
/// of the side keeps neighbouring segments distinct, yet their bands still overlap.
pub(crate) const EDGE_SIGMA_ALONG: f32 = 0.08;
/// Inward decay length, as a fraction of the active side: weight exp(-d / depth). The light
/// follows the outermost content, and a centred subject past 3 depths (18 %) weighs nothing.
pub(crate) const EDGE_DEPTH: f32 = 0.06;
/// Distance under which an edge point counts as lying on an edge.
const EDGE_EPSILON: f32 = 1e-4;

/// Hue room coordinates to normalised screen coordinates: X to u, Z (height) to v.
fn screen_point(pos: [f32; 3]) -> (f32, f32) {
    (
        pos[0].clamp(-1.0, 1.0) * 0.5 + 0.5,
        0.5 - pos[2].clamp(-1.0, 1.0) * 0.5,
    )
}

/// Radial projection from the picture centre onto the left, top or right edge.
/// A ray towards the bottom edge goes to the nearer bottom corner: the strip has no bottom side.
pub(crate) fn project_to_edge(u: f32, v: f32) -> (f32, f32) {
    let (dx, dy) = (u - 0.5, v - 0.5);
    if dx.abs() < EDGE_EPSILON && dy.abs() < EDGE_EPSILON {
        return (0.5, 0.0);
    }
    if dy >= dx.abs() {
        return (if dx < 0.0 { 0.0 } else { 1.0 }, 1.0);
    }
    if dx.abs() >= dy.abs() {
        let x = if dx < 0.0 { 0.0 } else { 1.0 };
        (x, (0.5 + 0.5 * dy / dx.abs()).clamp(0.0, 1.0))
    } else {
        ((0.5 + 0.5 * dx / dy.abs()).clamp(0.0, 1.0), 0.0)
    }
}

/// Edges an edge point lies on: the x of its left or right edge, and whether it is on the top.
pub(crate) fn edge_sides(point: (f32, f32)) -> (Option<f32>, bool) {
    let side = if point.0 <= EDGE_EPSILON {
        Some(0.0)
    } else if point.0 >= 1.0 - EDGE_EPSILON {
        Some(1.0)
    } else {
        None
    };
    (side, point.1 <= EDGE_EPSILON)
}

/// Bounding rectangle [x_min, x_max, y_min, y_max] of an edge band: 3 sigma along the edge
/// on each side of the point, 3 depths inward. A corner point takes the union of both bands.
pub(crate) fn edge_band(point: (f32, f32)) -> [f32; 4] {
    let along = 3.0 * EDGE_SIGMA_ALONG;
    let deep = 3.0 * EDGE_DEPTH;
    let (side, top) = edge_sides(point);
    let mut band = [
        f32::INFINITY,
        f32::NEG_INFINITY,
        f32::INFINITY,
        f32::NEG_INFINITY,
    ];
    let mut include = |x_min: f32, x_max: f32, y_min: f32, y_max: f32| {
        band = [
            band[0].min(x_min),
            band[1].max(x_max),
            band[2].min(y_min),
            band[3].max(y_max),
        ];
    };
    if let Some(edge_x) = side {
        let (x_min, x_max) = if edge_x == 0.0 {
            (0.0, deep)
        } else {
            (1.0 - deep, 1.0)
        };
        include(x_min, x_max, point.1 - along, point.1 + along);
    }
    if top {
        include(point.0 - along, point.0 + along, 0.0, deep);
    }
    band.map(|bound| bound.clamp(0.0, 1.0))
}

#[derive(Clone, Serialize, Deserialize)]
pub struct NanoleafConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    pub ip: String,
    pub auth_token: String,
    #[serde(default = "default_nanoleaf_port")]
    pub udp_port: u16,
    /// Number of addressable LED segments along the lightstrip (default 30 for 4D strip)
    #[serde(default = "default_nanoleaf_segments")]
    pub segments: u16,
    /// Specific panel IDs retrieved from Nanoleaf layout
    #[serde(default)]
    pub panel_ids: Vec<u16>,
    #[serde(default)]
    pub alignment: NanoleafAlignment,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct NanoleafAlignment {
    #[serde(default)]
    pub start_corner: NanoleafStartCorner,
    #[serde(default)]
    pub reverse_direction: bool,
    #[serde(default)]
    #[schema(minimum = 0, maximum = 39)]
    pub perimeter_offset: u8,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "kebab-case")]
pub enum NanoleafStartCorner {
    #[default]
    BottomCenter,
    BottomLeft,
    TopLeft,
    TopRight,
    BottomRight,
}

/// What the Hue lights do when the TV powers off or goes to standby while this daemon streams.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TvOffLights {
    /// Turn the Entertainment area's lights off once our stream has stopped.
    #[default]
    Off,
    /// Leave the lights as the bridge restores them after streaming.
    Restore,
}

const REDACTED: &str = "<redacted>";

// Credentials must never reach logs or panic messages through `{:?}`.
impl std::fmt::Debug for NanoleafConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("NanoleafConfig")
            .field("enabled", &self.enabled)
            .field("ip", &self.ip)
            .field("auth_token", &format_args!("{REDACTED}"))
            .field("udp_port", &self.udp_port)
            .field("segments", &self.segments)
            .field("panel_ids", &self.panel_ids)
            .field("alignment", &self.alignment)
            .finish()
    }
}

fn default_nanoleaf_port() -> u16 {
    60222
}

fn default_nanoleaf_segments() -> u16 {
    30
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PresetTuning {
    pub brightness_multiplier: f32,
    pub saturation_boost: f32,
    pub peak_weight: f32,
    pub spatial_blend: f32,
    pub gamma: f32,
    pub noise_gate_threshold: f32,
    pub smoothing_factor: f32,
    pub rise_smoothing_factor: f32,
    pub fall_smoothing_factor: f32,
    pub strict_blackout: bool,
    pub max_color_step: u8,
    pub use_xy_gamut: bool,
    pub letterbox_detection: bool,
    pub hdr_tone_mapping: bool,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub active_preset: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub preset_overrides: BTreeMap<String, PresetTuning>,
    /// Advertise the dashboard and OpenAPI URL over Bonjour.
    #[serde(default)]
    pub bonjour_enabled: bool,
    /// Automatically pause/resume outputs with the TV's active/standby state.
    #[serde(default = "default_true")]
    pub auto_tv_power: bool,
    /// Hue lights after a TV standby or a system shutdown that stops our stream.
    #[serde(default)]
    pub tv_off_lights: TvOffLights,
    #[serde(default = "default_true")]
    pub hue_enabled: bool,
    /// Runtime preference: keep Hue configured but release its entertainment area when off.
    #[serde(default = "default_true")]
    pub hue_sync_enabled: bool,
    pub bridge_ip: String,
    pub username: String,
    pub clientkey: String,
    /// Stable identity checked against the Hue-signed certificate, independent of IP/renewal.
    #[serde(default)]
    pub hue_bridge_id: Option<String>,
    /// SHA-256 fingerprint of the bridge's local V2 HTTPS certificate.
    #[serde(default)]
    pub hue_bridge_certificate_sha256: Option<String>,
    /// Deprecated V1 selector retained only to load existing configurations.
    /// New configurations store the V2 UUID here as well as in `entertainment_configuration_id`.
    pub entertainment_area_id: String,
    /// V2 entertainment configuration UUID embedded in HueStream packets.
    #[serde(default)]
    pub entertainment_configuration_id: Option<String>,
    #[serde(default)]
    pub nanoleaf: Option<NanoleafConfig>,
    /// Runtime preference: stop sending Nanoleaf UDP frames when off.
    #[serde(default = "default_true")]
    pub nanoleaf_sync_enabled: bool,
    #[serde(default = "default_fps")]
    pub fps: u32,
    #[serde(default = "default_brightness")]
    pub brightness_multiplier: f32,
    /// Independent final output trim for Hue Entertainment lights.
    #[serde(default = "default_output_trim")]
    pub hue_output_brightness: f32,
    /// Independent final output trim for the Nanoleaf 4D perimeter.
    #[serde(default = "default_output_trim")]
    pub nanoleaf_output_brightness: f32,
    #[serde(default = "default_false")]
    pub use_xy_gamut: bool,
    #[serde(default = "default_true")]
    pub hdr_tone_mapping: bool,
    #[serde(default = "default_true")]
    pub letterbox_detection: bool,
    #[serde(default = "default_saturation_boost")]
    pub saturation_boost: f32,
    #[serde(default = "default_peak_weight")]
    pub peak_weight: f32,
    /// Nearby-zone gradient blending, 0 (off) to 0.5.
    #[serde(default)]
    pub spatial_blend: f32,
    /// Hue sampling-footprint expansion; independent of colour presets and neighbour blending.
    #[serde(default = "default_depth_influence")]
    pub depth_influence: f32,
    #[serde(default = "default_gamma")]
    pub gamma: f32,
    #[serde(default = "default_noise_gate")]
    pub noise_gate_threshold: f32,
    #[serde(default = "default_smoothing_factor")]
    pub smoothing_factor: f32,
    #[serde(default = "default_smoothing_factor")]
    pub rise_smoothing_factor: f32,
    #[serde(default = "default_smoothing_factor")]
    pub fall_smoothing_factor: f32,
    /// When a sampled region passes the black gate, turn that output fully off without afterglow.
    #[serde(default)]
    pub strict_blackout: bool,
    /// Whole-screen black hold and fade; both zero disables protection.
    #[serde(default = "default_black_frame_hold_ms")]
    pub black_frame_hold_ms: u32,
    #[serde(default = "default_black_frame_fade_ms")]
    pub black_frame_fade_ms: u32,
    /// Maximum per-component RGB change per reference frame at 30 FPS.
    #[serde(default = "default_max_color_step")]
    pub max_color_step: u8,
    #[serde(default = "default_true")]
    pub adaptive_throttling: bool,
    #[serde(default = "default_zones")]
    pub zones: Vec<LightZone>,
    #[serde(default = "default_capture_width")]
    pub capture_width: u32,
    #[serde(default = "default_capture_height")]
    pub capture_height: u32,
}

fn default_capture_width() -> u32 {
    320
}

fn default_capture_height() -> u32 {
    180
}

fn default_fps() -> u32 {
    30
}

fn default_brightness() -> f32 {
    1.0
}

fn default_output_trim() -> f32 {
    1.0
}

fn default_true() -> bool {
    true
}

fn default_false() -> bool {
    false
}

fn default_saturation_boost() -> f32 {
    1.5
}

fn default_peak_weight() -> f32 {
    0.35
}

fn default_gamma() -> f32 {
    1.0
}

fn default_noise_gate() -> f32 {
    0.02
}

pub fn default_black_frame_hold_ms() -> u32 {
    750
}

pub fn default_black_frame_fade_ms() -> u32 {
    2000
}

fn default_smoothing_factor() -> f32 {
    0.35
}

fn default_max_color_step() -> u8 {
    12
}

fn default_zones() -> Vec<LightZone> {
    vec![
        LightZone {
            hue_channel_position: None,
            channel_id: 0,
            name: "Left".to_string(),
            hue_device_id: None,
            hue_segment_index: None,
            hue_segment_count: None,
            hue_positions: Vec::new(),
            x_min: 0.0,
            x_max: 0.25,
            y_min: 0.1,
            y_max: 0.9,
        },
        LightZone {
            hue_channel_position: None,
            channel_id: 1,
            name: "Top".to_string(),
            hue_device_id: None,
            hue_segment_index: None,
            hue_segment_count: None,
            hue_positions: Vec::new(),
            x_min: 0.2,
            x_max: 0.8,
            y_min: 0.0,
            y_max: 0.3,
        },
        LightZone {
            hue_channel_position: None,
            channel_id: 2,
            name: "Right".to_string(),
            hue_device_id: None,
            hue_segment_index: None,
            hue_segment_count: None,
            hue_positions: Vec::new(),
            x_min: 0.75,
            x_max: 1.0,
            y_min: 0.1,
            y_max: 0.9,
        },
        LightZone {
            hue_channel_position: None,
            channel_id: 3,
            name: "Bottom".to_string(),
            hue_device_id: None,
            hue_segment_index: None,
            hue_segment_count: None,
            hue_positions: Vec::new(),
            x_min: 0.2,
            x_max: 0.8,
            y_min: 0.7,
            y_max: 1.0,
        },
    ]
}

impl Config {
    pub fn apply_hue_pairing(&mut self, bridge_ip: String, paired: crate::hue::PairedBridge) {
        self.bridge_ip = bridge_ip;
        self.username = paired.username;
        self.clientkey = paired.clientkey;
        self.hue_bridge_id = Some(paired.bridge_id);
        self.hue_bridge_certificate_sha256 = Some(paired.certificate_sha256);
        self.hue_enabled = true;
        self.hue_sync_enabled = true;
        self.entertainment_configuration_id = paired
            .area
            .as_ref()
            .map(|area| area.configuration_id.clone());
        self.entertainment_area_id = self
            .entertainment_configuration_id
            .clone()
            .unwrap_or_default();
        self.zones = paired.area.map(|area| area.zones).unwrap_or_default();
    }

    /// The fields a Hue bridge sync persists, compared to skip an unchanged write.
    pub fn hue_area(&self) -> (&str, Option<&str>, Option<&str>, Option<&str>, &[LightZone]) {
        (
            &self.entertainment_area_id,
            self.entertainment_configuration_id.as_deref(),
            self.hue_bridge_id.as_deref(),
            self.hue_bridge_certificate_sha256.as_deref(),
            &self.zones,
        )
    }

    pub fn hue_trust(&self) -> crate::hue::BridgeTrust<'_> {
        crate::hue::BridgeTrust {
            bridge_id: self.hue_bridge_id.as_deref(),
            certificate_sha256: self.hue_bridge_certificate_sha256.as_deref(),
        }
    }

    fn validate_capture_dimensions(&self, path: &Path) -> Result<(), ConfigError> {
        // Bounds both native capture requests and MockCapture's u32 RGBA allocation math.
        if self.capture_width == 0
            || self.capture_height == 0
            || self.capture_width > 4096
            || self.capture_height > 4096
            || self.capture_width as u64 * self.capture_height as u64 > 3840 * 2160
        {
            return Err(ConfigError::Invalid {
                path: path.to_path_buf(),
                reason: "capture dimensions must fit within 4096 per side and 4K pixels",
            });
        }
        Ok(())
    }

    pub fn new_default(bridge_ip: &str, username: &str, clientkey: &str, area_id: &str) -> Self {
        Self {
            active_preset: Some("neutral".into()),
            preset_overrides: BTreeMap::new(),
            bonjour_enabled: false,
            auto_tv_power: true,
            tv_off_lights: TvOffLights::default(),
            hue_enabled: true,
            hue_sync_enabled: true,
            bridge_ip: bridge_ip.to_string(),
            username: username.to_string(),
            clientkey: clientkey.to_string(),
            hue_bridge_id: None,
            hue_bridge_certificate_sha256: None,
            entertainment_area_id: area_id.to_string(),
            entertainment_configuration_id: None,
            nanoleaf: None,
            nanoleaf_sync_enabled: true,
            fps: default_fps(),
            brightness_multiplier: 1.0,
            hue_output_brightness: default_output_trim(),
            nanoleaf_output_brightness: default_output_trim(),
            use_xy_gamut: false,
            hdr_tone_mapping: false,
            letterbox_detection: true,
            saturation_boost: 1.0,
            peak_weight: 0.15,
            spatial_blend: 0.15,
            depth_influence: default_depth_influence(),
            gamma: default_gamma(),
            noise_gate_threshold: 0.02,
            smoothing_factor: 0.35,
            rise_smoothing_factor: 0.35,
            fall_smoothing_factor: 0.35,
            strict_blackout: false,
            black_frame_hold_ms: default_black_frame_hold_ms(),
            black_frame_fade_ms: default_black_frame_fade_ms(),
            max_color_step: 12,
            adaptive_throttling: true,
            zones: default_zones(),
            capture_width: default_capture_width(),
            capture_height: default_capture_height(),
        }
    }

    pub fn load<P: AsRef<Path>>(path: P) -> Result<Self, ConfigError> {
        let path = path.as_ref();
        let file = File::open(path).map_err(|source| ConfigError::Open {
            path: path.to_path_buf(),
            source,
        })?;
        let config: Config =
            serde_json::from_reader(file).map_err(|source| ConfigError::Parse {
                path: path.to_path_buf(),
                source,
            })?;
        config.validate_capture_dimensions(path)?;
        Ok(config)
    }

    /// Read, mutate, and atomically save under a lock shared by cooperating writers.
    /// `on_missing` is used only when the config file does not exist.
    pub fn update<P: AsRef<Path>>(
        path: P,
        on_missing: Option<Self>,
        mutate: impl FnOnce(&mut Self) -> anyhow::Result<()>,
    ) -> anyhow::Result<Self> {
        Self::update_if_changed(path, on_missing, |config| mutate(config).map(|()| true))
            .map(|(config, _)| config)
    }

    /// Like `update`, but `mutate` reports whether it changed anything; an unchanged result
    /// skips the write, so a repeated sync does not wear the TV's flash.
    pub fn update_if_changed<P: AsRef<Path>>(
        path: P,
        on_missing: Option<Self>,
        mutate: impl FnOnce(&mut Self) -> anyhow::Result<bool>,
    ) -> anyhow::Result<(Self, bool)> {
        let path = path.as_ref();
        let _process_lock = CONFIG_WRITE_MUTEX.lock().map_err(|_| ConfigError::Lock {
            path: path.to_path_buf(),
            source: std::io::Error::other("config write mutex poisoned"),
        })?;
        let _file_lock = lock_config(path)?;
        let mut config = match (Self::load(path), on_missing) {
            (Ok(config), _) => config,
            (Err(ConfigError::Open { source, .. }), Some(default))
                if source.kind() == std::io::ErrorKind::NotFound =>
            {
                default
            }
            (Err(error), _) => return Err(error.into()),
        };
        let changed = mutate(&mut config)?;
        if changed {
            config.save_unlocked(path)?;
        }
        Ok((config, changed))
    }

    /// Removes `save_unlocked` temporary files that a killed writer left next to the config.
    /// Writers hold the config lock while their temporary file exists, so this takes it too.
    pub fn remove_stale_temp_files<P: AsRef<Path>>(path: P) -> Result<usize, ConfigError> {
        let path = path.as_ref();
        let _process_lock = CONFIG_WRITE_MUTEX.lock().map_err(|_| ConfigError::Lock {
            path: path.to_path_buf(),
            source: std::io::Error::other("config write mutex poisoned"),
        })?;
        let _file_lock = lock_config(path)?;
        let dir = config_dir(path);
        let entries = fs::read_dir(dir).map_err(|source| ConfigError::Open {
            path: dir.to_path_buf(),
            source,
        })?;
        let mut removed = 0;
        for entry in entries.flatten() {
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            // Only regular files: a planted symlink or directory is left alone.
            let regular = fs::symlink_metadata(entry.path()).is_ok_and(|meta| meta.is_file());
            if regular && is_temp_file_name(path, &name) && fs::remove_file(entry.path()).is_ok() {
                removed += 1;
            }
        }
        Ok(removed)
    }

    fn save_unlocked(&self, path: &Path) -> Result<(), ConfigError> {
        self.validate_capture_dimensions(path)?;
        let temp_path = path.with_extension(format!(
            "new-{}-{}",
            std::process::id(),
            NEXT_TEMP_FILE.fetch_add(1, Ordering::Relaxed)
        ));
        let json = serde_json::to_string_pretty(self)?;
        // Every write is an fsync on the TV's flash; identical bytes would wear it for nothing.
        if fs::read(path).is_ok_and(|current| current == json.as_bytes()) {
            return Ok(());
        }
        let mut options = OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options
            .open(&temp_path)
            .map_err(|source| ConfigError::Write {
                path: temp_path.clone(),
                source,
            })?;
        let result = (|| {
            file.write_all(json.as_bytes())
                .map_err(|source| ConfigError::Write {
                    path: temp_path.clone(),
                    source,
                })?;
            file.sync_all().map_err(|source| ConfigError::Write {
                path: temp_path.clone(),
                source,
            })?;
            fs::rename(&temp_path, path).map_err(|source| ConfigError::Write {
                path: path.to_path_buf(),
                source,
            })
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temp_path);
        }
        result?;
        // The rename is durable only once the directory entry itself reaches the flash.
        sync_config_directory(path)
    }
}

// The serialised form lists every field, including future ones; only the credentials are masked.
impl std::fmt::Debug for Config {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut value = serde_json::to_value(self).map_err(|_| std::fmt::Error)?;
        for pointer in ["/username", "/clientkey", "/nanoleaf/auth_token"] {
            if let Some(secret) = value.pointer_mut(pointer) {
                *secret = REDACTED.into();
            }
        }
        write!(formatter, "Config {value}")
    }
}

/// Matches `save_unlocked` temporary names: `<stem>.new-<pid>-<counter>`.
fn is_temp_file_name(config_path: &Path, name: &str) -> bool {
    let Some(stem) = config_path.file_stem().and_then(|stem| stem.to_str()) else {
        return false;
    };
    name.strip_prefix(stem)
        .and_then(|rest| rest.strip_prefix(".new-"))
        .and_then(|rest| rest.split_once('-'))
        .is_some_and(|(pid, counter)| {
            [pid, counter]
                .iter()
                .all(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()))
        })
}

fn config_dir(path: &Path) -> &Path {
    match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    }
}

fn sync_config_directory(path: &Path) -> Result<(), ConfigError> {
    File::open(config_dir(path))
        .and_then(|directory| directory.sync_all())
        .map_err(|source| ConfigError::Durability {
            path: path.to_path_buf(),
            source,
        })
}

fn lock_config(path: &Path) -> Result<File, ConfigError> {
    let mut name: OsString = path.as_os_str().to_os_string();
    name.push(".lock");
    let lock_path = PathBuf::from(name);
    let mut options = OpenOptions::new();
    options.create(true).read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        options.mode(0o600);
        options.custom_flags(libc::O_NOFOLLOW);
        let file = options
            .open(&lock_path)
            .map_err(|source| ConfigError::Lock {
                path: lock_path.clone(),
                source,
            })?;
        file.set_permissions(fs::Permissions::from_mode(0o600))
            .map_err(|source| ConfigError::Lock {
                path: lock_path.clone(),
                source,
            })?;
        loop {
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } == 0 {
                return Ok(file);
            }
            let source = std::io::Error::last_os_error();
            if source.kind() != std::io::ErrorKind::Interrupted {
                return Err(ConfigError::Lock {
                    path: lock_path,
                    source,
                });
            }
        }
    }
    #[cfg(not(unix))]
    {
        options
            .open(&lock_path)
            .map_err(|source| ConfigError::Lock {
                path: lock_path,
                source,
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_output_redacts_hue_and_nanoleaf_credentials() {
        let mut config = Config::new_default(
            "192.0.2.10",
            "fixture-application-key",
            "fixture-client-key",
            "area",
        );
        config.nanoleaf = Some(NanoleafConfig {
            enabled: true,
            ip: "192.0.2.20".into(),
            auth_token: "fixtureNanoleafToken".into(),
            udp_port: 60222,
            segments: 30,
            panel_ids: vec![1, 2],
            alignment: NanoleafAlignment::default(),
        });
        let debug = format!("{config:?}");
        for secret in [
            "fixture-application-key",
            "fixture-client-key",
            "fixtureNanoleafToken",
        ] {
            assert!(!debug.contains(secret), "{secret} leaked in {debug}");
        }
        assert!(debug.contains("192.0.2.10") && debug.contains("192.0.2.20"));
        assert_eq!(debug.matches(REDACTED).count(), 3);
        let nanoleaf = format!("{:?}", config.nanoleaf.unwrap());
        assert!(!nanoleaf.contains("fixtureNanoleafToken") && nanoleaf.contains(REDACTED));
    }

    #[test]
    fn startup_cleanup_removes_only_stale_config_temp_files() {
        let dir = std::env::temp_dir().join(format!(
            "lg-hue-sync-temp-cleanup-{}-{}",
            std::process::id(),
            NEXT_TEMP_FILE.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        Config::update(&path, Some(Config::new_default("", "", "", "")), |_| Ok(())).unwrap();
        let stale = ["config.new-123-0", "config.new-4567-89"];
        let kept = [
            "config.new-abc-1",
            "config.new-12",
            "config.new--1",
            "other.new-1-2",
            "config.json.bak",
        ];
        for name in stale.iter().chain(&kept) {
            fs::write(dir.join(name), b"{}").unwrap();
        }
        fs::create_dir(dir.join("config.new-9-9")).unwrap();
        assert_eq!(Config::remove_stale_temp_files(&path).unwrap(), stale.len());
        for name in stale {
            assert!(!dir.join(name).exists(), "{name} survived");
        }
        for name in kept.iter().chain(&["config.json", "config.new-9-9"]) {
            assert!(dir.join(name).exists(), "{name} was removed");
        }
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn tv_off_lights_defaults_to_off_and_accepts_restore() {
        let mut value = serde_json::to_value(Config::new_default("", "", "", "")).unwrap();
        assert_eq!(value["tv_off_lights"], "off");
        value.as_object_mut().unwrap().remove("tv_off_lights");
        let config: Config = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(config.tv_off_lights, TvOffLights::Off);
        value["tv_off_lights"] = serde_json::json!("restore");
        let config: Config = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(config.tv_off_lights, TvOffLights::Restore);
        value["tv_off_lights"] = serde_json::json!("dim");
        assert!(serde_json::from_value::<Config>(value).is_err());
    }

    #[test]
    fn directory_sync_handles_relative_paths_and_reports_uncertain_durability() {
        sync_config_directory(Path::new("config.json")).unwrap();
        let path = std::env::temp_dir()
            .join(format!(
                "lg-hue-sync-missing-parent-{}-{}",
                std::process::id(),
                NEXT_TEMP_FILE.fetch_add(1, Ordering::Relaxed)
            ))
            .join("config.json");
        let error = sync_config_directory(&path).unwrap_err();
        assert!(matches!(
            &error,
            ConfigError::Durability { path: failed, source }
                if failed == &path && source.kind() == std::io::ErrorKind::NotFound
        ));
        assert!(error.to_string().contains("was replaced"));
        assert!(error.to_string().contains("durability is unconfirmed"));
    }

    #[test]
    fn legacy_configs_start_without_preset_overrides() {
        let mut value = serde_json::to_value(Config::new_default("", "", "", "")).unwrap();
        value.as_object_mut().unwrap().remove("active_preset");
        value.as_object_mut().unwrap().remove("preset_overrides");
        let config: Config = serde_json::from_value(value).unwrap();
        assert!(config.active_preset.is_none() && config.preset_overrides.is_empty());
        assert!(serde_json::to_value(config)
            .unwrap()
            .get("preset_overrides")
            .is_none());
    }

    #[test]
    fn preset_overrides_round_trip_without_replacing_pairing_or_output_settings() {
        let path = std::env::temp_dir().join(format!(
            "lg-hue-sync-preset-config-{}-{}.json",
            std::process::id(),
            NEXT_TEMP_FILE.fetch_add(1, Ordering::Relaxed)
        ));
        let mut initial = Config::new_default(
            "fixture-bridge",
            "fixture-key",
            "fixture-client-key",
            "fixture-area",
        );
        initial.hue_output_brightness = 0.75;
        initial.nanoleaf_output_brightness = 1.25;
        initial.bonjour_enabled = true;
        Config::update(&path, Some(initial), |_| Ok(())).unwrap();
        let tuning = PresetTuning {
            brightness_multiplier: 1.2,
            saturation_boost: 1.5,
            peak_weight: 0.25,
            spatial_blend: 0.1,
            gamma: 1.1,
            noise_gate_threshold: 0.025,
            smoothing_factor: 0.35,
            rise_smoothing_factor: 0.4,
            fall_smoothing_factor: 0.2,
            strict_blackout: true,
            max_color_step: 14,
            use_xy_gamut: false,
            letterbox_detection: true,
            hdr_tone_mapping: false,
        };
        Config::update(&path, None, |config| {
            config.active_preset = Some("neutralReference".into());
            config
                .preset_overrides
                .insert("neutralReference".into(), tuning.clone());
            Ok(())
        })
        .unwrap();
        let loaded = Config::load(&path).unwrap();
        assert_eq!(loaded.active_preset.as_deref(), Some("neutralReference"));
        assert_eq!(
            loaded.preset_overrides.get("neutralReference"),
            Some(&tuning)
        );
        assert_eq!(loaded.username, "fixture-key");
        assert_eq!(loaded.clientkey, "fixture-client-key");
        assert_eq!(loaded.bridge_ip, "fixture-bridge");
        assert_eq!(loaded.entertainment_area_id, "fixture-area");
        assert_eq!(
            (
                loaded.hue_output_brightness,
                loaded.nanoleaf_output_brightness
            ),
            (0.75, 1.25)
        );
        assert!(loaded.bonjour_enabled);
        let _ = fs::remove_file(&path);
        let _ = fs::remove_file(path.with_extension("json.lock"));
    }

    #[test]
    fn pairing_without_an_area_keeps_credentials_but_clears_old_mapping() {
        let mut config = Config::new_default("old", "old", "old", "old-area");
        let paired = crate::hue::PairedBridge {
            username: "test-application-key".into(),
            clientkey: "00".repeat(16),
            area: None,
            bridge_id: "001788fffe000001".into(),
            certificate_sha256: "aa".repeat(32),
        };
        config.apply_hue_pairing("192.0.2.1".into(), paired);
        assert_eq!(config.username, "test-application-key");
        assert_eq!(config.clientkey, "00".repeat(16));
        assert_eq!(config.hue_bridge_id.as_deref(), Some("001788fffe000001"));
        assert!(config.hue_enabled && config.hue_sync_enabled);
        assert!(config.entertainment_configuration_id.is_none());
        assert!(config.entertainment_area_id.is_empty() && config.zones.is_empty());
    }

    #[test]
    fn bonjour_is_opt_in_for_new_and_existing_configs() {
        let config = Config::new_default("", "", "", "");
        assert!(!config.bonjour_enabled);
        let mut value = serde_json::to_value(config).unwrap();
        value.as_object_mut().unwrap().remove("bonjour_enabled");
        assert!(
            !serde_json::from_value::<Config>(value.clone())
                .unwrap()
                .bonjour_enabled
        );
        value["bonjour_enabled"] = true.into();
        assert!(
            serde_json::from_value::<Config>(value)
                .unwrap()
                .bonjour_enabled
        );
    }

    #[test]
    fn new_configs_disable_unverified_hdr_curve_without_changing_legacy_missing_field() {
        let config = Config::new_default("", "", "", "");
        assert!(!config.hdr_tone_mapping);

        let mut legacy = serde_json::to_value(config).unwrap();
        legacy.as_object_mut().unwrap().remove("hdr_tone_mapping");
        let loaded: Config = serde_json::from_value(legacy).unwrap();
        assert!(loaded.hdr_tone_mapping);
    }

    #[test]
    fn rejects_capture_dimensions_that_cannot_safely_allocate_rgba() {
        let path = std::env::temp_dir().join(format!(
            "lg-hue-sync-config-validation-{}-{}.json",
            std::process::id(),
            NEXT_TEMP_FILE.fetch_add(1, Ordering::Relaxed)
        ));
        let mut config = Config::new_default("", "", "", "");
        for (width, height) in [(0, 180), (320, 0), (u32::MAX, 180), (4096, 4096)] {
            config.capture_width = width;
            config.capture_height = height;
            fs::write(&path, serde_json::to_vec(&config).unwrap()).unwrap();
            assert!(matches!(
                Config::load(&path),
                Err(ConfigError::Invalid { .. })
            ));
            fs::remove_file(&path).unwrap();
            assert!(Config::update(&path, Some(config.clone()), |_| Ok(())).is_err());
        }
        fs::remove_file(format!("{}.lock", path.display())).unwrap();
    }

    #[test]
    fn concurrent_updates_produce_valid_config() {
        let path = std::env::temp_dir().join(format!(
            "lg-hue-sync-concurrent-save-{}-{}.json",
            std::process::id(),
            NEXT_TEMP_FILE.fetch_add(1, Ordering::Relaxed)
        ));
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(4));
        let threads: Vec<_> = (0..4)
            .map(|index| {
                let path = path.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    Config::update(&path, Some(Config::new_default("", "", "", "")), |config| {
                        config.gamma = 1.0 + index as f32 * 0.1;
                        Ok(())
                    })
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap().unwrap();
        }
        Config::load(&path).unwrap();
        fs::remove_file(&path).unwrap();
        fs::remove_file(format!("{}.lock", path.display())).unwrap();
    }

    #[test]
    fn concurrent_updates_preserve_disjoint_fields() {
        let path = std::env::temp_dir().join(format!(
            "lg-hue-sync-concurrent-update-{}-{}.json",
            std::process::id(),
            NEXT_TEMP_FILE.fetch_add(1, Ordering::Relaxed)
        ));
        let mut initial = Config::new_default("", "", "", "");
        initial.capture_width = 320;
        initial.capture_height = 180;
        Config::update(&path, Some(initial), |_| Ok(())).unwrap();

        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let threads: Vec<_> = (0..2)
            .map(|field| {
                let path = path.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    for _ in 0..50 {
                        Config::update(&path, None, |config| {
                            if field == 0 {
                                config.capture_width += 1;
                            } else {
                                config.capture_height += 1;
                            }
                            Ok(())
                        })?;
                    }
                    Ok::<_, anyhow::Error>(())
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap().unwrap();
        }
        let saved = Config::load(&path).unwrap();
        assert_eq!((saved.capture_width, saved.capture_height), (370, 230));
        fs::remove_file(&path).unwrap();
        fs::remove_file(format!("{}.lock", path.display())).unwrap();
    }

    #[test]
    fn unchanged_hue_area_is_detected_and_skips_the_write() {
        let saved = Config::new_default("192.0.2.1", "user", "key", "area");
        let mut synced = saved.clone();
        assert!(saved.hue_area() == synced.hue_area());
        synced.zones[0].name.push_str(" moved");
        assert!(saved.hue_area() != synced.hue_area());
        let mut synced = saved.clone();
        synced.entertainment_configuration_id = Some("other".into());
        assert!(saved.hue_area() != synced.hue_area());

        // A missing file stays missing when nothing changed: no write happened.
        let path = std::env::temp_dir().join(format!(
            "lg-hue-sync-unchanged-area-{}-{}.json",
            std::process::id(),
            NEXT_TEMP_FILE.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_file(&path);
        let (_, changed) =
            Config::update_if_changed(&path, Some(saved.clone()), |_| Ok(false)).unwrap();
        assert!(!changed && !path.exists());
        let (_, changed) = Config::update_if_changed(&path, Some(saved), |_| Ok(true)).unwrap();
        assert!(changed && path.exists());
        let _ = std::fs::remove_file(&path);
        let mut lock = path.into_os_string();
        lock.push(".lock");
        let _ = std::fs::remove_file(lock);
    }

    #[cfg(unix)]
    #[test]
    fn identical_serialised_config_is_not_rewritten() {
        use std::os::unix::fs::MetadataExt;
        let path = std::env::temp_dir().join(format!(
            "lg-hue-sync-identical-{}-{}.json",
            std::process::id(),
            NEXT_TEMP_FILE.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_file(&path);
        let initial = Config::new_default("", "", "", "");
        Config::update(&path, Some(initial), |_| Ok(())).unwrap();
        let written = std::fs::metadata(&path).unwrap().ino();
        // `update` reports a change, but the serialised bytes equal the file: no rename.
        Config::update(&path, None, |_| Ok(())).unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().ino(), written);
        Config::update(&path, None, |config| {
            config.bonjour_enabled = !config.bonjour_enabled;
            Ok(())
        })
        .unwrap();
        assert_ne!(std::fs::metadata(&path).unwrap().ino(), written);
        let _ = std::fs::remove_file(&path);
        let mut lock = path.into_os_string();
        lock.push(".lock");
        let _ = std::fs::remove_file(lock);
    }

    #[test]
    fn legacy_per_light_trim_fields_are_ignored() {
        let config: Config = serde_json::from_str(
            r#"{"bridge_ip":"127.0.0.1","username":"user","clientkey":"key","entertainment_area_id":"area","hue_light_trims":[{"device_id":"id","output_trim":1.3}],"zones":[{"channel_id":0,"name":"Hue","output_trim":1.3,"x_min":0.0,"x_max":1.0,"y_min":0.0,"y_max":1.0}]}"#,
        )
        .unwrap();
        let saved = serde_json::to_value(config).unwrap();
        assert!(saved.get("hue_light_trims").is_none());
        assert!(saved["zones"][0].get("output_trim").is_none());
    }

    #[test]
    fn test_3d_front_light_has_tight_directional_span() {
        // Front Left light right near the TV wall (Y = 1.0)
        let zone = LightZone::from_3d_position(0, "Front Left", [-0.8, 1.0, 0.0]);
        let span_x = zone.x_max - zone.x_min;
        let span_y = zone.y_max - zone.y_min;

        // Front lights should have tight directional focus (~0.25)
        assert!(
            (span_x - 0.25).abs() < 0.05,
            "Front light span_x should be tight ~0.25, got {}",
            span_x
        );
        assert!(
            (span_y - 0.25).abs() < 0.05,
            "Front light span_y should be tight ~0.25, got {}",
            span_y
        );
        assert!(
            zone.x_min < 0.2,
            "Front left should be on left edge of screen"
        );
    }

    #[test]
    fn depth_expansion_is_bounded_symmetric_and_preserves_channel_metadata() {
        let mut zone = LightZone::from_3d_position(7, "Gradient", [0.0, -1.0, 0.0]);
        zone.hue_segment_index = Some(3);
        zone.hue_segment_count = Some(7);
        // Without a V2 owner device the segment keeps the depth-scaled square.
        for (influence, expected) in [(0.0, 0.25), (0.5, 0.3125), (1.0, 0.375)] {
            zone.set_depth_influence(influence);
            assert!((zone.x_max - zone.x_min - expected).abs() < 1e-6);
            assert!((zone.y_max - zone.y_min - expected).abs() < 1e-6);
            assert!((zone.x_min + zone.x_max - 1.0).abs() < 1e-6);
        }
        // An edge band does not grow with depth influence.
        zone.hue_device_id = Some("device".into());
        zone.set_depth_influence(0.0);
        let band = [zone.x_min, zone.x_max, zone.y_min, zone.y_max];
        zone.set_depth_influence(1.0);
        assert_eq!(band, [zone.x_min, zone.x_max, zone.y_min, zone.y_max]);
        assert_eq!(
            (
                zone.channel_id,
                zone.hue_segment_index,
                zone.hue_segment_count
            ),
            (7, Some(3), Some(7))
        );
        assert_eq!(zone.hue_device_id.as_deref(), Some("device"));
        let mut left = LightZone::from_3d_position(1, "Left", [-0.9, -0.4, 0.3]);
        let mut right = LightZone::from_3d_position(2, "Right", [0.9, -0.4, 0.3]);
        left.set_depth_influence(1.0);
        right.set_depth_influence(1.0);
        assert!((left.x_max - left.x_min - (right.x_max - right.x_min)).abs() < 1e-6);
        let mut previous = 0.25;
        for y in [1.0, 0.5, 0.0, -0.5, -1.0] {
            let mut point = LightZone::from_3d_position(0, "", [0.0, y, 0.0]);
            point.set_depth_influence(1.0);
            let span = point.x_max - point.x_min;
            assert!(span >= previous && span <= 0.375);
            previous = span;
        }
        let mut legacy = zone.clone();
        legacy.hue_channel_position = None;
        let bounds = [legacy.x_min, legacy.x_max, legacy.y_min, legacy.y_max];
        legacy.set_depth_influence(0.0);
        assert_eq!(
            bounds,
            [legacy.x_min, legacy.x_max, legacy.y_min, legacy.y_max]
        );
        let mut json = serde_json::to_value(Config::new_default("", "", "", "")).unwrap();
        json.as_object_mut().unwrap().remove("depth_influence");
        let restored: Config = serde_json::from_value(json).unwrap();
        assert_eq!(restored.depth_influence, 0.5);
        let restored_zone: LightZone =
            serde_json::from_str(&serde_json::to_string(&zone).unwrap()).unwrap();
        assert_eq!(
            restored_zone.hue_channel_position,
            zone.hue_channel_position
        );
    }

    #[test]
    fn gradient_segments_project_onto_left_top_and_right_edges_in_hue_order() {
        // A ring around the picture centre, in Hue order from the lower left over the top.
        let ring: [[f32; 3]; 6] = [
            [-0.8, 0.0, -0.3],
            [-0.8, 0.0, 0.3],
            [-0.3, 0.0, 0.8],
            [0.3, 0.0, 0.8],
            [0.8, 0.0, 0.3],
            [0.8, 0.0, -0.3],
        ];
        let zones: Vec<LightZone> = ring
            .iter()
            .enumerate()
            .map(|(index, &pos)| {
                let mut zone = LightZone::from_3d_position(index as u8, "Strip", pos);
                zone.hue_device_id = Some("strip".into());
                zone.hue_segment_index = Some(index as u8);
                zone.hue_segment_count = Some(ring.len() as u8);
                zone.set_depth_influence(0.5);
                zone
            })
            .collect();
        let points: Vec<(f32, f32)> = zones.iter().map(|z| z.edge_point().unwrap()).collect();
        assert_eq!(
            zones.iter().map(|z| z.channel_id).collect::<Vec<_>>(),
            [0, 1, 2, 3, 4, 5]
        );
        // Left edge going up, top edge going right, right edge going down.
        assert!(points[0].0 == 0.0 && points[1].0 == 0.0 && points[0].1 > points[1].1);
        assert!(points[2].1 == 0.0 && points[3].1 == 0.0 && points[2].0 < points[3].0);
        assert!(points[4].0 == 1.0 && points[5].0 == 1.0 && points[4].1 < points[5].1);
        // Left and right bands hug their edge, 3 depths deep; the top band hugs the top.
        assert!(zones[0].x_min == 0.0 && (zones[0].x_max - 0.18).abs() < 1e-6);
        assert!(zones[2].y_min == 0.0 && (zones[2].y_max - 0.18).abs() < 1e-6);
        assert!(zones[5].x_max == 1.0 && (zones[5].x_min - 0.82).abs() < 1e-6);

        // The strip has no bottom side: a bottom ray goes to the nearer bottom corner.
        assert_eq!(project_to_edge(0.6, 0.95), (1.0, 1.0));
        assert_eq!(project_to_edge(0.4, 0.95), (0.0, 1.0));
        assert_eq!(project_to_edge(0.5, 0.5), (0.5, 0.0));
        let mut bottom = LightZone::from_3d_position(9, "Strip", [-0.2, 0.0, -0.9]);
        bottom.hue_device_id = Some("strip".into());
        bottom.hue_segment_count = Some(7);
        bottom.set_depth_influence(0.5);
        assert_eq!(bottom.edge_point(), Some((0.0, 1.0)));
        assert_eq!(bottom.y_max, 1.0);
        // A top corner takes the union of its top and side bands.
        let corner = edge_band((0.0, 0.0));
        for (actual, expected) in corner.into_iter().zip([0.0, 0.24, 0.0, 0.24]) {
            assert!((actual - expected).abs() < 1e-6, "{corner:?}");
        }
    }

    #[test]
    fn test_3d_height_mapping() {
        // Ceiling light (Z = 1.0): maps to top of screen (Y near 0.0)
        let ceiling = LightZone::from_3d_position(2, "Top Atmos", [0.0, 0.5, 1.0]);
        assert!(
            ceiling.y_min < 0.1,
            "Ceiling light should map to top of screen (y_min < 0.1), got {}",
            ceiling.y_min
        );

        // Floor light (Z = -1.0): maps to bottom of screen (Y near 1.0)
        let floor = LightZone::from_3d_position(3, "Floor Light", [0.0, 0.5, -1.0]);
        assert!(
            floor.y_max > 0.9,
            "Floor light should map to bottom of screen (y_max > 0.9), got {}",
            floor.y_max
        );
    }
}
