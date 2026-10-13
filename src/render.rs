use std::cmp::{max, min};
use std::collections::HashMap;

use anyhow::{Context, Result, bail};
use font8x8::{BASIC_FONTS, UnicodeFonts};
use image::{ImageBuffer, RgbImage};
use rayon::prelude::*;

use crate::model::{Bounds, Graph, Lane, Pos, SurfaceCell};

const BACKGROUND: [u8; 3] = [20, 22, 26];
const MAX_IMAGE_BYTES: u64 = 512 * 1024 * 1024;
type SurfaceChunks = HashMap<(i32, i32), Box<[SurfaceCell; 256]>>;
type PixelPoint = (i64, i64);

pub fn rail_bounds(positions: &[Pos], padding: i32) -> Bounds {
    let mut bounds = Bounds {
        min_x: positions[0].x,
        max_x: positions[0].x,
        min_z: positions[0].z,
        max_z: positions[0].z,
    };
    for pos in positions.iter().skip(1) {
        bounds.min_x = min(bounds.min_x, pos.x);
        bounds.max_x = max(bounds.max_x, pos.x);
        bounds.min_z = min(bounds.min_z, pos.z);
        bounds.max_z = max(bounds.max_z, pos.z);
    }
    let padding = padding.max(0);
    bounds.min_x = bounds.min_x.saturating_sub(padding);
    bounds.max_x = bounds.max_x.saturating_add(padding);
    bounds.min_z = bounds.min_z.saturating_sub(padding);
    bounds.max_z = bounds.max_z.saturating_add(padding);
    bounds
}

pub fn output_scale(bounds: Bounds, max_size: u32) -> u64 {
    let max_size = u64::from(max_size.max(1));
    max(
        bounds.width().div_ceil(max_size),
        bounds.height().div_ceil(max_size),
    )
    .max(1)
}

