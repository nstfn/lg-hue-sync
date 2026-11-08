use crate::color::gamut::{rgb_to_xy_brightness, HueGamut};
use crate::config::{EdgeGeometry, LightZone};
use palette::{FromColor, Hsv, Mix, Srgb};

use serde::{Deserialize, Serialize};
use std::time::{Duration, Instant};

pub(crate) const REFERENCE_FRAME: Duration = Duration::from_nanos(33_333_333);
/// A scene cut may move this many times the normal step: black to white in about 5 frames.
const SCENE_CUT_STEP_MULTIPLIER: f32 = 4.0;
/// The spatial blend reaches full strength at this zone level (0..1) or at twice the noise gate.
const MIN_BLEND_FADE_LEVEL: f32 = 0.02;
/// Consecutive matching letterbox detections required before the active area changes.
const LETTERBOX_CONFIRMATIONS: u8 = 3;
/// Two detected areas match when every bound differs by at most this fraction of the frame.
const LETTERBOX_MATCH_TOLERANCE: f32 = 0.02;

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct RgbColor {
    #[schema(minimum = 0, maximum = 255)]
    pub r: u8,
    #[schema(minimum = 0, maximum = 255)]
    pub g: u8,
    #[schema(minimum = 0, maximum = 255)]
    pub b: u8,
}

impl RgbColor {
    pub fn new(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }

    /// Convert to 16-bit RGB values (0..65535) for Hue Entertainment protocol
    pub fn to_u16(self) -> (u16, u16, u16) {
        (
            ((self.r as u16) << 8) | (self.r as u16),
            ((self.g as u16) << 8) | (self.g as u16),
            ((self.b as u16) << 8) | (self.b as u16),
        )
    }

    /// Relative perceptual luminance in range [0.0, 1.0] (Rec. 709 coefficients)
    pub fn luminance(self) -> f32 {
        0.2126 * (self.r as f32 / 255.0)
            + 0.7152 * (self.g as f32 / 255.0)
            + 0.0722 * (self.b as f32 / 255.0)
    }

    /// Chroma (max - min) in range [0.0, 1.0]. Unlike (max - min) / max, near-black
    /// compression noise such as (3, 0, 1) stays close to grey instead of fully saturated.
    pub fn chroma(self) -> f32 {
        let max = self.r.max(self.g).max(self.b);
        let min = self.r.min(self.g).min(self.b);
        f32::from(max - min) / 255.0
    }

    /// Color difference metric (Euclidean distance in normalized RGB, range 0.0 to ~1.73)
    pub fn delta(self, other: RgbColor) -> f32 {
        let dr = (self.r as f32 - other.r as f32) / 255.0;
        let dg = (self.g as f32 - other.g as f32) / 255.0;
        let db = (self.b as f32 - other.b as f32) / 255.0;
        (dr * dr + dg * dg + db * db).sqrt()
    }
}

/// Fractional RGB in the existing encoded 0..255 domain; not HDR or linear light.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct FloatColor {
    pub r: f32,
    pub g: f32,
    pub b: f32,
}

impl From<RgbColor> for FloatColor {
    fn from(color: RgbColor) -> Self {
        Self::new(color.r as f32, color.g as f32, color.b as f32)
    }
}

impl FloatColor {
    pub fn new(r: f32, g: f32, b: f32) -> Self {
        Self { r, g, b }
    }

    pub fn to_rgb(self) -> RgbColor {
        RgbColor::new(
            self.r.round().clamp(0.0, 255.0) as u8,
            self.g.round().clamp(0.0, 255.0) as u8,
            self.b.round().clamp(0.0, 255.0) as u8,
        )
    }

    pub fn luminance(self) -> f32 {
        (0.2126 * self.r + 0.7152 * self.g + 0.0722 * self.b) / 255.0
    }

    pub fn scale(self, multiplier: f32) -> Self {
        Self::new(
            self.r * multiplier,
            self.g * multiplier,
            self.b * multiplier,
        )
    }

    /// Largest channel in the encoded 0..255 domain.
    pub fn max_channel(self) -> f32 {
        self.r.max(self.g).max(self.b)
    }

    /// Output trim with one factor for all channels. The factor is limited so that the
    /// largest channel stays at or below 255; per-channel clipping would shift the hue.
    pub fn trim(self, multiplier: f32) -> Self {
        let max = self.max_channel();
        let factor = if max > 0.0 {
            multiplier.max(0.0).min(255.0 / max)
        } else {
            0.0
        };
        self.scale(factor)
    }

    /// 16-bit RGB (0..65535) for the Hue Entertainment protocol, without an 8-bit step.
    pub fn to_u16(self) -> (u16, u16, u16) {
        let wide = |value: f32| (value.clamp(0.0, 255.0) * 257.0).round() as u16;
        (wide(self.r), wide(self.g), wide(self.b))
    }

    /// CIE 1931 xy and brightness as 16-bit HueStream values, clamped to the gamut.
    pub fn to_xy_u16(self, gamut: HueGamut) -> (u16, u16, u16) {
        let xy = rgb_to_xy_brightness(self.r, self.g, self.b, gamut);
        (
            (xy.x * 65535.0).clamp(0.0, 65535.0).round() as u16,
            (xy.y * 65535.0).clamp(0.0, 65535.0).round() as u16,
            (xy.brightness * 65535.0).clamp(0.0, 65535.0).round() as u16,
        )
    }

    /// Same metric as `RgbColor::delta`: Euclidean RGB distance, normalised by 255.
    pub fn delta(self, other: FloatColor) -> f32 {
        let dr = (self.r - other.r) / 255.0;
        let dg = (self.g - other.g) / 255.0;
        let db = (self.b - other.b) / 255.0;
        (dr * dr + dg * dg + db * db).sqrt()
    }

    pub fn lerp(self, target: FloatColor, alpha: f32) -> Self {
        if alpha == 0.0 {
            return self;
        }
        if alpha == 1.0 {
            return target;
        }
        Self::from_encoded(self.encoded_rgb().mix(target.encoded_rgb(), alpha))
    }

    /// Legacy midtone lift; capture transfer and HDR metadata are unavailable here.
    pub fn tone_map_hdr(self) -> Self {
        let r_f = self.r / 255.0;
        let g_f = self.g / 255.0;
        let b_f = self.b / 255.0;

        let r_tm = (r_f / (r_f + 0.25) * 1.25).clamp(0.0, 1.0);
        let g_tm = (g_f / (g_f + 0.25) * 1.25).clamp(0.0, 1.0);
        let b_tm = (b_f / (b_f + 0.25) * 1.25).clamp(0.0, 1.0);

        Self {
            r: r_tm * 255.0,
            g: g_tm * 255.0,
            b: b_tm * 255.0,
        }
    }

    /// OLED near-black noise gate on the largest channel, so saturated dark blues survive.
    /// Levels at or below the threshold become 0; the output ramps up to full by 2 * threshold.
    pub fn apply_noise_gate(self, threshold: f32) -> Self {
        let level = self.max_channel() / 255.0;
        if level <= threshold {
            Self::default()
        } else {
            let ramp = ((level - threshold) / threshold).clamp(0.0, 1.0);
            self.scale(ramp)
        }
    }

    /// Keep the established encoded RGB domain; conversion does not imply capture metadata.
    fn encoded_rgb(self) -> Srgb<f32> {
        Srgb::new(self.r / 255.0, self.g / 255.0, self.b / 255.0)
    }

    fn from_encoded(rgb: Srgb<f32>) -> Self {
        Self::new(
            rgb.red.clamp(0.0, 1.0) * 255.0,
            rgb.green.clamp(0.0, 1.0) * 255.0,
            rgb.blue.clamp(0.0, 1.0) * 255.0,
        )
    }

    /// Preserve the existing multiplicative HSV saturation control, not Palette's additive one.
    pub fn boost_saturation(self, boost: f32) -> Self {
        if boost == 1.0 {
            return self;
        }
        let mut hsv = Hsv::from_color(self.encoded_rgb());
        hsv.saturation = (hsv.saturation * boost).clamp(0.0, 1.0);
        Self::from_encoded(Srgb::from_color(hsv))
    }

    /// Applies gamma contrast expansion: C_out = C_in^gamma
    pub fn apply_gamma(self, gamma: f32) -> Self {
        if (gamma - 1.0).abs() < 0.01 {
            return self;
        }
        let r_f = (self.r / 255.0).powf(gamma).clamp(0.0, 1.0);
        let g_f = (self.g / 255.0).powf(gamma).clamp(0.0, 1.0);
        let b_f = (self.b / 255.0).powf(gamma).clamp(0.0, 1.0);
        Self::new(r_f * 255.0, g_f * 255.0, b_f * 255.0)
    }
}

#[derive(Clone)]
pub struct ColorProcessor {
    rise: f32,
    fall: f32,
    hdr_tone_mapping: bool,
    saturation_boost: f32,
    noise_gate_threshold: f32,
    peak_weight: f32,
    gamma: f32,
    max_color_step: u8,
    strict_blackout: bool,
    spatial_blend: f32,
    room_response: bool,
}

impl ColorProcessor {
    pub fn new(
        smoothing: f32,
        hdr_tone_mapping: bool,
        saturation_boost: f32,
        noise_gate_threshold: f32,
    ) -> Self {
        Self {
            rise: smoothing,
            fall: smoothing,
            hdr_tone_mapping,
            saturation_boost,
            noise_gate_threshold,
            peak_weight: 0.35,
            gamma: 1.0,
            max_color_step: 12,
            strict_blackout: false,
            spatial_blend: 0.0,
            room_response: false,
        }
    }

    pub fn set_spatial_blend(&mut self, strength: f32) {
        self.spatial_blend = strength.clamp(0.0, 0.5);
    }

