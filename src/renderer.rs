use std::{
    collections::HashMap,
    sync::{Mutex, OnceLock},
};

use ab_glyph::{Font, FontArc, FontVec, PxScale, ScaleFont, point};
use anyhow::{Context, Result};
use fontdb::{Database, Family, Query, Stretch, Style, Weight};
use tiny_skia::{FillRule, LineCap, Paint, Path, PathBuilder, Pixmap, Stroke, Transform};

use crate::config::RenderConfig;

const BASE_RING_WIDTH: f32 = 0.13;
const BAR_SHELL_OCCUPANCY: f32 = 0.98;
const BAR_FILL_OCCUPANCY: f32 = 0.90;
const BAR_CORNER_RATIO: f32 = 0.26;
const ARC_CONTROL: f32 = 0.552_284_8;

pub struct Renderer {
    background: Pixmap,
    frame: Pixmap,
    geometry: Geometry,
}

struct Geometry {
    center_x: f32,
    center_y: f32,
    base_radius: f32,
    max_length: f32,
    min_length: f32,
    bar_width: f32,
    cosines: Vec<f32>,
    sines: Vec<f32>,
}

impl Renderer {
    pub fn new(config: &RenderConfig, title: &str) -> Result<Self> {
        let mut background = cached_background(config)?;
        if config.show_title
            && let Some(font) = system_font()
        {
            draw_titles(&mut background, title, font);
        }
        let shortest = config.width.min(config.height) as f32;
        let mut cosines = Vec::with_capacity(config.bars);
        let mut sines = Vec::with_capacity(config.bars);
        for index in 0..config.bars {
            let angle = -std::f32::consts::FRAC_PI_2
                + std::f32::consts::TAU * index as f32 / config.bars as f32;
            cosines.push(angle.cos());
            sines.push(angle.sin());
        }
        let frame = background.clone();
        Ok(Self {
            background,
            frame,
            geometry: Geometry {
                center_x: config.width as f32 * 0.5,
                center_y: config.height as f32 * 0.435,
                base_radius: shortest * 0.17,
                max_length: shortest * 0.072,
                min_length: shortest * 0.016,
                bar_width: (shortest * 0.012).max(4.0),
                cosines,
                sines,
            },
        })
    }

    pub fn frame(&mut self, spectrum: &[f32], energy: f32, progress: f32) -> &[u8] {
        self.frame
            .data_mut()
            .copy_from_slice(self.background.data());
        let frame = &mut self.frame;
        let geometry = &self.geometry;
        let pulse = 1.0 + energy * 0.010;
        let base_radius = geometry.base_radius * pulse;
        let max_length = geometry.max_length * (0.86 + energy * 0.14);
        let progress_radius = geometry.base_radius
            + geometry.min_length
            + geometry.max_length
            + geometry.bar_width * 2.4;

        stroke_circle(
            frame,
            geometry.center_x,
            geometry.center_y,
            progress_radius,
            (geometry.bar_width * 0.11).max(1.5),
            (34, 63, 99, 25),
        );
        stroke_circle(
            frame,
            geometry.center_x,
            geometry.center_y,
            base_radius,
            (geometry.bar_width * BASE_RING_WIDTH).max(1.5),
            (59, 96, 133, 54),
        );
        draw_bars(frame, geometry, spectrum, energy, base_radius, max_length);
        draw_progress(frame, geometry, progress_radius, progress);
        fill_circle(
            frame,
            geometry.center_x,
            geometry.center_y,
            geometry.base_radius * (0.48 + energy * 0.025),
            (3, 8, 17, 170),
        );
        stroke_circle(
            frame,
            geometry.center_x,
            geometry.center_y,
            geometry.base_radius * (0.48 + energy * 0.025),
            (geometry.bar_width * 0.12).max(1.5),
            (64, 103, 143, 38),
        );
        frame.data()
    }
}