#[allow(clippy::too_many_arguments)]
pub fn render_map(
    surfaces: &SurfaceChunks,
    graph: &Graph,
    lanes: &[Lane],
    stations: &[usize],
    bounds: Bounds,
    scale: u64,
    line_width: u32,
) -> Result<RgbImage> {
    if scale == 0 {
        bail!("map scale must be at least one block per pixel");
    }
    if !(1..=16).contains(&line_width) {
        bail!("lane width must be between 1 and 16 pixels");
    }
    if bounds.min_x > bounds.max_x || bounds.min_z > bounds.max_z {
        bail!("map bounds are inverted");
    }
    let width = bounds.width().div_ceil(scale);
    let height = bounds.height().div_ceil(scale);
    let bytes = width
        .checked_mul(height)
        .and_then(|pixels| pixels.checked_mul(3))
        .context("map dimensions overflow; reduce --max-size")?;
    if bytes > MAX_IMAGE_BYTES {
        bail!(
            "the {width}x{height} map needs {} MiB for pixels (limit: 512 MiB); reduce --max-size",
            bytes.div_ceil(1024 * 1024)
        );
    }
    let width = u32::try_from(width).context("map width is too large; reduce --max-size")?;
    let height = u32::try_from(height).context("map height is too large; reduce --max-size")?;
    let mut pixels = Vec::new();
    let byte_count = usize::try_from(bytes).context("map image exceeds addressable memory")?;
    pixels
        .try_reserve_exact(byte_count)
        .context("could not allocate the map image; reduce --max-size")?;
    pixels.resize(byte_count, 0);

    pixels
        .par_chunks_mut(width as usize * 3)
        .enumerate()
        .for_each(|(image_z, row)| {
            let world_z = sample_axis(bounds.min_z, bounds.max_z, image_z as u64, scale);
            let neighbor_offset = scale.min(u64::from(u32::MAX) + 1) as i64;
            let north_z = i64::from(world_z) - neighbor_offset;
            let mut current = RowSampler::new(surfaces);
            let mut north = RowSampler::new(surfaces);
            let mut west = RowSampler::new(surfaces);
            let mut previous: Option<(i32, Option<SurfaceCell>)> = None;
            for (image_x, pixel) in row.as_chunks_mut::<3>().0.iter_mut().enumerate() {
                let world_x = sample_axis(bounds.min_x, bounds.max_x, image_x as u64, scale);
                let cell = current.sample(i64::from(world_x), i64::from(world_z));
                let color = if let Some(cell) = cell {
                    let west_x = i64::from(world_x) - neighbor_offset;
                    let western = match previous {
                        Some((x, cell)) if i64::from(x) == west_x => cell,
                        _ => west.sample(west_x, i64::from(world_z)),
                    };
                    let north_height = north
                        .sample(i64::from(world_x), north_z)
                        .map_or(cell.height, |other| other.height);
                    let west_height = western.map_or(cell.height, |other| other.height);
                    let slope = (i32::from(cell.height) * 2
                        - i32::from(north_height)
                        - i32::from(west_height))
                    .clamp(-8, 8) as f32;
                    shade(cell.color, 0.88 + slope * 0.025)
                } else {
                    BACKGROUND
                };
                pixel.copy_from_slice(&color);
                previous = Some((world_x, cell));
            }
        });
    let mut image = ImageBuffer::from_raw(width, height, pixels)
        .context("map image dimensions do not match its pixel buffer")?;

    let paths: Vec<_> = lanes
        .iter()
        .map(|lane| projected_path(graph, lane, bounds, scale))
        .collect();
    let casing = StrokeBrush::new(line_width + 3);
    let foreground = StrokeBrush::new(line_width);
    for path in &paths {
        draw_path(&mut image, path, &casing, [16, 18, 22]);
    }
    for (lane, path) in lanes.iter().zip(&paths) {
        draw_path(&mut image, path, &foreground, lane.color);
    }

    let line_width = i64::from(line_width);
    let station_outline = StrokeBrush::new((line_width * 2 + 9) as u32);
    let station_fill = StrokeBrush::new((line_width * 2 + 5) as u32);
    let station_center = StrokeBrush::new((line_width * 2 + 1) as u32);
    let station_points: Vec<_> = stations
        .iter()
        .map(|&station| project(graph.positions[station], bounds, scale))
        .collect();
    for &point in &station_points {
        draw_brush(&mut image, point, &station_outline, [10, 11, 13]);
        draw_brush(&mut image, point, &station_fill, [250, 250, 245]);
        draw_brush(&mut image, point, &station_center, [28, 31, 36]);
    }

    for label in place_station_labels(&station_points, width, height, line_width) {
        draw_label(&mut image, label.rect.x + 2, label.rect.y + 2, &label.text);
    }
    Ok(image)
}

#[derive(Clone, Copy)]
struct PixelRect {
    x: i64,
    y: i64,
    width: i64,
    height: i64,
}

impl PixelRect {
    fn overlaps(self, other: Self) -> bool {
        self.x < other.x + other.width
            && other.x < self.x + self.width
            && self.y < other.y + other.height
            && other.y < self.y + self.height
    }

    fn expanded(self, margin: i64) -> Self {
        Self {
            x: self.x - margin,
            y: self.y - margin,
            width: self.width + margin * 2,
            height: self.height + margin * 2,
        }
    }

    fn bins(self) -> impl Iterator<Item = (i64, i64)> {
        let first_x = self.x.div_euclid(32);
        let last_x = (self.x + self.width - 1).div_euclid(32);
        let first_y = self.y.div_euclid(32);
        let last_y = (self.y + self.height - 1).div_euclid(32);
        (first_x..=last_x).flat_map(move |x| (first_y..=last_y).map(move |y| (x, y)))
    }
}

#[derive(Default)]
struct LabelOccupancy {
    bins: HashMap<(i64, i64), Vec<PixelRect>>,
}