    /// Blend nearby screen regions without assuming channel IDs are spatially ordered.
    /// Keep the temporal state independent of this spatial pass.
    pub fn blend_spatial(&self, colors: &mut [FloatColor], center: impl Fn(usize) -> (f32, f32)) {
        if self.spatial_blend == 0.0 {
            return;
        }
        // The blend fades in with the zone's own level: a zone leaving black must not jump
        // to its neighbours' colour, which would bypass smoothing and the step limit.
        let fade_end = (2.0 * self.noise_gate_threshold).max(MIN_BLEND_FADE_LEVEL);
        // ponytail: O(n²) over small light sets; cache neighbour weights if profiling warrants it.
        let original = colors.to_vec();
        for (i, color) in colors.iter_mut().enumerate() {
            let fade = (color.max_channel() / 255.0 / fade_end).clamp(0.0, 1.0);
            if fade == 0.0 {
                continue;
            }
            let (x, y) = center(i);
            let mut sum = FloatColor::default();
            let mut total = 0.0;
            for (j, neighbour) in original.iter().enumerate() {
                if i == j {
                    continue;
                }
                let (nx, ny) = center(j);
                let distance = ((x - nx).powi(2) + (y - ny).powi(2)).sqrt();
                let weight = (1.0 - distance / 0.35).max(0.0).powi(2);
                sum.r += neighbour.r * weight;
                sum.g += neighbour.g * weight;
                sum.b += neighbour.b * weight;
                total += weight;
            }
            if total > 0.0 {
                // The zone itself counts with weight 1, so a lone distant neighbour blends less.
                let share = total / (1.0 + total);
                let strength = self.spatial_blend * fade * share;
                *color = color.lerp(sum.scale(1.0 / total), strength);
            }
        }
    }

    fn for_room(&self) -> Self {
        Self {
            room_response: true,
            ..self.clone()
        }
    }

    /// Sampling weight from chroma (see `RgbColor::chroma`): vivid pixels count more than grey.
    pub fn sample_weight(&self, chroma: f32) -> f32 {
        1.0 + self.saturation_boost * chroma * chroma
    }

    pub fn set_temporal_response(&mut self, rise: f32, fall: f32) {
        self.rise = rise.clamp(0.05, 1.0);
        self.fall = fall.clamp(0.05, 1.0);
    }

    pub fn set_strict_blackout(&mut self, enabled: bool) {
        self.strict_blackout = enabled;
    }

    pub fn set_hdr_tone_mapping(&mut self, enabled: bool) {
        self.hdr_tone_mapping = enabled;
    }

    pub fn set_saturation_boost(&mut self, boost: f32) {
        self.saturation_boost = boost.clamp(0.5, 3.0);
    }

    pub fn set_peak_weight(&mut self, weight: f32) {
        self.peak_weight = weight.clamp(0.0, 1.0);
    }

    pub fn set_gamma(&mut self, gamma: f32) {
        self.gamma = gamma.clamp(0.5, 3.0);
    }

    pub fn set_noise_gate_threshold(&mut self, threshold: f32) {
        self.noise_gate_threshold = threshold.clamp(0.0, 0.1);
    }

    pub fn set_max_color_step(&mut self, step: u8) {
        self.max_color_step = step.max(1);
    }

    /// Shared per-colour adjustment; sampling and temporal response remain separate.
    pub fn transform_color(&self, target: FloatColor) -> FloatColor {
        self.transform_with_gate(target, self.noise_gate_threshold)
    }

    fn transform_with_gate(&self, mut target: FloatColor, threshold: f32) -> FloatColor {
        if threshold > 0.0 {
            target = target.apply_noise_gate(threshold);
        }
        if self.saturation_boost != 1.0 {
            target = target.boost_saturation(self.saturation_boost);
        }
        if (self.gamma - 1.0).abs() >= 0.01 {
            target = target.apply_gamma(self.gamma);
        }
        if self.hdr_tone_mapping {
            target = target.tone_map_hdr();
        }
        target
    }

    /// Mean and highlight blend that `process` starts from, before the gate and transforms.
    fn blend_target(&self, mean: FloatColor, peak: FloatColor, max_luma: f32) -> FloatColor {
        if self.peak_weight > 0.0 && max_luma > 0.0 {
            mean.lerp(peak, self.peak_weight)
        } else {
            mean
        }
    }

    pub fn process(
        &self,
        mean: FloatColor,
        peak: FloatColor,
        max_luma: f32,
        current: FloatColor,
        is_scene_cut: bool,
        elapsed: Duration,
    ) -> FloatColor {
        // Preserve 30 FPS tuning without treating a capture stall as one giant color jump.
        let elapsed = elapsed.min(Duration::from_millis(100));
        let mut target = self.blend_target(mean, peak, max_luma);
        // Require more light to leave black than to remain lit. Uniform black still stops.
        let threshold = self.noise_gate_threshold
            * if current == FloatColor::default() {
                1.2
            } else {
                0.8
            };
        target = self.transform_with_gate(target, threshold);
        if self.strict_blackout && target == FloatColor::default() {
            return target;
        }
        let alpha = if is_scene_cut {
            1.0
        } else if target.luminance() < current.luminance() {
            elapsed_alpha(self.fall, elapsed)
        } else {
            elapsed_alpha(self.rise, elapsed)
        };
        let mut max_step = self.max_color_step as f32 * elapsed.as_secs_f32() * 30.0;
        if is_scene_cut {
            max_step *= SCENE_CUT_STEP_MULTIPLIER;
        }
        let next = if self.room_response && !is_scene_cut {
            let brightness = |c: FloatColor| c.r.max(c.g).max(c.b);
            let from = brightness(current);
            let to = brightness(target);
            let base = if to < from { self.fall } else { self.rise };
            let slow_elapsed = elapsed.mul_f32(0.6);
            let brightness_alpha = elapsed_alpha(base, slow_elapsed);
            let colour_alpha = elapsed_alpha(base.min(0.25), slow_elapsed);
            let from_colour = if from > 0.0 {
                current.scale(1.0 / from)
            } else if to > 0.0 {
                target.scale(1.0 / to)
            } else {
                FloatColor::default()
            };
            let to_colour = if to > 0.0 {
                target.scale(1.0 / to)
            } else {
                from_colour
            };
            let colour = from_colour.lerp(to_colour, colour_alpha);
            let norm = brightness(colour);
            if norm > 0.0 {
                colour.scale((from + (to - from) * brightness_alpha) / norm)
            } else {
                FloatColor::default()
            }
        } else {
            current.lerp(target, alpha)
        };
        limit_color_step(current, next, max_step)
    }
}

fn elapsed_alpha(reference_alpha: f32, elapsed: Duration) -> f32 {
    1.0 - (1.0 - reference_alpha).powf(elapsed.as_secs_f32() * 30.0)
}

/// Active viewport bounds after detecting letterbox/pillarbox bars (normalized 0.0 to 1.0)
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ActiveRect {
    pub x_min: f32,
    pub x_max: f32,
    pub y_min: f32,
    pub y_max: f32,
}

impl Default for ActiveRect {
    fn default() -> Self {
        Self {
            x_min: 0.0,
            x_max: 1.0,
            y_min: 0.0,
            y_max: 1.0,
        }
    }
}

/// Letterbox hysteresis: adopt a new active area only after consecutive matching detections,
/// so one dark or ambiguous frame cannot crop the sampled area.
#[derive(Debug, Clone, Default)]
pub struct LetterboxTracker {
    active: ActiveRect,
    candidate: Option<ActiveRect>,
    matches: u8,
}

impl LetterboxTracker {
    pub fn active(&self) -> ActiveRect {
        self.active
    }

    /// Feed one detection (`None` = no evidence) and return the active area to sample.
    pub fn observe(&mut self, detected: Option<ActiveRect>) -> ActiveRect {
        let Some(rect) = detected else {
            return self.active;
        };
        if rects_match(rect, self.active) {
            self.candidate = None;
            self.matches = 0;
            return self.active;
        }
        match self.candidate {
            Some(candidate) if rects_match(rect, candidate) => self.matches += 1,
            _ => self.matches = 1,
        }
        self.candidate = Some(rect);
        if self.matches >= LETTERBOX_CONFIRMATIONS {
            self.active = rect;
            self.candidate = None;
            self.matches = 0;
        }
        self.active
    }
}

fn rects_match(a: ActiveRect, b: ActiveRect) -> bool {
    [
        (a.x_min, b.x_min),
        (a.x_max, b.x_max),
        (a.y_min, b.y_min),
        (a.y_max, b.y_max),
    ]
    .into_iter()
    .all(|(left, right)| (left - right).abs() <= LETTERBOX_MATCH_TOLERANCE)
}

/// Spatial weight factors of one sampled column or row (step 2), in active-area coordinates.
#[derive(Clone, Copy, Default)]
struct AxisWeight {
    /// Room Gaussian, or the along-edge Gaussian of an edge band (0 beyond 3 sigma).
    gauss: f32,
    /// Distance to the edge on this axis; infinite when the band has no edge on this axis.
    inward: f32,
    /// exp(-inward / depth), 0 beyond 3 depths.
    decay: f32,
}

impl AxisWeight {
    fn room(offset: f32, half: f32) -> Self {
        let n = offset / half.max(0.001);
        Self {
            gauss: (-2.0 * n * n).exp(),
            ..Self::default()
        }
    }