fn cached_background(config: &RenderConfig) -> Result<Pixmap> {
    static CACHE: OnceLock<Mutex<HashMap<(u32, u32), Pixmap>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let mut cache = cache.lock().expect("background cache poisoned");
    let key = (config.width, config.height);
    if let Some(background) = cache.get(&key) {
        return Ok(background.clone());
    }
    let background = create_background(config)?;
    cache.insert(key, background.clone());
    Ok(background)
}

fn create_background(config: &RenderConfig) -> Result<Pixmap> {
    let mut pixmap =
        Pixmap::new(config.width, config.height).context("invalid frame dimensions")?;
    let width = config.width as f32;
    let height = config.height as f32;
    let pixels = pixmap.data_mut();
    for y in 0..config.height {
        for x in 0..config.width {
            let nx = x as f32 / (width - 1.0).max(1.0);
            let ny = y as f32 / (height - 1.0).max(1.0);
            let teal = radial(nx, ny, 0.27, 0.24, 0.54);
            let blue = radial(nx, ny, 0.76, 0.34, 0.64);
            let ice = radial(nx, ny, 0.50, 0.48, 0.50);
            let distance = ((nx - 0.5).powi(2) + ((ny - 0.48) * 1.2).powi(2)).sqrt();
            let vignette = (1.14 - distance * 1.45).clamp(0.42, 1.0);
            let noise = pixel_noise(x, y);
            let base = [3.0 + ny * 2.0, 6.0 + ny * 3.0, 13.0 + ny * 6.0];
            let rgb = [
                (base[0] + teal * 2.0 + blue * 5.0 + ice * 4.0) * vignette + noise,
                (base[1] + teal * 20.0 + blue * 12.0 + ice * 14.0) * vignette + noise,
                (base[2] + teal * 30.0 + blue * 42.0 + ice * 25.0) * vignette + noise,
            ];
            let offset = ((y * config.width + x) * 4) as usize;
            let pixel = &mut pixels[offset..offset + 4];
            pixel[0] = rgb[0].clamp(0.0, 255.0) as u8;
            pixel[1] = rgb[1].clamp(0.0, 255.0) as u8;
            pixel[2] = rgb[2].clamp(0.0, 255.0) as u8;
            pixel[3] = 255;
        }
    }
    Ok(pixmap)
}

fn radial(x: f32, y: f32, center_x: f32, center_y: f32, radius: f32) -> f32 {
    (1.0 - ((x - center_x).powi(2) + (y - center_y).powi(2)).sqrt() / radius)
        .clamp(0.0, 1.0)
        .powi(2)
}

fn pixel_noise(x: u32, y: u32) -> f32 {
    let mut value = x.wrapping_mul(0x9e37_79b9) ^ y.wrapping_mul(0x85eb_ca6b) ^ 42;
    value ^= value >> 16;
    value = value.wrapping_mul(0x7feb_352d);
    value ^= value >> 15;
    (value as f32 / u32::MAX as f32 - 0.5) * 2.0
}

fn draw_bars(
    frame: &mut Pixmap,
    geometry: &Geometry,
    spectrum: &[f32],
    energy: f32,
    base_radius: f32,
    max_length: f32,
) {
    let mut shell = PathBuilder::new();
    let mut fill = PathBuilder::new();
    let inner = bar_inner_radius(base_radius, geometry.bar_width);
    let slot_width = std::f32::consts::TAU * inner / spectrum.len() as f32;
    let shell_width = slot_width * BAR_SHELL_OCCUPANCY;
    let fill_width = slot_width * BAR_FILL_OCCUPANCY;
    let outline_width = (shell_width - fill_width) * 0.55;
    for (index, value) in spectrum.iter().copied().enumerate() {
        let value = value.clamp(0.0, 1.0);
        let length = geometry.min_length + value.powf(0.86) * max_length;
        let outer = inner + length;
        let cosine = geometry.cosines[index];
        let sine = geometry.sines[index];
        push_spectrum_bar(
            &mut shell,
            geometry,
            (cosine, sine),
            inner..outer,
            shell_width,
        );
        push_spectrum_bar(
            &mut fill,
            geometry,
            (cosine, sine),
            inner..outer - outline_width,
            fill_width,
        );
    }
    if let Some(path) = shell.finish() {
        fill_path(frame, &path, (9, 29, 55, 224));
    }
    if let Some(path) = fill.finish() {
        fill_path(frame, &path, (55, 104, 151, 214 + (energy * 18.0) as u8));
    }
}