impl LabelOccupancy {
    fn insert(&mut self, rect: PixelRect) {
        for bin in rect.bins() {
            self.bins.entry(bin).or_default().push(rect);
        }
    }

    fn overlaps(&self, rect: PixelRect) -> bool {
        rect.bins().any(|bin| {
            self.bins
                .get(&bin)
                .is_some_and(|occupied| occupied.iter().any(|&other| rect.overlaps(other)))
        })
    }
}

struct StationLabel {
    text: String,
    rect: PixelRect,
}

fn place_station_labels(
    points: &[PixelPoint],
    image_width: u32,
    image_height: u32,
    line_width: i64,
) -> Vec<StationLabel> {
    let radius = line_width + 4;
    let mut occupancy = LabelOccupancy::default();
    for &(x, y) in points {
        occupancy.insert(
            PixelRect {
                x: x - radius,
                y: y - radius,
                width: radius * 2 + 1,
                height: radius * 2 + 1,
            }
            .expanded(2),
        );
    }
    points
        .iter()
        .enumerate()
        .map(|(number, &(x, y))| {
            let text = format!("S{}", number + 1);
            let width = text.len() as i64 * 8 + 4;
            let height = 12;
            let clamp = |(left, top): PixelPoint| PixelRect {
                x: left.clamp(0, (i64::from(image_width) - width).max(0)),
                y: top.clamp(0, (i64::from(image_height) - height).max(0)),
                width,
                height,
            };
            let fallback = clamp((x + radius + 3, y - height / 2));
            let rect = (0..4)
                .flat_map(|ring| {
                    let distance = radius + 3 + ring * 14;
                    [
                        (x + distance, y - height / 2),
                        (x - distance - width, y - height / 2),
                        (x - width / 2, y - distance - height),
                        (x - width / 2, y + distance),
                        (x + distance, y - distance - height),
                        (x - distance - width, y - distance - height),
                        (x + distance, y + distance),
                        (x - distance - width, y + distance),
                    ]
                })
                .map(clamp)
                .find(|&rect| !occupancy.overlaps(rect))
                // A tiny or fully occupied image may have no free position;
                // retain every ID even when overlap is unavoidable.
                .unwrap_or(fallback);
            occupancy.insert(rect.expanded(2));
            StationLabel { text, rect }
        })
        .collect()
}

fn sample_axis(minimum: i32, maximum: i32, pixel: u64, scale: u64) -> i32 {
    let last_offset = (i64::from(maximum) - i64::from(minimum)) as u64;
    let offset = pixel
        .saturating_mul(scale)
        .saturating_add(scale / 2)
        .min(last_offset);
    (i64::from(minimum) + offset as i64) as i32
}

struct RowSampler<'a> {
    surfaces: &'a SurfaceChunks,
    chunk_key: Option<(i32, i32)>,
    chunk: Option<&'a [SurfaceCell; 256]>,
}

impl<'a> RowSampler<'a> {
    fn new(surfaces: &'a SurfaceChunks) -> Self {
        Self {
            surfaces,
            chunk_key: None,
            chunk: None,
        }
    }

    fn sample(&mut self, x: i64, z: i64) -> Option<SurfaceCell> {
        let (Ok(x), Ok(z)) = (i32::try_from(x), i32::try_from(z)) else {
            return None;
        };
        let key = (x.div_euclid(16), z.div_euclid(16));
        if self.chunk_key != Some(key) {
            self.chunk_key = Some(key);
            self.chunk = self.surfaces.get(&key).map(Box::as_ref);
        }
        let cell = self.chunk?[z.rem_euclid(16) as usize * 16 + x.rem_euclid(16) as usize];
        cell.present.then_some(cell)
    }
}