    fn edge(along: f32, inward: Option<f32>, edge: EdgeGeometry) -> Self {
        let sigma = edge.sigma_along;
        let gauss = if along.abs() <= 3.0 * sigma {
            (-along * along / (2.0 * sigma * sigma)).exp()
        } else {
            0.0
        };
        let (inward, decay) = match inward {
            Some(d) if d <= 3.0 * edge.depth => (d, (-d / edge.depth).exp()),
            Some(d) => (d, 0.0),
            None => (f32::INFINITY, 0.0),
        };
        Self {
            gauss,
            inward,
            decay,
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Footprint {
    Uniform,
    Room,
    Edge,
}

/// Precomputed pixel bounds and separable weights of one zone.
struct ZoneWeights {
    footprint: Footprint,
    x_start: u32,
    y_start: u32,
    columns: Vec<AxisWeight>,
    rows: Vec<AxisWeight>,
}

impl ZoneWeights {
    fn weight(&self, column: AxisWeight, row: AxisWeight) -> f32 {
        match self.footprint {
            Footprint::Uniform => 1.0,
            Footprint::Room => column.gauss * row.gauss,
            // A corner point lies on two edges: the nearer edge decides. On one edge,
            // the other axis has an infinite distance and never wins.
            Footprint::Edge if column.inward < row.inward => column.decay * row.gauss,
            Footprint::Edge => column.gauss * row.decay,
        }
    }
}

/// Zone weights for one frame size and active area. `ZoneSampler::set_depth_influence` and
/// `ZoneSampler::set_edge_geometry` drop the cache, since they are the only zone changes.
struct WeightCache {
    width: u32,
    height: u32,
    active: ActiveRect,
    zones: Vec<ZoneWeights>,
}

fn build_zone_weights(
    zone: &LightZone,
    width: u32,
    height: u32,
    active: ActiveRect,
    edge_geometry: EdgeGeometry,
) -> ZoneWeights {
    let act_x_min = active.x_min;
    let act_x_range = (active.x_max - act_x_min).max(0.1);
    let act_y_min = active.y_min;
    let act_y_range = (active.y_max - act_y_min).max(0.1);
    // Remap normalized coordinates to the active viewport
    let room = zone.uses_room_sampling();
    let edge = zone.edge_point();
    let footprint = if room {
        Footprint::Room
    } else if edge.is_some() {
        Footprint::Edge
    } else {
        Footprint::Uniform
    };
    let (center_x, center_y) = zone
        .hue_channel_position
        .filter(|_| room)
        .map(|pos| {
            (
                pos[0].clamp(-1.0, 1.0) * 0.5 + 0.5,
                0.5 - pos[2].clamp(-1.0, 1.0) * 0.5,
            )
        })
        .unwrap_or((
            (zone.x_min + zone.x_max) * 0.5,
            (zone.y_min + zone.y_max) * 0.5,
        ));
    let half_x = (center_x - zone.x_min)
        .abs()
        .max((zone.x_max - center_x).abs());
    let half_y = (center_y - zone.y_min)
        .abs()
        .max((zone.y_max - center_y).abs());
    let mapped_x_min = act_x_min + zone.x_min * act_x_range;
    let mapped_x_max = act_x_min + zone.x_max * act_x_range;
    let mapped_y_min = act_y_min + zone.y_min * act_y_range;
    let mapped_y_max = act_y_min + zone.y_max * act_y_range;

    let x_start = (mapped_x_min * width as f32).clamp(0.0, (width - 1) as f32) as u32;
    let x_end = (mapped_x_max * width as f32).clamp(0.0, width as f32) as u32;
    let y_start = (mapped_y_min * height as f32).clamp(0.0, (height - 1) as f32) as u32;
    let y_end = (mapped_y_max * height as f32).clamp(0.0, height as f32) as u32;

    let (edge_x, edge_y) = edge.unwrap_or_default();
    let (side, top) = edge.map(crate::config::edge_sides).unwrap_or((None, false));
    // Rows carry the Gaussian along a left or right edge: the vertical offset moves its centre.
    // On a top-only point the rows only carry the top decay, so the offset changes nothing.
    let along_y = if side.is_some() {
        edge_geometry.side_center(edge_y)
    } else {
        edge_y
    };
    let columns = (x_start..x_end)
        .step_by(2)
        .map(|x| {
            let nx = (x as f32 / width as f32 - act_x_min) / act_x_range;
            match footprint {
                Footprint::Uniform => AxisWeight::default(),
                Footprint::Room => AxisWeight::room(nx - center_x, half_x),
                Footprint::Edge => {
                    AxisWeight::edge(nx - edge_x, side.map(|s| (nx - s).abs()), edge_geometry)
                }
            }
        })
        .collect();
    let rows = (y_start..y_end)
        .step_by(2)
        .map(|y| {
            let ny = (y as f32 / height as f32 - act_y_min) / act_y_range;
            match footprint {
                Footprint::Uniform => AxisWeight::default(),
                Footprint::Room => AxisWeight::room(ny - center_y, half_y),
                Footprint::Edge => {
                    AxisWeight::edge(ny - along_y, top.then_some(ny.abs()), edge_geometry)
                }
            }
        })
        .collect();
    ZoneWeights {
        footprint,
        x_start,
        y_start,
        columns,
        rows,
    }
}

pub struct ZoneSampler {
    zones: Vec<LightZone>,
    smoothed_colors: Vec<FloatColor>,
    processor: ColorProcessor,
    black_frame: super::black_frame::BlackFrameGuard,
    letterbox_detection: bool,
    letterbox: LetterboxTracker,
    frame_count: u64,
    last_global_color: RgbColor,
    last_sample: Option<Instant>,
    weights: Option<WeightCache>,
    edge_geometry: EdgeGeometry,
    /// Per-segment cut threshold for edge-sampled zones; `None` turns the detection off.
    edge_cut_threshold: Option<f32>,
    #[cfg(test)]
    weight_builds: u32,
}

impl ZoneSampler {
    pub fn set_depth_influence(&mut self, influence: f32) {
        for zone in &mut self.zones {
            zone.set_footprint(influence, self.edge_geometry);
        }
        self.weights = None;
    }

    /// Resizes the edge bands and drops the weights: the next frame rebuilds them.
    pub fn set_edge_geometry(&mut self, edge: EdgeGeometry) {
        if edge == self.edge_geometry {
            return;
        }
        self.edge_geometry = edge;
        for zone in &mut self.zones {
            zone.set_edge_geometry(edge);
        }
        self.weights = None;
    }

    pub fn set_edge_cut_threshold(&mut self, threshold: Option<f32>) {
        self.edge_cut_threshold = threshold;
    }

    pub fn new(
        zones: Vec<LightZone>,
        smoothing_factor: f32,
        hdr_tone_mapping: bool,
        letterbox_detection: bool,
        saturation_boost: f32,
        noise_gate_threshold: f32,
    ) -> Self {
        let len = zones.len();
        Self {
            zones,
            smoothed_colors: vec![FloatColor::default(); len],
            processor: ColorProcessor::new(
                smoothing_factor,
                hdr_tone_mapping,
                saturation_boost,
                noise_gate_threshold,
            ),
            black_frame: Default::default(),
            letterbox_detection,
            letterbox: LetterboxTracker::default(),
            frame_count: 0,
            last_global_color: RgbColor::new(0, 0, 0),
            last_sample: None,
            weights: None,
            edge_geometry: EdgeGeometry::default(),
            edge_cut_threshold: None,
            #[cfg(test)]
            weight_builds: 0,
        }
    }

    pub fn set_spatial_blend(&mut self, strength: f32) {
        self.processor.set_spatial_blend(strength);
    }

    pub fn set_temporal_response(&mut self, rise: f32, fall: f32) {
        self.processor.set_temporal_response(rise, fall);
    }

    pub fn set_strict_blackout(&mut self, enabled: bool) {
        self.processor.set_strict_blackout(enabled);
    }

    pub fn set_black_frame_timing(&mut self, hold_ms: u32, fade_ms: u32) {
        self.black_frame.configure(hold_ms, fade_ms);
    }

    pub fn reset_black_frame(&mut self) {
        self.black_frame.reset();
        self.smoothed_colors.fill(FloatColor::default());
        self.last_sample = None;
    }

    pub fn set_hdr_tone_mapping(&mut self, enabled: bool) {
        self.processor.set_hdr_tone_mapping(enabled);
    }

    pub fn set_letterbox_detection(&mut self, enabled: bool) {
        self.letterbox_detection = enabled;
    }

    pub fn set_saturation_boost(&mut self, boost: f32) {
        self.processor.set_saturation_boost(boost);
    }

    pub fn set_peak_weight(&mut self, weight: f32) {
        self.processor.set_peak_weight(weight);
    }

    pub fn set_gamma(&mut self, gamma: f32) {
        self.processor.set_gamma(gamma);
    }

    pub fn set_noise_gate_threshold(&mut self, threshold: f32) {
        self.processor.set_noise_gate_threshold(threshold);
    }

    pub fn set_max_color_step(&mut self, step: u8) {
        self.processor.set_max_color_step(step);
    }

    /// Fast letterbox / pillarbox detector. Evaluates top/bottom row luminance to find black bars.
    /// Returns `None` when the centre is dark too: such a frame cannot tell bars from content.
    pub fn detect_active_rect(
        data: &[u8],
        width: u32,
        height: u32,
        is_bgra: bool,
    ) -> Option<ActiveRect> {
        let luma_threshold = 12u32; // Below this is considered black letterbox bar
        let sample_step_x = 4usize;
        let max_bar_y = height / 3; // Never crop more than 33% of screen

        // A fully dark frame would measure symmetric bars and shrink the area to the middle third.
        let mut centre_luma_sum = 0u64;
        let mut centre_count = 0u64;
        for y in (max_bar_y..height.saturating_sub(max_bar_y)).step_by(2) {
            let row_offset = (y * width * 4) as usize;
            for x in (0..width as usize).step_by(sample_step_x) {
                let pixel_offset = row_offset + (x * 4);
                if pixel_offset + 2 < data.len() {
                    let (r, g, b) = (
                        u64::from(data[pixel_offset]),
                        u64::from(data[pixel_offset + 1]),
                        u64::from(data[pixel_offset + 2]),
                    );
                    let (r, b) = if is_bgra { (b, r) } else { (r, b) };
                    centre_luma_sum += (r * 299 + g * 587 + b * 114) / 1000;
                    centre_count += 1;
                }
            }
        }
        if centre_count == 0 || centre_luma_sum / centre_count <= u64::from(luma_threshold) {
            return None;
        }

        // Top bar detection
        let mut top_bar_height = 0u32;
        for y in 0..max_bar_y {
            let row_offset = (y * width * 4) as usize;
            let mut row_luma_sum = 0u32;
            let mut sample_count = 0u32;

            for x in (0..width as usize).step_by(sample_step_x) {
                let pixel_offset = row_offset + (x * 4);
                if pixel_offset + 2 < data.len() {
                    let (r, g, b) = if is_bgra {
                        (
                            data[pixel_offset + 2] as u32,
                            data[pixel_offset + 1] as u32,
                            data[pixel_offset] as u32,
                        )
                    } else {
                        (
                            data[pixel_offset] as u32,
                            data[pixel_offset + 1] as u32,
                            data[pixel_offset + 2] as u32,
                        )
                    };
                    let luma = (r * 299 + g * 587 + b * 114) / 1000;
                    row_luma_sum += luma;
                    sample_count += 1;
                }
            }

            if sample_count > 0 && (row_luma_sum / sample_count) <= luma_threshold {
                top_bar_height = y + 1;
            } else {
                break;
            }
        }

        // Bottom bar detection
        let mut bottom_bar_height = 0u32;
        for y in 0..max_bar_y {
            let actual_y = height - 1 - y;
            let row_offset = (actual_y * width * 4) as usize;
            let mut row_luma_sum = 0u32;
            let mut sample_count = 0u32;

            for x in (0..width as usize).step_by(sample_step_x) {
                let pixel_offset = row_offset + (x * 4);
                if pixel_offset + 2 < data.len() {
                    let (r, g, b) = if is_bgra {
                        (
                            data[pixel_offset + 2] as u32,
                            data[pixel_offset + 1] as u32,
                            data[pixel_offset] as u32,
                        )
                    } else {
                        (
                            data[pixel_offset] as u32,
                            data[pixel_offset + 1] as u32,
                            data[pixel_offset + 2] as u32,
                        )
                    };
                    let luma = (r * 299 + g * 587 + b * 114) / 1000;
                    row_luma_sum += luma;
                    sample_count += 1;
                }
            }

            if sample_count > 0 && (row_luma_sum / sample_count) <= luma_threshold {
                bottom_bar_height = y + 1;
            } else {
                break;
            }
        }

        // Require symmetry within 6 pixels to avoid false positives from dark shadows
        let y_min = if top_bar_height >= 4
            && (top_bar_height as i32 - bottom_bar_height as i32).abs() <= 6
        {
            top_bar_height as f32 / height as f32
        } else {
            0.0
        };

        let y_max = if bottom_bar_height >= 4
            && (top_bar_height as i32 - bottom_bar_height as i32).abs() <= 6
        {
            (height - bottom_bar_height) as f32 / height as f32
        } else {
            1.0
        };

        Some(ActiveRect {
            x_min: 0.0,
            x_max: 1.0,
            y_min,
            y_max,
        })
    }

    /// Samples frame buffer (assumed 4 bytes per pixel: BGRA or RGBA), rounded to 8 bits.
    /// Returns sampled channel colors and a boolean indicating whether a hard scene cut occurred.
    pub fn sample_frame(
        &mut self,
        data: &[u8],
        width: u32,
        height: u32,
        is_bgra: bool,
    ) -> (Vec<(u8, RgbColor)>, bool) {
        let (colors, is_scene_cut) = self.sample_frame_precise(data, width, height, is_bgra);
        let colors = colors
            .into_iter()
            .map(|(channel_id, color)| (channel_id, color.to_rgb()))
            .collect();
        (colors, is_scene_cut)
    }

    /// Same as `sample_frame`, but keeps fractional colours so the output trim and the
    /// 16-bit conversion do not step through 8-bit plateaus on dark fades.
    pub fn sample_frame_precise(
        &mut self,
        data: &[u8],
        width: u32,
        height: u32,
        is_bgra: bool,
    ) -> (Vec<(u8, FloatColor)>, bool) {
        let now = Instant::now();
        let elapsed = self
            .last_sample
            .replace(now)
            .map(|last| now.duration_since(last))
            .unwrap_or(REFERENCE_FRAME);
        let mut output = self.smoothed_colors.clone();
        if self
            .black_frame
            .apply(data, width, height, &mut output, now)
        {
            return (self.output_colors(output), false);
        }
        self.frame_count += 1;

        // Run letterbox detector periodically (every 15 frames) or on first frame
        if self.letterbox_detection
            && (self.frame_count == 1 || self.frame_count.is_multiple_of(15))
        {
            self.letterbox
                .observe(Self::detect_active_rect(data, width, height, is_bgra));
        }

        // 1. Calculate overall global average color to detect sudden scene cuts
        let mut global_r: u64 = 0;
        let mut global_g: u64 = 0;
        let mut global_b: u64 = 0;
        let mut global_count: u64 = 0;

        // Sample center 50% grid for fast global scene change detection
        let g_ystart = (height / 4) as usize;
        let g_yend = (3 * height / 4) as usize;
        let g_xstart = (width / 4) as usize;
        let g_xend = (3 * width / 4) as usize;

        for y in (g_ystart..g_yend).step_by(4) {
            let row_offset = y * (width as usize) * 4;
            for x in (g_xstart..g_xend).step_by(4) {
                let pixel_offset = row_offset + (x * 4);
                if pixel_offset + 2 < data.len() {
                    let (r, g, b) = if is_bgra {
                        (
                            data[pixel_offset + 2] as u64,
                            data[pixel_offset + 1] as u64,
                            data[pixel_offset] as u64,
                        )
                    } else {
                        (
                            data[pixel_offset] as u64,
                            data[pixel_offset + 1] as u64,
                            data[pixel_offset + 2] as u64,
                        )
                    };
                    global_r += r;
                    global_g += g;
                    global_b += b;
                    global_count += 1;
                }
            }
        }

        let current_global_color = RgbColor::new(
            global_r.checked_div(global_count).unwrap_or(0) as u8,
            global_g.checked_div(global_count).unwrap_or(0) as u8,
            global_b.checked_div(global_count).unwrap_or(0) as u8,
        );

        // If global frame difference exceeds 0.35, classify as a sudden scene cut
        let is_scene_cut =
            self.frame_count > 1 && current_global_color.delta(self.last_global_color) > 0.35;
        self.last_global_color = current_global_color;

        let active = self.letterbox.active();
        let stale = self.weights.as_ref().is_none_or(|cache| {
            cache.width != width || cache.height != height || cache.active != active
        });
        if stale {
            self.weights = Some(WeightCache {
                width,
                height,
                active,
                zones: self
                    .zones
                    .iter()
                    .map(|zone| build_zone_weights(zone, width, height, active, self.edge_geometry))
                    .collect(),
            });
            #[cfg(test)]
            {
                self.weight_builds += 1;
            }
        }
        let cache = self.weights.as_ref().expect("weights were just built");

        for (i, (zone, weights)) in self.zones.iter().zip(&cache.zones).enumerate() {
            let room = zone.uses_room_sampling();
            let mut region = crate::color::sampling::RegionSample::default();

            for (row_index, &row) in weights.rows.iter().enumerate() {
                let y = weights.y_start + 2 * row_index as u32;
                let row_offset = (y * width * 4) as usize;
                for (column_index, &column) in weights.columns.iter().enumerate() {
                    let spatial = weights.weight(column, row);
                    if spatial == 0.0 {
                        continue; // Outside the edge band.
                    }
                    let x = weights.x_start + 2 * column_index as u32;
                    let pixel_offset = row_offset + (x * 4) as usize;
                    if pixel_offset + 3 < data.len() {
                        let (r, g, b) = if is_bgra {
                            (
                                data[pixel_offset + 2],
                                data[pixel_offset + 1],
                                data[pixel_offset],
                            )
                        } else {
                            (
                                data[pixel_offset],
                                data[pixel_offset + 1],
                                data[pixel_offset + 2],
                            )
                        };

                        let pix = RgbColor::new(r, g, b);

                        // Chroma weighting: vivid accents get higher weight so they aren't diluted by grey
                        let weight = self.processor.sample_weight(pix.chroma()) * spatial;

                        region.add(pix, weight);
                    }
                }
            }

            let (mean_color, highlight, max_luma) = region.finish();

            let current = self.smoothed_colors[i];
            let processor = if room {
                self.processor.for_room()
            } else {
                self.processor.clone()
            };
            // A gap this large means the light lags far behind the picture: the segment jumps
            // like a scene cut. Once the gap is below the threshold, the normal ramp gives a
            // soft landing. Only gradient-strip segments read one edge; other zones keep the
            // global cut alone.
            let zone_cut = is_scene_cut
                || self.edge_cut_threshold.is_some_and(|threshold| {
                    zone.uses_edge_sampling()
                        && processor
                            .blend_target(mean_color, highlight, max_luma)
                            .delta(current)
                            > threshold
                });
            let smoothed =
                processor.process(mean_color, highlight, max_luma, current, zone_cut, elapsed);
            self.smoothed_colors[i] = smoothed;
        }

        output.clone_from(&self.smoothed_colors);
        (self.output_colors(output), is_scene_cut)
    }

    fn output_colors(&self, mut results: Vec<FloatColor>) -> Vec<(u8, FloatColor)> {
        self.processor.blend_spatial(&mut results, |i| {
            let z = &self.zones[i];
            ((z.x_min + z.x_max) * 0.5, (z.y_min + z.y_max) * 0.5)
        });
        self.zones
            .iter()
            .zip(results)
            .map(|(z, c)| (z.channel_id, c))
            .collect()
    }

    #[allow(dead_code)]
    pub fn active_rect(&self) -> ActiveRect {
        self.letterbox.active()
    }
}

/// Scale the whole change vector, not each channel, so the colour keeps its hue on the way.
fn limit_color_step(current: FloatColor, target: FloatColor, max_step: f32) -> FloatColor {
    let (dr, dg, db) = (
        target.r - current.r,
        target.g - current.g,
        target.b - current.b,
    );
    let largest = dr.abs().max(dg.abs()).max(db.abs());
    if largest <= max_step {
        return target;
    }
    let factor = max_step.max(0.0) / largest;
    FloatColor::new(
        current.r + dr * factor,
        current.g + dg * factor,
        current.b + db * factor,
    )
}

#[cfg(test)]
mod tests {
    #[test]
    fn room_response_is_gentler_than_edges_and_keeps_black_immediate() {
        let mut edge = ColorProcessor::new(0.35, false, 1.0, 0.0);
        edge.set_peak_weight(0.0);
        edge.set_max_color_step(255);
        edge.set_strict_blackout(true);
        let room = edge.for_room();
        let current = FloatColor::new(100.0, 100.0, 100.0);
        let target = FloatColor::new(200.0, 0.0, 0.0);
        let fast = edge.process(target, target, 1.0, current, false, REFERENCE_FRAME);
        let gentle = room.process(target, target, 1.0, current, false, REFERENCE_FRAME);
        assert!(gentle.r < fast.r);
        assert!(gentle.g > fast.g);
        let red = FloatColor::new(100.0, 0.0, 0.0);
        let green = FloatColor::new(0.0, 100.0, 0.0);
        let shifted = room.process(green, green, 1.0, red, false, REFERENCE_FRAME);
        assert!((shifted.r.max(shifted.g).max(shifted.b) - 100.0).abs() < 0.001);
        assert_eq!(
            room.process(
                FloatColor::default(),
                FloatColor::default(),
                0.0,
                current,
                false,
                REFERENCE_FRAME
            ),
            FloatColor::default()
        );
    }

    #[test]
    fn black_hysteresis_requires_more_light_to_wake_than_to_remain_lit() {
        let mut processor = ColorProcessor::new(1.0, false, 1.0, 0.02);
        processor.set_peak_weight(0.0);
        processor.set_strict_blackout(true);
        let target = FloatColor::new(6.0, 6.0, 6.0);
        assert_eq!(
            processor.process(
                target,
                target,
                0.1,
                FloatColor::default(),
                false,
                REFERENCE_FRAME
            ),
            FloatColor::default()
        );
        let lit = processor.process(
            target,
            target,
            0.1,
            FloatColor::new(3.0, 3.0, 3.0),
            false,
            REFERENCE_FRAME,
        );
        assert!(lit.r > 0.0);
    }

    #[test]
    fn single_channel_room_footprints_expand_and_gradient_channels_read_the_edge() {
        let mut room = LightZone::from_3d_position(2, "Fixture", [0.0, 0.0, 0.0]);
        room.hue_device_id = Some("fixture-device".into());
        room.hue_segment_count = Some(1);
        let mut gradient = room.clone();
        gradient.hue_segment_count = Some(7);
        let mut square = room.clone();
        square.hue_segment_count = None;
        room.set_depth_influence(0.5);
        gradient.set_depth_influence(0.5);
        square.set_depth_influence(0.5);
        assert!(room.uses_room_sampling());
        assert!(!gradient.uses_room_sampling());
        assert!(gradient.uses_edge_sampling() && !room.uses_edge_sampling());
        assert!((room.x_max - room.x_min - 1.5 * (square.x_max - square.x_min)).abs() < 0.00001);
        // The picture centre reads the top-centre band: 3 sigma each way, 3 depths deep.
        let band = [
            gradient.x_min,
            gradient.x_max,
            gradient.y_min,
            gradient.y_max,
        ];
        for (actual, expected) in band.into_iter().zip([0.26, 0.74, 0.0, 0.18]) {
            assert!((actual - expected).abs() < 1e-5, "{band:?}");
        }
        assert_eq!(room.hue_channel_position, gradient.hue_channel_position);
        assert_eq!(room.channel_id, 2);
        room.hue_segment_count = None;
        assert!(!room.uses_room_sampling());

        let mut corner = LightZone::from_3d_position(2, "Fixture", [-1.0, 0.0, 1.0]);
        corner.hue_device_id = Some("fixture-device".into());
        corner.hue_segment_count = Some(1);
        corner.set_depth_influence(0.5);
        let mut uniform = corner.clone();
        uniform.hue_device_id = None;
        let mut gaussian = ZoneSampler::new(vec![corner], 1.0, false, false, 1.0, 0.0);
        let mut box_sample = ZoneSampler::new(vec![uniform], 1.0, false, false, 1.0, 0.0);
        gaussian.set_peak_weight(0.0);
        box_sample.set_peak_weight(0.0);
        gaussian.set_max_color_step(255);
        box_sample.set_max_color_step(255);
        let mut pixels = vec![0u8; 64 * 64 * 4];
        for y in 0..64 {
            for x in 0..64 {
                pixels[(y * 64 + x) * 4 + if x < 4 { 0 } else { 2 }] = 180;
            }
        }
        let g = gaussian.sample_frame(&pixels, 64, 64, false).0[0].1;
        let b = box_sample.sample_frame(&pixels, 64, 64, false).0[0].1;
        assert!(
            g.r > b.r && g.b < b.b,
            "Gaussian must favour the true corner: {g:?} vs {b:?}"
        );
    }

    #[test]
    fn whole_screen_black_holds_before_strict_black_and_resets_without_stale_colours() {
        let zone = LightZone::from_3d_position(7, "Fixture", [0.0, -1.0, 0.0]);
        let mut sampler = ZoneSampler::new(vec![zone], 1.0, false, true, 1.0, 0.02);
        sampler.set_strict_blackout(true);
        sampler.set_black_frame_timing(750, 2000);
        sampler
            .smoothed_colors
            .fill(FloatColor::new(80.0, 40.0, 20.0));
        let (held, cut) = sampler.sample_frame(&[0; 64], 4, 4, false);
        assert_eq!(held, vec![(7, RgbColor::new(80, 40, 20))]);
        sampler.set_black_frame_timing(0, 1);
        sampler.sample_frame(&[0; 64], 4, 4, false);
        std::thread::sleep(Duration::from_millis(3));
        let (faded, _) = sampler.sample_frame(&[0; 64], 4, 4, false);
        assert_eq!(faded, vec![(7, RgbColor::new(0, 0, 0))]);
        assert_eq!(
            sampler.smoothed_colors,
            vec![FloatColor::new(80.0, 40.0, 20.0)]
        );
        assert!(!cut);
        assert_eq!(sampler.active_rect(), ActiveRect::default());
        sampler.reset_black_frame();
        let (black, _) = sampler.sample_frame(&[0; 64], 4, 4, true);
        assert_eq!(black, vec![(7, RgbColor::new(0, 0, 0))]);
        sampler
            .smoothed_colors
            .fill(FloatColor::new(80.0, 40.0, 20.0));
        sampler.set_black_frame_timing(0, 0);
        let (disabled, _) = sampler.sample_frame(&[0; 64], 4, 4, false);
        assert_eq!(disabled, vec![(7, RgbColor::new(0, 0, 0))]);
    }

    #[test]
    fn live_depth_changes_sampling_without_replacing_channel_state() {
        let zone = LightZone::from_3d_position(7, "Gradient", [0.0, -1.0, 0.0]);
        let mut sampler = ZoneSampler::new(vec![zone], 1.0, false, false, 1.0, 0.0);
        let mut pixels = vec![0u8; 64 * 64 * 4];
        for y in 0..64 {
            for x in 20..24 {
                pixels[(y * 64 + x) * 4] = 255;
            }
        }
        sampler.set_depth_influence(0.0);
        let (off, _) = sampler.sample_frame(&pixels, 64, 64, false);
        sampler.set_depth_influence(1.0);
        sampler.last_sample = Some(Instant::now() - std::time::Duration::from_millis(34));
        let (strong, _) = sampler.sample_frame(&pixels, 64, 64, false);
        assert_eq!(off[0].1.r, 0);
        assert!(strong[0].1.r > 0);
        assert_eq!(sampler.zones[0].channel_id, 7);
    }
    use super::*;

    #[test]
    fn test_saturation_weighting_favors_vivid_colors() {
        let zone = LightZone {
            hue_channel_position: None,
            channel_id: 0,
            name: "Test".to_string(),
            hue_device_id: None,
            hue_segment_index: None,
            hue_segment_count: None,
            hue_positions: Vec::new(),
            x_min: 0.0,
            x_max: 1.0,
            y_min: 0.0,
            y_max: 1.0,
        };
        let mut sampler = ZoneSampler::new(vec![zone], 1.0, false, false, 2.0, 0.0);
        sampler.set_peak_weight(0.0);
        sampler.set_max_color_step(255);

        // Frame with half dull grey (100, 100, 100) and half bright red (255, 0, 0)
        let width = 4u32;
        let height = 2u32;
        let mut data = vec![0u8; (width * height * 4) as usize];
        for y in 0..height {
            for x in 0..width {
                let idx = ((y * width + x) * 4) as usize;
                if x < 2 {
                    data[idx] = 100;
                    data[idx + 1] = 100;
                    data[idx + 2] = 100;
                } else {
                    data[idx] = 255; // Red
                    data[idx + 1] = 0;
                    data[idx + 2] = 0;
                }
                data[idx + 3] = 255;
            }
        }

        let (results, _) = sampler.sample_frame(&data, width, height, false);
        let color = results[0].1;
        assert!(color.r > 200, "sampled color: {color:?}");
        assert!(color.g < 80);
    }

    #[test]
    fn test_letterbox_detection_detects_black_bars() {
        let width = 16u32;
        let height = 16u32;
        let mut data = vec![0u8; (width * height * 4) as usize];

        // Fill center rows 4..12 with bright white (y=0..3 and y=12..15 are black)
        for y in 4..12 {
            for x in 0..width {
                let idx = ((y * width + x) * 4) as usize;
                data[idx] = 200;
                data[idx + 1] = 200;
                data[idx + 2] = 200;
                data[idx + 3] = 255;
            }
        }

        let rect = ZoneSampler::detect_active_rect(&data, width, height, false)
            .expect("a bright centre is evidence of bars");
        assert_eq!(rect.y_min, 0.25); // 4 / 16 = 0.25
        assert_eq!(rect.y_max, 0.75); // 12 / 16 = 0.75
    }

    #[test]
    fn test_noise_gate_suppresses_faint_artifacts() {
        let faint_noise = FloatColor::new(4.0, 4.0, 4.0);
        let gated = faint_noise.apply_noise_gate(0.02);
        assert_eq!(gated, FloatColor::new(0.0, 0.0, 0.0));

        let bright = FloatColor::new(100.0, 100.0, 100.0);
        let not_gated = bright.apply_noise_gate(0.02);
        assert_eq!(not_gated, bright);
    }

    #[test]
    fn strict_blackout_does_not_leave_a_black_afterglow() {
        let zone = LightZone {
            hue_channel_position: None,
            channel_id: 0,
            name: "Test".to_string(),
            hue_device_id: None,
            hue_segment_index: None,
            hue_segment_count: None,
            hue_positions: Vec::new(),
            x_min: 0.0,
            x_max: 1.0,
            y_min: 0.0,
            y_max: 1.0,
        };
        let mut sampler = ZoneSampler::new(vec![zone], 0.35, false, false, 1.0, 0.02);
        sampler.set_strict_blackout(true);
        let bright = vec![255; 16];
        sampler.sample_frame(&bright, 2, 2, false);

        let black = vec![0; 16];
        let (colors, _) = sampler.sample_frame(&black, 2, 2, false);
        assert_eq!(colors[0].1, RgbColor::new(0, 0, 0));
    }

    #[test]
    fn test_scene_cut_is_limited() {
        assert_eq!(
            limit_color_step(
                FloatColor::new(0.0, 0.0, 0.0),
                FloatColor::new(255.0, 255.0, 255.0),
                12.0
            ),
            FloatColor::new(12.0, 12.0, 12.0)
        );
    }

    #[test]
    fn smoothing_tracks_elapsed_time_across_frame_rates() {
        let mut processor = ColorProcessor::new(0.05, false, 1.0, 0.0);
        processor.set_peak_weight(0.0);
        processor.set_max_color_step(255);
        let target = FloatColor::new(200.0, 200.0, 200.0);
        assert!((elapsed_alpha(0.05, REFERENCE_FRAME) - 0.05).abs() < 0.000_01);
        for (start, end) in [
            (FloatColor::new(0.0, 0.0, 0.0), target),
            (target, FloatColor::new(0.0, 0.0, 0.0)),
        ] {
            let mut outputs = Vec::new();
            for fps in [20, 30, 60] {
                let mut current = start;
                for _ in 0..(fps / 2) {
                    current = processor.process(
                        end,
                        end,
                        1.0,
                        current,
                        false,
                        Duration::from_secs_f32(1.0 / fps as f32),
                    );
                }
                outputs.push(current.r);
            }
            assert!(
                outputs
                    .iter()
                    .all(|value| (*value - outputs[1]).abs() < 0.01),
                "{outputs:?}"
            );
        }
    }

    #[test]
    fn large_color_steps_have_the_same_half_second_response() {
        let mut processor = ColorProcessor::new(1.0, false, 1.0, 0.0);
        processor.set_peak_weight(0.0);
        let black = FloatColor::new(0.0, 0.0, 0.0);
        let white = FloatColor::new(255.0, 255.0, 255.0);
        let outputs: Vec<f32> = [20, 30, 60]
            .into_iter()
            .map(|fps| {
                let mut current = black;
                for _ in 0..(fps / 2) {
                    current = processor.process(
                        white,
                        white,
                        1.0,
                        current,
                        false,
                        Duration::from_secs_f32(1.0 / fps as f32),
                    );
                }
                current.r
            })
            .collect();
        assert!(outputs.iter().all(|v| (*v - 180.0).abs() < 0.001));
        assert_eq!(
            processor.process(white, white, 1.0, black, false, Duration::from_secs(10)),
            FloatColor::new(36.0, 36.0, 36.0)
        );
    }

    #[test]
    fn elapsed_smoothing_preserves_zero_time_scene_cuts_and_blackout() {
        let mut processor = ColorProcessor::new(0.35, false, 1.0, 0.0);
        processor.set_peak_weight(0.0);
        let black = FloatColor::new(0.0, 0.0, 0.0);
        let white = FloatColor::new(255.0, 255.0, 255.0);
        assert_eq!(
            processor.process(white, white, 1.0, black, false, Duration::ZERO),
            black
        );
        assert_eq!(
            processor
                .process(white, white, 1.0, black, true, REFERENCE_FRAME)
                .to_rgb(),
            RgbColor::new(48, 48, 48)
        );
        assert_eq!(
            processor.process(white, white, 1.0, black, true, Duration::ZERO),
            black
        );
        processor.set_strict_blackout(true);
        assert_eq!(
            processor.process(black, black, 0.0, white, false, Duration::ZERO),
            black
        );
    }

    #[test]
    fn legacy_midtone_lift_leaves_black_and_white_unchanged() {
        let mut processor = ColorProcessor::new(1.0, false, 1.0, 0.0);
        processor.set_peak_weight(0.0);
        processor.set_max_color_step(255);
        let black = FloatColor::new(0.0, 0.0, 0.0);
        let gray = FloatColor::new(128.0, 128.0, 128.0);
        let white = FloatColor::new(255.0, 255.0, 255.0);
        for input in [black, gray, white] {
            assert_eq!(
                processor
                    .process(input, input, 1.0, black, true, REFERENCE_FRAME)
                    .to_rgb(),
                input.to_rgb()
            );
        }
        processor.set_hdr_tone_mapping(true);
        assert_eq!(
            processor
                .process(black, black, 1.0, white, true, REFERENCE_FRAME)
                .to_rgb(),
            black.to_rgb()
        );
        assert_eq!(
            processor
                .process(white, white, 1.0, black, true, REFERENCE_FRAME)
                .to_rgb(),
            white.to_rgb()
        );
        let lifted = processor.process(gray, gray, 1.0, black, true, REFERENCE_FRAME);
        assert!(lifted.r > gray.r);
        assert_eq!(lifted.r, lifted.g);
        assert_eq!(lifted.g, lifted.b);
    }

    #[test]
    fn fractional_fades_converge_without_stalling_and_tiny_steps_accumulate() {
        let mut processor = ColorProcessor::new(0.05, false, 1.0, 0.0);
        processor.set_peak_weight(0.0);
        processor.set_max_color_step(1);
        let target = FloatColor::new(1.0, 1.0, 1.0);
        for (mut current, end) in [
            (FloatColor::default(), target),
            (target, FloatColor::default()),
        ] {
            for _ in 0..180 {
                current = processor.process(end, end, 1.0, current, false, REFERENCE_FRAME);
            }
            assert_eq!(current.to_rgb(), end.to_rgb());
            assert!((current.r - end.r).abs() < 0.001);
        }
        let mut current = FloatColor::default();
        let white = FloatColor::new(255.0, 255.0, 255.0);
        for _ in 0..60 {
            current = processor.process(
                white,
                white,
                1.0,
                current,
                false,
                Duration::from_secs_f32(1.0 / 60.0),
            );
        }
        assert!((current.r - 30.0).abs() < 0.001);
        assert_eq!(
            FloatColor::new(0.49, 0.5, 300.0).to_rgb(),
            RgbColor::new(0, 1, 255)
        );
    }

    #[test]
    fn spatial_blend_uses_geometry_preserves_black_and_does_not_mutate_inputs() {
        let mut processor = ColorProcessor::new(1.0, false, 1.0, 0.0);
        let original = [
            FloatColor::new(255.0, 0.0, 0.0),
            FloatColor::new(0.0, 0.0, 255.0),
            FloatColor::default(),
        ];
        let center = |i| [(0.0, 0.0), (0.1, 0.0), (1.0, 1.0)][i];
        let mut output = original;
        processor.blend_spatial(&mut output, center);
        assert_eq!(output, original);
        processor.set_spatial_blend(0.5);
        processor.blend_spatial(&mut output, center);
        // One neighbour at distance 0.1: weight 0.51, share 0.51 / 1.51, strength 0.5 * 0.338.
        assert!((output[0].r - 211.926).abs() < 0.01, "{:?}", output[0]);
        assert_eq!(output[0].g, 0.0);
        assert!((output[0].b - 43.074).abs() < 0.01, "{:?}", output[0]);
        assert_eq!(
            (output[1].r, output[1].g, output[1].b),
            (output[0].b, output[0].g, output[0].r)
        );
        assert_eq!(output[2], FloatColor::default());
        let mut isolated = original;
        processor.blend_spatial(&mut isolated, |i| (i as f32, 0.0));
        assert_eq!(isolated, original);
        // Permuting IDs/order with their geometry cannot change spatial results.
        let mut reordered = [original[1], original[0], original[2]];
        processor.blend_spatial(&mut reordered, |i| center([1, 0, 2][i]));
        assert_eq!(reordered, [output[1], output[0], output[2]]);
    }

    #[test]
    fn hue_sampler_retains_sub_byte_fades_between_frames() {
        let zone = LightZone {
            hue_channel_position: None,
            channel_id: 7,
            name: "Test".into(),
            hue_device_id: None,
            hue_segment_index: None,
            hue_segment_count: None,
            hue_positions: Vec::new(),
            x_min: 0.0,
            x_max: 1.0,
            y_min: 0.0,
            y_max: 1.0,
        };
        let mut sampler = ZoneSampler::new(vec![zone], 0.05, false, false, 1.0, 0.0);
        sampler.set_peak_weight(0.0);
        let pixels = vec![1; 16];
        let mut result = Vec::new();
        for _ in 0..180 {
            sampler.last_sample = Some(Instant::now() - REFERENCE_FRAME);
            result = sampler.sample_frame(&pixels, 2, 2, false).0;
        }
        assert_eq!(result, vec![(7, RgbColor::new(1, 1, 1))]);
        assert!(sampler.smoothed_colors[0].r > 0.999);
    }

    #[test]
    fn palette_preserves_encoded_mixing_and_multiplicative_hsv_saturation() {
        let colors = [
            FloatColor::default(),
            FloatColor::new(255.0, 255.0, 255.0),
            FloatColor::new(255.0, 0.0, 0.0),
            FloatColor::new(0.0, 255.0, 128.0),
            FloatColor::new(12.25, 80.5, 220.75),
            FloatColor::new(1.0, 1.001, 0.999),
        ];
        for color in colors {
            for boost in [0.5_f32, 1.0, 1.5, 2.5, 3.0] {
                let max = color.r.max(color.g).max(color.b);
                let min = color.r.min(color.g).min(color.b);
                let factor = if max > min {
                    boost.min(max / (max - min))
                } else {
                    1.0
                };
                let expected = [color.r, color.g, color.b].map(|c| max - (max - c) * factor);
                let actual = color.boost_saturation(boost);
                let mut processor = ColorProcessor::new(1.0, false, 1.0, 0.0);
                processor.set_saturation_boost(boost);
                let transformed = processor.transform_color(color);
                assert!((transformed.r - actual.r).abs() < 0.001);
                assert!((transformed.g - actual.g).abs() < 0.001);
                assert!((transformed.b - actual.b).abs() < 0.001);
                for (value, expected) in [actual.r, actual.g, actual.b].into_iter().zip(expected) {
                    assert!((value - expected).abs() < 0.001, "{color:?}, boost={boost}");
                    assert!((0.0..=255.0).contains(&value));
                }
            }
            for alpha in [0.0, 0.05, 0.5, 1.0] {
                let target = FloatColor::new(100.0, 200.0, 50.0);
                let mixed = color.lerp(target, alpha);
                for (actual, (a, b)) in [mixed.r, mixed.g, mixed.b].into_iter().zip([
                    (color.r, target.r),
                    (color.g, target.g),
                    (color.b, target.b),
                ]) {
                    assert!((actual - (a + (b - a) * alpha)).abs() < 0.0001);
                }
            }
        }
        assert_eq!(
            FloatColor::new(-10.0, 300.0, f32::NAN).to_rgb(),
            RgbColor::new(0, 255, 0)
        );
    }

    fn processor_for_steps(rise: f32) -> ColorProcessor {
        let mut processor = ColorProcessor::new(rise, false, 1.0, 0.0);
        processor.set_peak_weight(0.0);
        processor
    }

    #[test]
    fn output_trim_preserves_hue_and_scales_dim_values_linearly() {
        // Old per-channel clipping: (255, 128, 0) * 1.3 became (255, 166, 0), a yellower orange.
        let orange = FloatColor::new(255.0, 128.0, 0.0);
        assert_eq!(orange.trim(1.3), orange);
        let near_peak = FloatColor::new(250.0, 125.0, 0.0).trim(1.3);
        assert!((near_peak.r - 255.0).abs() < 0.001);
        assert!((near_peak.g / near_peak.r - 0.5).abs() < 1e-6);
        let dim = FloatColor::new(196.0, 98.0, 10.0).trim(1.3);
        for (actual, expected) in [(dim.r, 254.8), (dim.g, 127.4), (dim.b, 13.0)] {
            assert!((actual - expected).abs() < 0.001, "{dim:?}");
        }
        assert_eq!(FloatColor::default().trim(1.3), FloatColor::default());
        assert_eq!(orange.to_u16(), (65535, 128 * 257, 0));
    }

    #[test]
    fn noise_gate_uses_the_largest_channel_and_keeps_dark_blue() {
        // Luma of (0, 0, 70) is 0.0198, which the old gate turned black at 0.02.
        let blue = FloatColor::new(0.0, 0.0, 70.0);
        assert_eq!(blue.apply_noise_gate(0.02), blue);
        assert_eq!(
            FloatColor::new(0.0, 0.0, 5.0).apply_noise_gate(0.02),
            FloatColor::default()
        );
        // Ramp: level 0.03 is halfway between the threshold and twice the threshold.
        let ramped = FloatColor::new(0.0, 0.0, 7.65).apply_noise_gate(0.02);
        assert!((ramped.b - 3.825).abs() < 0.01, "{ramped:?}");
    }

    #[test]
    fn step_limit_keeps_a_constant_hue_from_black_to_orange() {
        let processor = processor_for_steps(0.35);
        let orange = FloatColor::new(255.0, 128.0, 0.0);
        let mut current = FloatColor::default();
        for frame in 0..40 {
            current = processor.process(orange, orange, 1.0, current, false, REFERENCE_FRAME);
            assert_eq!(current.b, 0.0);
            assert!(
                (current.g / current.r - 128.0 / 255.0).abs() < 1e-4,
                "frame {frame}: {current:?}"
            );
            if frame == 0 {
                // The largest channel moves by the step limit; the others keep the ratio.
                assert!((current.r - 12.0).abs() < 0.001, "{current:?}");
            }
        }
        assert!(current.r > 254.0, "{current:?}");
    }

    #[test]
    fn spatial_blend_fades_in_for_a_zone_leaving_black() {
        let mut processor = processor_for_steps(1.0);
        processor.set_noise_gate_threshold(0.02);
        processor.set_spatial_blend(0.5);
        let center = |i| [(0.0, 0.0), (0.1, 0.0)][i];
        let white = FloatColor::new(255.0, 255.0, 255.0);
        let mut black = [white, FloatColor::default()];
        processor.blend_spatial(&mut black, center);
        assert_eq!(black[1], FloatColor::default());
        // Level 2/255 is below the fade end (2 * gate = 0.04): the blend adds 8.4, not 126.5.
        let mut leaving = [white, FloatColor::new(2.0, 2.0, 2.0)];
        processor.blend_spatial(&mut leaving, center);
        let jump = leaving[1].r - 2.0;
        assert!(jump > 0.0 && jump <= 12.0, "{:?}", leaving[1]);
        // Above the fade end the blend reaches its full, distance-based strength.
        let mut lit = [white, FloatColor::new(20.0, 20.0, 20.0)];
        processor.blend_spatial(&mut lit, center);
        assert!((lit[1].r - (20.0 + 235.0 * 0.5 * 0.337_838)).abs() < 0.01);
    }

    #[test]
    fn one_distant_neighbour_gets_less_than_full_blend_strength() {
        let mut processor = processor_for_steps(1.0);
        processor.set_spatial_blend(0.5);
        let mut colors = [
            FloatColor::new(255.0, 0.0, 0.0),
            FloatColor::new(0.0, 0.0, 255.0),
        ];
        // Distance 0.3: weight 0.0204, share 0.02. The old normalisation gave the full 0.5.
        processor.blend_spatial(&mut colors, |i| [(0.0, 0.0), (0.3, 0.0)][i]);
        assert!(colors[0].b > 0.0 && colors[0].b < 5.0, "{:?}", colors[0]);
        assert!(colors[0].r > 250.0, "{:?}", colors[0]);
    }

    #[test]
    fn dark_frames_keep_the_active_area_and_bars_need_consecutive_detections() {
        let size = 16u32;
        let dark = vec![0u8; (size * size * 4) as usize];
        assert_eq!(
            ZoneSampler::detect_active_rect(&dark, size, size, false),
            None
        );

        let zone = LightZone {
            channel_id: 0,
            name: "Test".to_string(),
            hue_device_id: None,
            hue_segment_index: None,
            hue_segment_count: None,
            hue_positions: Vec::new(),
            hue_channel_position: None,
            x_min: 0.0,
            x_max: 1.0,
            y_min: 0.0,
            y_max: 1.0,
        };
        let mut sampler = ZoneSampler::new(vec![zone], 0.35, false, true, 1.0, 0.02);
        sampler.sample_frame(&dark, size, size, false);
        // The old detector measured 5-row bars on each side and kept only the middle third.
        assert_eq!(sampler.active_rect(), ActiveRect::default());

        let mut bars = dark.clone();
        for pixel in bars[(4 * size * 4) as usize..(12 * size * 4) as usize].chunks_mut(4) {
            pixel.copy_from_slice(&[200, 200, 200, 255]);
        }
        let detected = ZoneSampler::detect_active_rect(&bars, size, size, false);
        let letterbox = detected.expect("bars around a bright centre");
        let mut tracker = LetterboxTracker::default();
        for _ in 1..LETTERBOX_CONFIRMATIONS {
            assert_eq!(tracker.observe(detected), ActiveRect::default());
            // A dark frame in between neither adopts nor resets the candidate.
            assert_eq!(tracker.observe(None), ActiveRect::default());
        }
        assert_eq!(tracker.observe(detected), letterbox);
        assert_eq!(letterbox.y_min, 0.25);
        // One stray full-frame detection does not undo the adopted letterbox.
        assert_eq!(tracker.observe(Some(ActiveRect::default())), letterbox);
        assert_eq!(tracker.observe(detected), letterbox);
    }

    #[test]
    fn scene_cut_moves_four_times_the_normal_step() {
        let processor = processor_for_steps(0.35);
        let white = FloatColor::new(255.0, 255.0, 255.0);
        let black = FloatColor::default();
        let normal = processor.process(white, white, 1.0, black, false, REFERENCE_FRAME);
        let cut = processor.process(white, white, 1.0, black, true, REFERENCE_FRAME);
        assert!((normal.r - 12.0).abs() < 0.001, "{normal:?}");
        assert!((cut.r - 48.0).abs() < 0.001, "{cut:?}");
        let mut current = black;
        let mut frames = 0;
        while current.r < 255.0 {
            current = processor.process(white, white, 1.0, current, true, REFERENCE_FRAME);
            frames += 1;
        }
        assert_eq!(frames, 6);
    }

    #[test]
    fn dark_fade_through_the_trim_is_monotonic_without_8_bit_plateaus() {
        let processor = processor_for_steps(0.05);
        let target = FloatColor::new(10.0, 10.0, 10.0);
        let mut current = FloatColor::default();
        let mut previous_wide = 0u16;
        let mut previous_byte = 0u8;
        let mut byte_plateaus = 0;
        for frame in 0..60 {
            current = processor.process(target, target, 1.0, current, false, REFERENCE_FRAME);
            let wide = current.trim(1.3).to_u16().0;
            assert!(
                wide > previous_wide,
                "frame {frame}: {wide} <= {previous_wide}"
            );
            previous_wide = wide;
            // The old path rounded to 8 bits before the trim and held the same value.
            let byte = current.to_rgb().r;
            if byte == previous_byte {
                byte_plateaus += 1;
            }
            previous_byte = byte;
        }
        assert!(byte_plateaus > 10, "{byte_plateaus}");
    }

    #[test]
    fn near_black_noise_weighs_about_the_same_as_grey() {
        let processor = ColorProcessor::new(1.0, false, 2.0, 0.0);
        let noise = processor.sample_weight(RgbColor::new(3, 0, 1).chroma());
        let grey = processor.sample_weight(RgbColor::new(2, 2, 2).chroma());
        let red = processor.sample_weight(RgbColor::new(255, 0, 0).chroma());
        // The old (max - min) / max measure gave the noise the weight of pure red (3.0).
        assert!((noise - grey).abs() < 0.001, "{noise} vs {grey}");
        assert_eq!(red, 3.0);
    }

    const BORDER_WIDTH: usize = 320;
    const BORDER_HEIGHT: usize = 180;

    /// Grey (128, 128, 128) band 15 % deep on every side, red (255, 0, 0) inside.
    fn grey_border_red_centre() -> Vec<u8> {
        let (border_x, border_y) = (BORDER_WIDTH * 15 / 100, BORDER_HEIGHT * 15 / 100);
        let mut pixels = vec![255u8; BORDER_WIDTH * BORDER_HEIGHT * 4];
        for y in 0..BORDER_HEIGHT {
            for x in 0..BORDER_WIDTH {
                let border = x < border_x
                    || x >= BORDER_WIDTH - border_x
                    || y < border_y
                    || y >= BORDER_HEIGHT - border_y;
                let rgb = if border { [128, 128, 128] } else { [255, 0, 0] };
                pixels[(y * BORDER_WIDTH + x) * 4..][..3].copy_from_slice(&rgb);
            }
        }
        pixels
    }

    fn gradient_segment(channel_id: u8, pos: [f32; 3]) -> LightZone {
        let mut zone = LightZone::from_3d_position(channel_id, "Gradient", pos);
        zone.hue_device_id = Some("strip".into());
        zone.hue_segment_index = Some(channel_id);
        zone.hue_segment_count = Some(7);
        zone.set_depth_influence(0.5);
        zone
    }

    /// Instant response, no peak mix, chroma weight 1 + 1.5 * chroma^2 (pure red weighs 2.5).
    fn instant_sampler(zones: Vec<LightZone>) -> ZoneSampler {
        let mut sampler = ZoneSampler::new(zones, 1.0, false, false, 1.5, 0.0);
        sampler.set_peak_weight(0.0);
        sampler.set_max_color_step(255);
        sampler
    }

    #[test]
    fn gradient_segments_read_the_grey_edge_band_not_the_red_centre() {
        let zones = vec![
            gradient_segment(0, [-0.8, 0.0, 0.0]),
            gradient_segment(1, [-0.5, 0.0, 0.6]),
            gradient_segment(2, [0.0, 0.0, 0.8]),
            gradient_segment(3, [0.8, 0.0, 0.0]),
        ];
        let mut sampler = instant_sampler(zones);
        let (colors, _) = sampler.sample_frame_precise(
            &grey_border_red_centre(),
            BORDER_WIDTH as u32,
            BORDER_HEIGHT as u32,
            false,
        );
        assert_eq!(
            colors.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
            [0, 1, 2, 3]
        );
        for (channel, color) in colors {
            // The band reaches 3 depths (18 %) inward; red starts at 15 %. Red then holds
            // (e^-2.5 - e^-3) / (1 - e^-3) = 3.4 % of the inward weight, 8 % after its 2.5x
            // chroma weight: r - g is about 21 before, and about 31 after, the 1.5 saturation
            // boost. 48 leaves margin for the step-2 grid; the red centre alone gives 255.
            assert!(color.r - color.g < 48.0, "channel {channel}: {color:?}");
            assert!(color.g > 90.0, "channel {channel}: {color:?}");
        }
    }

    /// Red (255, 0, 0) on the upper half, blue (0, 0, 255) on the lower half.
    fn red_top_blue_bottom() -> Vec<u8> {
        let mut pixels = vec![255u8; BORDER_WIDTH * BORDER_HEIGHT * 4];
        for y in 0..BORDER_HEIGHT {
            let rgb = if y < BORDER_HEIGHT / 2 {
                [255, 0, 0]
            } else {
                [0, 0, 255]
            };
            for x in 0..BORDER_WIDTH {
                pixels[(y * BORDER_WIDTH + x) * 4..][..3].copy_from_slice(&rgb);
            }
        }
        pixels
    }

    #[test]
    fn vertical_offset_moves_what_a_side_segment_reads() {
        use crate::config::SCREEN_HEIGHT_CM;
        let sample = |offset_cm: f32| {
            let mut sampler = instant_sampler(vec![gradient_segment(0, [-0.8, 0.0, 0.0])]);
            let geometry = EdgeGeometry::default();
            sampler.set_edge_geometry(geometry.with_vertical_offset(offset_cm, SCREEN_HEIGHT_CM));
            let (colors, _) = sampler.sample_frame_precise(
                &red_top_blue_bottom(),
                BORDER_WIDTH as u32,
                BORDER_HEIGHT as u32,
                false,
            );
            colors[0].1
        };
        // The middle-left segment sits on the red/blue line: it mixes both.
        let level = sample(0.0);
        assert!(level.r > 60.0 && level.b > 60.0, "{level:?}");
        // +15 cm moves its centre 18.5 % up, 2.3 sigma above the line: red wins.
        let raised = sample(15.0);
        assert!(raised.r - raised.b > 150.0, "{raised:?}");
        // -15 cm moves it as far down: blue wins.
        let lowered = sample(-15.0);
        assert!(lowered.b - lowered.r > 150.0, "{lowered:?}");
    }

    #[test]
    fn room_lamp_on_the_picture_centre_still_sees_the_red_centre() {
        let mut lamp = LightZone::from_3d_position(5, "Lamp", [0.0, 0.0, 0.0]);
        lamp.hue_device_id = Some("lamp".into());
        lamp.hue_segment_count = Some(1);
        lamp.set_depth_influence(0.5);
        assert!(lamp.uses_room_sampling() && !lamp.uses_edge_sampling());
        let mut sampler = instant_sampler(vec![lamp]);
        let (colors, _) = sampler.sample_frame_precise(
            &grey_border_red_centre(),
            BORDER_WIDTH as u32,
            BORDER_HEIGHT as u32,
            false,
        );
        let color = colors[0].1;
        assert!(color.r - color.g > 200.0, "{color:?}");
    }

    #[test]
    fn sampling_weights_are_built_once_per_size_area_and_zone_list() {
        let mut lamp = LightZone::from_3d_position(5, "Lamp", [0.0, 0.0, 0.0]);
        lamp.hue_device_id = Some("lamp".into());
        lamp.hue_segment_count = Some(1);
        let zones = vec![gradient_segment(0, [-0.8, 0.0, 0.0]), lamp];
        let mut sampler = instant_sampler(zones);
        let frame = grey_border_red_centre();
        let (width, height) = (BORDER_WIDTH as u32, BORDER_HEIGHT as u32);
        sampler.sample_frame_precise(&frame, width, height, false);
        sampler.sample_frame_precise(&frame, width, height, false);
        assert_eq!(sampler.weight_builds, 1);
        sampler.set_depth_influence(1.0);
        sampler.sample_frame_precise(&frame, width, height, false);
        assert_eq!(sampler.weight_builds, 2);
        sampler.sample_frame_precise(&frame[..160 * 90 * 4], 160, 90, false);
        assert_eq!(sampler.weight_builds, 3);
        sampler.sample_frame_precise(&frame[..160 * 90 * 4], 160, 90, false);
        assert_eq!(sampler.weight_builds, 3);
    }
}