fn bar_inner_radius(base_radius: f32, bar_width: f32) -> f32 {
    base_radius + (bar_width * BASE_RING_WIDTH).max(1.5) * 0.5
}

fn push_spectrum_bar(
    builder: &mut PathBuilder,
    geometry: &Geometry,
    direction: (f32, f32),
    radii: std::ops::Range<f32>,
    width: f32,
) {
    let (cosine, sine) = direction;
    let half_width = width * 0.5;
    let corner = (width * BAR_CORNER_RATIO).min((radii.end - radii.start) * 0.45);
    let perpendicular = (-sine, cosine);
    let inner_center = (
        geometry.center_x + cosine * radii.start,
        geometry.center_y + sine * radii.start,
    );
    let outer_side = (
        geometry.center_x + cosine * (radii.end - corner),
        geometry.center_y + sine * (radii.end - corner),
    );
    let outer_face = (
        geometry.center_x + cosine * radii.end,
        geometry.center_y + sine * radii.end,
    );
    let side_left = (
        outer_side.0 + perpendicular.0 * half_width,
        outer_side.1 + perpendicular.1 * half_width,
    );
    let face_left = (
        outer_face.0 + perpendicular.0 * (half_width - corner),
        outer_face.1 + perpendicular.1 * (half_width - corner),
    );
    let face_right = (
        outer_face.0 - perpendicular.0 * (half_width - corner),
        outer_face.1 - perpendicular.1 * (half_width - corner),
    );
    let side_right = (
        outer_side.0 - perpendicular.0 * half_width,
        outer_side.1 - perpendicular.1 * half_width,
    );

    builder.move_to(
        inner_center.0 + perpendicular.0 * half_width,
        inner_center.1 + perpendicular.1 * half_width,
    );
    builder.line_to(side_left.0, side_left.1);
    builder.cubic_to(
        side_left.0 + cosine * corner * ARC_CONTROL,
        side_left.1 + sine * corner * ARC_CONTROL,
        face_left.0 + perpendicular.0 * corner * ARC_CONTROL,
        face_left.1 + perpendicular.1 * corner * ARC_CONTROL,
        face_left.0,
        face_left.1,
    );
    builder.line_to(face_right.0, face_right.1);
    builder.cubic_to(
        face_right.0 - perpendicular.0 * corner * ARC_CONTROL,
        face_right.1 - perpendicular.1 * corner * ARC_CONTROL,
        side_right.0 + cosine * corner * ARC_CONTROL,
        side_right.1 + sine * corner * ARC_CONTROL,
        side_right.0,
        side_right.1,
    );
    builder.line_to(
        inner_center.0 - perpendicular.0 * half_width,
        inner_center.1 - perpendicular.1 * half_width,
    );
    builder.close();
}

fn draw_progress(frame: &mut Pixmap, geometry: &Geometry, radius: f32, progress: f32) {
    if progress <= 0.0 {
        return;
    }
    let segments = (progress.clamp(0.0, 1.0) * 160.0).ceil() as usize;
    let mut builder = PathBuilder::new();
    for index in 0..=segments {
        let angle = progress_angle(progress, index, segments);
        let x = geometry.center_x + angle.cos() * radius;
        let y = geometry.center_y + angle.sin() * radius;
        if index == 0 {
            builder.move_to(x, y);
        } else {
            builder.line_to(x, y);
        }
    }
    if let Some(path) = builder.finish() {
        stroke_path(frame, &path, geometry.bar_width * 0.26, (84, 126, 169, 145));
    }
}