fn projected_path(graph: &Graph, lane: &Lane, bounds: Bounds, scale: u64) -> Vec<PixelPoint> {
    let mut path: Vec<PixelPoint> = Vec::new();
    for &node in &lane.path {
        let point = project(graph.positions[node], bounds, scale);
        if path.last() == Some(&point) {
            continue;
        }
        if path.len() >= 2 {
            let a = path[path.len() - 2];
            let b = path[path.len() - 1];
            let incoming = (i128::from(b.0 - a.0), i128::from(b.1 - a.1));
            let outgoing = (i128::from(point.0 - b.0), i128::from(point.1 - b.1));
            if incoming.0 * outgoing.1 == incoming.1 * outgoing.0
                && incoming.0 * outgoing.0 + incoming.1 * outgoing.1 > 0
            {
                *path.last_mut().expect("path has at least two points") = point;
                continue;
            }
        }
        path.push(point);
    }
    path
}

fn project(pos: Pos, bounds: Bounds, scale: u64) -> PixelPoint {
    let scale = scale.min(i64::MAX as u64) as i64;
    (
        (i64::from(pos.x) - i64::from(bounds.min_x)).div_euclid(scale),
        (i64::from(pos.z) - i64::from(bounds.min_z)).div_euclid(scale),
    )
}

struct StrokeBrush {
    // Each row is (vertical offset, first horizontal offset, last offset).
    rows: Vec<(i64, i64, i64)>,
}

impl StrokeBrush {
    fn new(width: u32) -> Self {
        let width = i64::from(width);
        let half_pixel = i64::from(width % 2 == 0);
        let diameter = width - 1 + half_pixel;
        let first = -(width - 1) / 2;
        let last = width / 2;
        let mut rows = Vec::new();
        for dy in first..=last {
            let mut start = None;
            let mut end = 0;
            for dx in first..=last {
                let x = dx * 2 - half_pixel;
                let y = dy * 2 - half_pixel;
                if x * x + y * y <= diameter * diameter {
                    start.get_or_insert(dx);
                    end = dx;
                }
            }
            if let Some(start) = start {
                rows.push((dy, start, end));
            }
        }
        Self { rows }
    }
}

fn draw_path(image: &mut RgbImage, path: &[PixelPoint], brush: &StrokeBrush, color: [u8; 3]) {
    if path.len() == 1 {
        draw_brush(image, path[0], brush, color);
    }
    for pair in path.windows(2) {
        draw_thick_line(image, pair[0], pair[1], brush, color);
    }
}

fn draw_thick_line(
    image: &mut RgbImage,
    start: PixelPoint,
    end: PixelPoint,
    brush: &StrokeBrush,
    color: [u8; 3],
) {
    let (mut x0, mut y0) = start;
    let (x1, y1) = end;
    if y0 == y1 {
        for &(offset, first, last) in &brush.rows {
            let left = min(x0, x1) + first;
            let right = max(x0, x1) + last;
            fill_rect(image, left, y0 + offset, right - left + 1, 1, color);
        }
        return;
    }
    if x0 == x1 {
        for &(offset, first, last) in &brush.rows {
            let top = min(y0, y1) + first;
            let bottom = max(y0, y1) + last;
            fill_rect(image, x0 + offset, top, 1, bottom - top + 1, color);
        }
        return;
    }
    let dx = (x1 - x0).abs();
    let sx = if x0 < x1 { 1 } else { -1 };
    let dy = -(y1 - y0).abs();
    let sy = if y0 < y1 { 1 } else { -1 };
    let mut error = dx + dy;
    loop {
        draw_brush(image, (x0, y0), brush, color);
        if x0 == x1 && y0 == y1 {
            break;
        }
        let twice = 2 * error;
        if twice >= dy {
            error += dy;
            x0 += sx;
        }
        if twice <= dx {
            error += dx;
            y0 += sy;
        }
    }
}

fn draw_brush(image: &mut RgbImage, center: PixelPoint, brush: &StrokeBrush, color: [u8; 3]) {
    for &(offset, first, last) in &brush.rows {
        fill_rect(
            image,
            center.0 + first,
            center.1 + offset,
            last - first + 1,
            1,
            color,
        );
    }
}

fn draw_label(image: &mut RgbImage, x: i64, y: i64, text: &str) {
    let width = text.chars().count() as i64 * 8 + 4;
    fill_rect(image, x - 2, y - 2, width, 12, [12, 14, 17]);
    for (char_number, character) in text.chars().enumerate() {
        let Some(glyph) = BASIC_FONTS.get(character) else {
            continue;
        };
        for (row, bits) in glyph.iter().copied().enumerate() {
            for column in 0..8 {
                if bits & (1 << column) != 0 {
                    put_pixel_checked(
                        image,
                        x + char_number as i64 * 8 + column,
                        y + row as i64,
                        [250, 250, 245],
                    );
                }
            }
        }
    }
}

fn fill_rect(image: &mut RgbImage, x: i64, y: i64, width: i64, height: i64, color: [u8; 3]) {
    let left = x.max(0).min(i64::from(image.width())) as usize;
    let right = (x + width).max(0).min(i64::from(image.width())) as usize;
    let top = y.max(0).min(i64::from(image.height())) as usize;
    let bottom = (y + height).max(0).min(i64::from(image.height())) as usize;
    if left >= right || top >= bottom {
        return;
    }
    let row_bytes = image.width() as usize * 3;
    let pixels: &mut [u8] = image.as_mut();
    for py in top..bottom {
        for pixel in pixels[py * row_bytes + left * 3..py * row_bytes + right * 3]
            .as_chunks_mut::<3>()
            .0
            .iter_mut()
        {
            pixel.copy_from_slice(&color);
        }
    }
}

fn put_pixel_checked(image: &mut RgbImage, x: i64, y: i64, color: [u8; 3]) {
    if x >= 0 && y >= 0 && x < i64::from(image.width()) && y < i64::from(image.height()) {
        image.put_pixel(x as u32, y as u32, image::Rgb(color));
    }
}