fn progress_angle(progress: f32, index: usize, segments: usize) -> f32 {
    -std::f32::consts::FRAC_PI_2
        - std::f32::consts::TAU * progress.clamp(0.0, 1.0) * index as f32 / segments as f32
}

fn stroke_circle(
    frame: &mut Pixmap,
    x: f32,
    y: f32,
    radius: f32,
    width: f32,
    color: (u8, u8, u8, u8),
) {
    if let Some(path) = PathBuilder::from_circle(x, y, radius) {
        stroke_path(frame, &path, width, color);
    }
}

fn fill_circle(frame: &mut Pixmap, x: f32, y: f32, radius: f32, color: (u8, u8, u8, u8)) {
    if let Some(path) = PathBuilder::from_circle(x, y, radius) {
        fill_path(frame, &path, color);
    }
}

fn fill_path(frame: &mut Pixmap, path: &Path, color: (u8, u8, u8, u8)) {
    let mut paint = Paint {
        anti_alias: true,
        ..Paint::default()
    };
    paint.set_color_rgba8(color.0, color.1, color.2, color.3);
    frame.fill_path(path, &paint, FillRule::Winding, Transform::identity(), None);
}

fn stroke_path(frame: &mut Pixmap, path: &Path, width: f32, color: (u8, u8, u8, u8)) {
    let mut paint = Paint {
        anti_alias: true,
        ..Paint::default()
    };
    paint.set_color_rgba8(color.0, color.1, color.2, color.3);
    let stroke = Stroke {
        width,
        line_cap: LineCap::Round,
        ..Stroke::default()
    };
    frame.stroke_path(path, &paint, &stroke, Transform::identity(), None);
}

fn system_font() -> Option<&'static FontArc> {
    static FONT: OnceLock<Option<FontArc>> = OnceLock::new();
    FONT.get_or_init(|| {
        let mut database = Database::new();
        database.load_system_fonts();
        database.set_sans_serif_family("Noto Sans");
        let id = database
            .query(&Query {
                families: &[Family::Name("Noto Sans"), Family::SansSerif],
                weight: Weight::BOLD,
                stretch: Stretch::Normal,
                style: Style::Normal,
            })
            .or_else(|| {
                database
                    .faces()
                    .min_by_key(|face| face.weight.0.abs_diff(Weight::BOLD.0))
                    .map(|face| face.id)
            })?;
        database.with_face_data(id, |data, face_index| {
            FontVec::try_from_vec_and_index(data.to_vec(), face_index)
                .ok()
                .map(FontArc::new)
        })?
    })
    .as_ref()
}

fn draw_titles(pixmap: &mut Pixmap, title: &str, font: &FontArc) {
    let height = pixmap.height() as f32;
    draw_centered_text(
        pixmap,
        "Master Duel OST",
        height * 0.090,
        height * 0.034,
        font,
        (125, 154, 180, 165),
    );
    let title = title_case(title);
    let size = fit_text_size(
        &title,
        pixmap.width() as f32 * 0.84,
        height * 0.082,
        height * 0.046,
        font,
    );
    draw_centered_text(
        pixmap,
        &title,
        height * 0.862,
        size,
        font,
        (174, 196, 216, 220),
    );
}