fn shade(color: [u8; 3], factor: f32) -> [u8; 3] {
    color.map(|channel| (f32::from(channel) * factor).clamp(0.0, 255.0) as u8)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn graph(positions: Vec<Pos>) -> Graph {
        Graph {
            edges: vec![Vec::new(); positions.len()],
            positions,
            stations: Vec::new(),
        }
    }

    #[test]
    fn scale_is_an_integer_and_never_zero() {
        let bounds = Bounds {
            min_x: -100,
            max_x: 899,
            min_z: 0,
            max_z: 199,
        };
        assert_eq!(output_scale(bounds, 500), 2);
        assert_eq!(output_scale(bounds, 2000), 1);
        assert_eq!(output_scale(bounds, 0), 1000);
    }

    #[test]
    fn full_coordinate_range_does_not_wrap() {
        let bounds = Bounds {
            min_x: i32::MIN,
            max_x: i32::MAX,
            min_z: i32::MIN,
            max_z: i32::MAX,
        };
        assert_eq!(output_scale(bounds, 1), 1_u64 << 32);
        let image = render_map(
            &HashMap::new(),
            &graph(Vec::new()),
            &[],
            &[],
            bounds,
            output_scale(bounds, 1),
            3,
        )
        .unwrap();
        assert_eq!(image.dimensions(), (1, 1));
        assert_eq!(sample_axis(i32::MAX - 1, i32::MAX, 0, 4), i32::MAX);
    }

    #[test]
    fn oversized_image_returns_an_error_before_allocating() {
        let bounds = Bounds {
            min_x: 0,
            max_x: 20_000,
            min_z: 0,
            max_z: 20_000,
        };
        let error =
            render_map(&HashMap::new(), &graph(Vec::new()), &[], &[], bounds, 1, 3).unwrap_err();
        assert!(error.to_string().contains("reduce --max-size"));
    }

    #[test]
    fn row_sampler_handles_negative_coordinates_and_missing_chunks() {
        let mut surfaces = HashMap::new();
        let mut chunk = Box::new([SurfaceCell::default(); 256]);
        chunk[255] = SurfaceCell {
            color: [1, 2, 3],
            height: 64,
            present: true,
        };
        surfaces.insert((-1, -1), chunk);
        let mut sampler = RowSampler::new(&surfaces);
        assert_eq!(sampler.sample(-1, -1).unwrap().color, [1, 2, 3]);
        assert!(sampler.sample(0, -1).is_none());
        assert!(sampler.sample(1, -1).is_none());
        assert!(sampler.sample(i64::from(i32::MIN) - 1, -1).is_none());
    }

    #[test]
    fn colored_lane_is_drawn_over_the_surface() {
        let mut surfaces = HashMap::new();
        surfaces.insert(
            (0, 0),
            Box::new(
                [SurfaceCell {
                    color: [80, 120, 60],
                    height: 64,
                    present: true,
                }; 256],
            ),
        );
        let graph = graph(vec![Pos { x: 2, y: 20, z: 2 }, Pos { x: 10, y: 20, z: 2 }]);
        let lanes = vec![Lane {
            path: vec![0, 1],
            color: [240, 30, 50],
        }];
        let bounds = Bounds {
            min_x: 0,
            max_x: 15,
            min_z: 0,
            max_z: 15,
        };

        let image = render_map(&surfaces, &graph, &lanes, &[], bounds, 1, 3).unwrap();
        assert_eq!(image.dimensions(), (16, 16));
        assert_eq!(image.get_pixel(6, 2).0, [240, 30, 50]);
        assert_eq!(image.get_pixel(6, 8).0, shade([80, 120, 60], 0.88));
    }

    #[test]
    fn stroke_width_is_exact_for_even_and_odd_widths() {
        for width in 1..=16 {
            let mut image = ImageBuffer::from_pixel(64, 64, image::Rgb([0, 0, 0]));
            draw_thick_line(
                &mut image,
                (10, 32),
                (54, 32),
                &StrokeBrush::new(width),
                [255, 0, 0],
            );
            let colored_rows = (0..64)
                .filter(|&row| image.get_pixel(32, row).0 == [255, 0, 0])
                .count();
            assert_eq!(colored_rows, width as usize, "width {width}");
        }
    }

    #[test]
    fn projection_keeps_bends_and_backtracking() {
        let graph = graph(vec![
            Pos { x: 0, y: 20, z: 0 },
            Pos { x: 1, y: 20, z: 0 },
            Pos { x: 2, y: 20, z: 0 },
            Pos { x: 3, y: 20, z: 0 },
            Pos { x: 3, y: 20, z: 1 },
            Pos { x: 3, y: 20, z: 2 },
        ]);
        let lane = Lane {
            path: vec![0, 1, 2, 3, 4, 5, 4],
            color: [255, 0, 0],
        };
        let bounds = rail_bounds(&graph.positions, 0);
        assert_eq!(
            projected_path(&graph, &lane, bounds, 1),
            vec![(0, 0), (3, 0), (3, 2), (3, 1)]
        );
        assert_eq!(
            projected_path(&graph, &lane, bounds, 2),
            vec![(0, 0), (1, 0), (1, 1), (1, 0)]
        );
    }

    #[test]
    fn small_map_still_draws_a_lane_collapsed_to_one_pixel() {
        let graph = graph(vec![Pos { x: 0, y: 20, z: 0 }, Pos { x: 1, y: 20, z: 0 }]);
        let lane = Lane {
            path: vec![0, 1],
            color: [255, 0, 0],
        };
        let image = render_map(
            &HashMap::new(),
            &graph,
            &[lane],
            &[],
            rail_bounds(&graph.positions, 0),
            2,
            1,
        )
        .unwrap();
        assert_eq!(image.dimensions(), (1, 1));
        assert_eq!(image.get_pixel(0, 0).0, [255, 0, 0]);
    }

    #[test]
    fn axis_stroke_optimization_matches_individual_brushes() {
        for width in 1..=16 {
            let brush = StrokeBrush::new(width);
            for vertical in [false, true] {
                let point = |step| if vertical { (10, step) } else { (step, 10) };
                let mut expected = ImageBuffer::from_pixel(32, 32, image::Rgb([0, 0, 0]));
                let mut actual = expected.clone();
                for step in -2..=24 {
                    draw_brush(&mut expected, point(step), &brush, [255, 0, 0]);
                }
                draw_thick_line(&mut actual, point(24), point(-2), &brush, [255, 0, 0]);
                assert_eq!(actual.as_raw(), expected.as_raw(), "width {width}");
            }
        }
    }

    #[test]
    fn station_label_stays_visible_at_the_top_right_edge() {
        let graph = graph(vec![Pos { x: 63, y: 20, z: 0 }]);
        let bounds = Bounds {
            min_x: 0,
            max_x: 63,
            min_z: 0,
            max_z: 31,
        };
        let image = render_map(&HashMap::new(), &graph, &[], &[0], bounds, 1, 3).unwrap();
        assert_eq!(image.get_pixel(35, 0).0, [12, 14, 17]);
        let glyph_pixels = (37..53)
            .flat_map(|x| (2..10).map(move |y| (x, y)))
            .filter(|&(x, y)| image.get_pixel(x, y).0 == [250, 250, 245])
            .count();
        assert!(glyph_pixels > 0);
    }

    #[test]
    fn nearby_station_labels_avoid_each_other_and_both_markers() {
        let graph = graph(vec![
            Pos {
                x: 128,
                y: 20,
                z: 128,
            },
            Pos {
                x: 143,
                y: 20,
                z: 128,
            },
        ]);
        let bounds = Bounds {
            min_x: 0,
            max_x: 271,
            min_z: 0,
            max_z: 256,
        };
        let image = render_map(&HashMap::new(), &graph, &[], &[0, 1], bounds, 1, 3).unwrap();
        assert_eq!(image.get_pixel(128, 128).0, [28, 31, 36]);
        assert_eq!(image.get_pixel(143, 128).0, [28, 31, 36]);

        let labels = place_station_labels(&[(128, 128), (143, 128)], 272, 257, 3);
        assert_eq!(labels.len(), 2);
        assert!(!labels[0].rect.overlaps(labels[1].rect));
        for label in &labels {
            for x in [128, 143] {
                assert!(!label.rect.overlaps(PixelRect {
                    x: x - 7,
                    y: 121,
                    width: 15,
                    height: 15,
                }));
            }
            let rect = label.rect;
            let glyph_pixels = (rect.x + 2..rect.x + rect.width - 2)
                .flat_map(|x| (rect.y + 2..rect.y + 10).map(move |y| (x, y)))
                .filter(|&(x, y)| image.get_pixel(x as u32, y as u32).0 == [250, 250, 245])
                .count();
            assert!(glyph_pixels > 0, "{} must remain visible", label.text);
        }
    }

    #[test]
    fn adjacent_parallel_lanes_keep_both_colors_visible() {
        let graph = graph(vec![
            Pos { x: 5, y: 20, z: 8 },
            Pos { x: 59, y: 20, z: 8 },
            Pos { x: 5, y: 20, z: 9 },
            Pos { x: 59, y: 20, z: 9 },
        ]);
        let lanes = [
            Lane {
                path: vec![0, 1],
                color: [255, 0, 0],
            },
            Lane {
                path: vec![2, 3],
                color: [0, 0, 255],
            },
        ];
        let bounds = Bounds {
            min_x: 0,
            max_x: 63,
            min_z: 0,
            max_z: 31,
        };
        let image = render_map(&HashMap::new(), &graph, &lanes, &[], bounds, 1, 3).unwrap();
        let colors: Vec<_> = (0..32).map(|y| image.get_pixel(32, y).0).collect();
        assert!(colors.contains(&[255, 0, 0]));
        assert!(colors.contains(&[0, 0, 255]));
    }
}