fn title_case(title: &str) -> String {
    title
        .split_whitespace()
        .map(|word| {
            let mut characters = word.chars();
            let Some(first) = characters.next() else {
                return String::new();
            };
            first.to_uppercase().chain(characters).collect::<String>()
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn fit_text_size(text: &str, max_width: f32, start: f32, minimum: f32, font: &FontArc) -> f32 {
    let mut size = start;
    while size > minimum && text_width(text, size, font) > max_width {
        size -= 2.0;
    }
    size.max(minimum)
}

fn text_width(text: &str, size: f32, font: &FontArc) -> f32 {
    let scaled = font.as_scaled(PxScale::from(size));
    let mut width = 0.0;
    let mut previous = None;
    for character in text.chars() {
        let glyph = scaled.glyph_id(character);
        if let Some(previous) = previous {
            width += scaled.kern(previous, glyph);
        }
        width += scaled.h_advance(glyph);
        previous = Some(glyph);
    }
    width
}

fn draw_centered_text(
    pixmap: &mut Pixmap,
    text: &str,
    center_y: f32,
    size: f32,
    font: &FontArc,
    color: (u8, u8, u8, u8),
) {
    let scaled = font.as_scaled(PxScale::from(size));
    let x = (pixmap.width() as f32 - text_width(text, size, font)) * 0.5;
    let baseline = center_y - (scaled.ascent() + scaled.descent()) * 0.5;
    draw_text(
        pixmap,
        text,
        x + 3.0,
        baseline + 4.0,
        size,
        font,
        (0, 0, 0, 150),
    );
    draw_text(pixmap, text, x, baseline, size, font, color);
}

fn draw_text(
    pixmap: &mut Pixmap,
    text: &str,
    mut x: f32,
    baseline: f32,
    size: f32,
    font: &FontArc,
    color: (u8, u8, u8, u8),
) {
    let scaled = font.as_scaled(PxScale::from(size));
    let mut previous = None;
    for character in text.chars() {
        let id = scaled.glyph_id(character);
        if let Some(previous) = previous {
            x += scaled.kern(previous, id);
        }
        let glyph = id.with_scale_and_position(size, point(x, baseline));
        if let Some(outlined) = font.outline_glyph(glyph) {
            let bounds = outlined.px_bounds();
            let width = pixmap.width();
            let height = pixmap.height();
            outlined.draw(|gx, gy, coverage| {
                let px = bounds.min.x as i32 + gx as i32;
                let py = bounds.min.y as i32 + gy as i32;
                if px < 0 || py < 0 || px >= width as i32 || py >= height as i32 {
                    return;
                }
                let offset = ((py as u32 * width + px as u32) * 4) as usize;
                blend_pixel(&mut pixmap.data_mut()[offset..offset + 4], color, coverage);
            });
        }
        x += scaled.h_advance(id);
        previous = Some(id);
    }
}

fn blend_pixel(pixel: &mut [u8], color: (u8, u8, u8, u8), coverage: f32) {
    let alpha = coverage * color.3 as f32 / 255.0;
    pixel[0] = (color.0 as f32 * alpha + pixel[0] as f32 * (1.0 - alpha)) as u8;
    pixel[1] = (color.1 as f32 * alpha + pixel[1] as f32 * (1.0 - alpha)) as u8;
    pixel[2] = (color.2 as f32 * alpha + pixel[2] as f32 * (1.0 - alpha)) as u8;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_titles_use_mixed_case() {
        assert_eq!(title_case("duel climax 01"), "Duel Climax 01");
        assert_eq!(title_case("dicerally"), "Dicerally");
    }

    #[test]
    fn flat_bar_base_touches_outer_edge_of_ring() {
        let base_radius = 100.0;
        let bar_width = 20.0;
        let ring_outer_edge = base_radius + bar_width * BASE_RING_WIDTH * 0.5;
        let bar_base = bar_inner_radius(base_radius, bar_width);

        assert_eq!(bar_base, ring_outer_edge);
    }

    #[test]
    fn spectrum_bars_are_dense_and_nearly_rectangular() {
        let radius = 246.0;
        let slot_width = std::f32::consts::TAU * radius / 72.0;
        let visible_gap = slot_width * (1.0 - BAR_FILL_OCCUPANCY);

        assert!(visible_gap < 3.0);
        assert!(slot_width * BAR_SHELL_OCCUPANCY > slot_width * 0.97);
    }

    #[test]
    fn progress_advances_counterclockwise_from_the_top() {
        let quarter_turn_end = progress_angle(0.25, 1, 1);

        assert!(quarter_turn_end.cos() < -0.99);
        assert!(quarter_turn_end.sin().abs() < 0.01);
    }
}
