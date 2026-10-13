use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use anyhow::{Context, Result, bail, ensure};
use flate2::read::{GzDecoder, ZlibDecoder};
use lz4_java_wrc::Lz4BlockInput;
use rayon::prelude::*;
use serde::Deserialize;

use crate::model::{Pos, Rail, SurfaceCell};

const NORTH: u8 = 1;
const EAST: u8 = 2;
const SOUTH: u8 = 4;
const WEST: u8 = 8;
const MAX_CHUNK_BYTES: usize = 64 * 1024 * 1024;
const WARNING_LIMIT: usize = 8;

#[derive(Default)]
pub struct ScanResult {
    pub surfaces: HashMap<(i32, i32), Box<[SurfaceCell; 256]>>,
    pub rails: HashMap<Pos, Rail>,
    pub chunks: usize,
    pub skipped_chunks: usize,
    warnings: Vec<String>,
}

impl ScanResult {
    fn merge(mut self, mut other: Self) -> Self {
        if self.surfaces.len() < other.surfaces.len() {
            std::mem::swap(&mut self, &mut other);
        }
        self.surfaces.extend(other.surfaces);
        self.rails.extend(other.rails);
        self.chunks += other.chunks;
        self.skipped_chunks += other.skipped_chunks;
        self.warnings.extend(
            other
                .warnings
                .into_iter()
                .take(WARNING_LIMIT.saturating_sub(self.warnings.len())),
        );
        self
    }

    fn warn(&mut self, message: String) {
        self.skipped_chunks += 1;
        if self.warnings.len() < WARNING_LIMIT {
            self.warnings.push(message);
        }
    }
}

#[derive(Deserialize, Debug)]
struct ChunkNbt {
    #[serde(rename = "DataVersion")]
    _data_version: i32,
    sections: Option<Vec<ChunkSection>>,
}

#[derive(Deserialize, Debug)]
struct ChunkSection {
    #[serde(rename = "Y")]
    y: i8,
    block_states: Option<BlockStates>,
}

#[derive(Deserialize, Debug)]
struct BlockStates {
    palette: Vec<BlockState>,
    data: Option<fastnbt::LongArray>,
}

#[derive(Deserialize, Debug)]
struct BlockState {
    #[serde(rename = "Name")]
    name: String,
    #[serde(rename = "Properties", default)]
    properties: BlockProperties,
}

#[derive(Default, Deserialize, Debug)]
struct BlockProperties {
    powered: Option<String>,
    shape: Option<String>,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum SurfaceKind {
    Air,
    Water,
    Solid,
}

#[derive(Clone, Copy)]
struct PaletteEntry {
    kind: SurfaceKind,
    color: [u8; 3],
    support: bool,
    rail: Option<Rail>,
}

struct PreparedSection<'a> {
    y: i8,
    palette: Vec<PaletteEntry>,
    data: Option<&'a [i64]>,
    values_per_long: usize,
    bits: usize,
    mask: u64,
    air_only: bool,
    has_rails: bool,
}

impl<'a> PreparedSection<'a> {
    fn new(section: &'a ChunkSection, support_blocks: &[String]) -> Result<Self> {
        let states = section.block_states.as_ref();
        let palette = states
            .map(|states| {
                states
                    .palette
                    .iter()
                    .map(|block| prepare_block(block, support_blocks))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        ensure!(
            palette.len() <= 4096,
            "block palette contains more than 4096 entries"
        );
        let bits = (usize::BITS - palette.len().saturating_sub(1).leading_zeros()).max(4) as usize;
        let values_per_long = 64 / bits;
        let data = states.and_then(|states| states.data.as_deref());
        if let Some(data) = data {
            ensure!(!palette.is_empty(), "block data has an empty palette");
            ensure!(
                data.len() == 4096usize.div_ceil(values_per_long),
                "invalid block data length for section Y={}: got {}, expected {}",
                section.y,
                data.len(),
                4096usize.div_ceil(values_per_long)
            );
        } else {
            ensure!(
                palette.len() <= 1,
                "section Y={} has multiple palette entries but no block data",
                section.y
            );
        }
        Ok(Self {
            y: section.y,
            air_only: palette.iter().all(|entry| entry.kind == SurfaceKind::Air),
            has_rails: palette.iter().any(|entry| entry.rail.is_some()),
            palette,
            data,
            values_per_long,
            bits,
            mask: (1u64 << bits) - 1,
        })
    }

    #[inline]
    fn at(&self, index: usize) -> Option<&PaletteEntry> {
        let palette_index = match self.data {
            Some(data) => {
                let packed = data[index / self.values_per_long] as u64;
                ((packed >> ((index % self.values_per_long) * self.bits)) & self.mask) as usize
            }
            None => 0,
        };
        self.palette.get(palette_index)
    }
}

fn prepare_sections<'a>(
    chunk: &'a ChunkNbt,
    support_blocks: &[String],
) -> Result<Vec<PreparedSection<'a>>> {
    let mut sections = chunk
        .sections
        .as_deref()
        .unwrap_or_default()
        .iter()
        .map(|section| PreparedSection::new(section, support_blocks))
        .collect::<Result<Vec<_>>>()?;
    sections.sort_unstable_by_key(|section| std::cmp::Reverse(section.y));
    ensure!(
        sections.windows(2).all(|pair| pair[0].y != pair[1].y),
        "duplicate section Y coordinates"
    );
    Ok(sections)
}

fn prepare_block(block: &BlockState, support_blocks: &[String]) -> PaletteEntry {
    let kind = match block.name.as_str() {
        "minecraft:air" | "minecraft:cave_air" | "minecraft:void_air" => SurfaceKind::Air,
        "minecraft:water" | "minecraft:bubble_column" => SurfaceKind::Water,
        _ => SurfaceKind::Solid,
    };
    let ordinary = block.name == "minecraft:rail";
    let powered = block.name == "minecraft:powered_rail"
        && block.properties.powered.as_deref() == Some("true");
    let shape = block.properties.shape.as_deref();
    let exits = rail_exits(shape);
    let ascending = match shape {
        Some("ascending_north") => NORTH,
        Some("ascending_east") => EAST,
        Some("ascending_south") => SOUTH,
        Some("ascending_west") => WEST,
        _ => 0,
    };
    PaletteEntry {
        kind,
        color: if kind == SurfaceKind::Solid {
            block_color(&block.name)
        } else {
            [0; 3]
        },
        support: support_blocks.contains(&block.name),
        rail: ((ordinary || powered) && exits != 0).then_some(Rail {
            exits,
            ascending,
            marker: powered,
            ordinary,
        }),
    }
}

pub fn scan_world(region_dir: &Path, support_blocks: &[String]) -> Result<ScanResult> {
    let mut paths = Vec::new();
    for entry in fs::read_dir(region_dir)
        .with_context(|| format!("could not read {}", region_dir.display()))?
    {
        let path = entry?.path();
        if parse_region_coords(&path).is_some() {
            paths.push(path);
        }
    }
    paths.sort_unstable();
    ensure!(
        !paths.is_empty(),
        "{} contains no r.X.Z.mca files",
        region_dir.display()
    );

    let result = paths
        .par_iter()
        .map(|path| match scan_region(path, support_blocks) {
            Ok(scan) => scan,
            Err(error) => {
                let mut scan = ScanResult::default();
                scan.warn(format!("{error:#}"));
                scan
            }
        })
        .reduce(ScanResult::default, ScanResult::merge);
    for warning in &result.warnings {
        eprintln!("warning: {warning}");
    }
    ensure!(
        result.chunks != 0,
        "no readable modern Java chunks were found"
    );
    Ok(result)
}

fn scan_region(path: &Path, support_blocks: &[String]) -> Result<ScanResult> {
    let (region_x, region_z) = parse_region_coords(path)
        .with_context(|| format!("invalid region filename {}", path.display()))?;
    let origin_x = region_x
        .checked_mul(512)
        .context("region X coordinate is out of range")?;
    let origin_z = region_z
        .checked_mul(512)
        .context("region Z coordinate is out of range")?;
    origin_x
        .checked_add(511)
        .context("region X coordinate is out of range")?;
    origin_z
        .checked_add(511)
        .context("region Z coordinate is out of range")?;
    let mut file =
        File::open(path).with_context(|| format!("could not open region {}", path.display()))?;
    let file_size = file.metadata()?.len();
    let mut header = [0u8; 8192];
    file.read_exact(&mut header)
        .with_context(|| format!("invalid region header {}", path.display()))?;
    let mut scan = ScanResult::default();

    for index in 0..1024 {
        let location = &header[index * 4..index * 4 + 4];
        if location == [0; 4] {
            continue;
        }
        let local_x = index % 32;
        let local_z = index / 32;
        let chunk_x = origin_x / 16 + local_x as i32;
        let chunk_z = origin_z / 16 + local_z as i32;
        let processed = (|| -> Result<_> {
            let bytes = read_chunk(&mut file, file_size, location, path, chunk_x, chunk_z)?;
            let chunk: ChunkNbt = fastnbt::from_bytes(&bytes)?;
            let sections = prepare_sections(&chunk, support_blocks)?;
            let surface = extract_surface(&sections);
            extract_rails(&sections, chunk_x, chunk_z, &mut scan.rails);
            Ok(surface)
        })();
        match processed {
            Ok(surface) => {
                scan.chunks += 1;
                scan.surfaces.insert((chunk_x, chunk_z), surface);
            }
            Err(error) => scan.warn(format!(
                "could not read chunk ({chunk_x},{chunk_z}) in {}: {error:#}",
                path.display()
            )),
        }
    }
    Ok(scan)
}

fn read_chunk(
    file: &mut File,
    file_size: u64,
    location: &[u8],
    path: &Path,
    chunk_x: i32,
    chunk_z: i32,
) -> Result<Vec<u8>> {
    let offset = u32::from_be_bytes([0, location[0], location[1], location[2]]) as u64 * 4096;
    let allocated = u64::from(location[3]) * 4096;
    ensure!(
        offset >= 8192 && allocated != 0,
        "invalid chunk sector location"
    );
    ensure!(
        offset + allocated <= file_size,
        "chunk sectors extend beyond region file"
    );
    file.seek(SeekFrom::Start(offset))?;
    let mut metadata = [0u8; 5];
    file.read_exact(&mut metadata)?;
    let length = u32::from_be_bytes(metadata[..4].try_into()?) as usize;
    ensure!(
        length >= 1 && length as u64 + 4 <= allocated,
        "invalid chunk length"
    );
    let scheme = metadata[4] & 0x7f;
    let data = if metadata[4] & 0x80 != 0 {
        ensure!(length == 1, "external chunk has unexpected inline data");
        let external = path.with_file_name(format!("c.{chunk_x}.{chunk_z}.mcc"));
        let mut external_file = File::open(&external)
            .with_context(|| format!("could not open external chunk {}", external.display()))?;
        let mut data = Vec::new();
        Read::by_ref(&mut external_file)
            .take(MAX_CHUNK_BYTES as u64 + 1)
            .read_to_end(&mut data)?;
        ensure!(
            data.len() <= MAX_CHUNK_BYTES,
            "external chunk exceeds 64 MiB"
        );
        data
    } else {
        let mut data = vec![0; length - 1];
        file.read_exact(&mut data)?;
        data
    };
    decompress(data, scheme)
}

fn decompress(data: Vec<u8>, scheme: u8) -> Result<Vec<u8>> {
    let reader: Box<dyn Read + '_> = match scheme {
        1 => Box::new(GzDecoder::new(&data[..])),
        2 => Box::new(ZlibDecoder::new(&data[..])),
        3 => return Ok(data),
        4 => {
            validate_lz4_blocks(&data)?;
            Box::new(Lz4BlockInput::new(&data[..]))
        }
        _ => bail!("unsupported Anvil compression type {scheme}"),
    };
    let mut output = Vec::new();
    reader
        .take(MAX_CHUNK_BYTES as u64 + 1)
        .read_to_end(&mut output)?;
    ensure!(
        output.len() <= MAX_CHUNK_BYTES,
        "decompressed chunk exceeds 64 MiB"
    );
    Ok(output)
}

// The decoder allocates from block headers before reading their payload.
// Check lengths first so a truncated or corrupt header cannot request GiBs.
fn validate_lz4_blocks(mut data: &[u8]) -> Result<()> {
    let mut total = 0usize;
    while !data.is_empty() {
        ensure!(
            data.len() >= 21 && &data[..8] == b"LZ4Block",
            "invalid LZ4 block header"
        );
        let compressed = u32::from_le_bytes(data[9..13].try_into()?) as usize;
        let decompressed = u32::from_le_bytes(data[13..17].try_into()?) as usize;
        ensure!(
            compressed <= data.len() - 21,
            "LZ4 block payload is truncated"
        );
        ensure!(
            decompressed <= MAX_CHUNK_BYTES - total,
            "decompressed chunk exceeds 64 MiB"
        );
        total += decompressed;
        if compressed == 0 && decompressed == 0 {
            return Ok(());
        }
        data = &data[21 + compressed..];
    }
    Ok(())
}

fn parse_region_coords(path: &Path) -> Option<(i32, i32)> {
    let name = path.file_name()?.to_str()?;
    let mut parts = name.split('.');
    if parts.next()? != "r" {
        return None;
    }
    let x = parts.next()?.parse().ok()?;
    let z = parts.next()?.parse().ok()?;
    if parts.next()? != "mca" || parts.next().is_some() {
        return None;
    }
    Some((x, z))
}

fn extract_surface(sections: &[PreparedSection<'_>]) -> Box<[SurfaceCell; 256]> {
    let mut cells = Box::new([SurfaceCell::default(); 256]);
    for z in 0..16 {
        for x in 0..16 {
            let column = z * 16 + x;
            let mut top: Option<(PaletteEntry, i16)> = None;
            let mut water_depth = 0;
            'column: for section in sections {
                if section.air_only {
                    continue;
                }
                for local_y in (0..16).rev() {
                    let Some(block) = section.at(local_y * 256 + column) else {
                        continue;
                    };
                    if block.kind == SurfaceKind::Air {
                        continue;
                    }
                    if top.is_none() {
                        top = Some((*block, i16::from(section.y) * 16 + local_y as i16));
                        if block.kind != SurfaceKind::Water {
                            break 'column;
                        }
                    }
                    if block.kind == SurfaceKind::Water {
                        water_depth += 1;
                    } else {
                        break 'column;
                    }
                }
            }
            if let Some((top, height)) = top {
                cells[column] = SurfaceCell {
                    color: if top.kind == SurfaceKind::Water {
                        depth_to_water_color(water_depth.max(1))
                    } else {
                        top.color
                    },
                    height,
                    present: true,
                };
            }
        }
    }
    cells
}

fn extract_rails(
    sections: &[PreparedSection<'_>],
    chunk_x: i32,
    chunk_z: i32,
    output: &mut HashMap<Pos, Rail>,
) {
    for (section_index, section) in sections.iter().enumerate() {
        if !section.has_rails {
            continue;
        }
        let below_section = sections
            .get(section_index + 1)
            .filter(|below| i16::from(below.y) + 1 == i16::from(section.y));
        for index in 0..4096 {
            let Some(block) = section.at(index) else {
                continue;
            };
            let Some(mut rail) = block.rail else {
                continue;
            };
            let x = index & 15;
            let z = (index >> 4) & 15;
            let y = index >> 8;
            if rail.marker {
                let below = if y == 0 {
                    below_section.and_then(|section| section.at(15 * 256 + z * 16 + x))
                } else {
                    section.at(index - 256)
                };
                rail.marker = below.is_some_and(|block| block.support);
                if !rail.marker {
                    continue;
                }
            }
            output.insert(
                Pos {
                    x: chunk_x * 16 + x as i32,
                    y: i16::from(section.y) * 16 + y as i16,
                    z: chunk_z * 16 + z as i32,
                },
                rail,
            );
        }
    }
}

fn rail_exits(shape: Option<&str>) -> u8 {
    match shape {
        Some("north_south" | "ascending_north" | "ascending_south") => NORTH | SOUTH,
        Some("east_west" | "ascending_east" | "ascending_west") => EAST | WEST,
        Some("north_east") => NORTH | EAST,
        Some("north_west") => NORTH | WEST,
        Some("south_east") => SOUTH | EAST,
        Some("south_west") => SOUTH | WEST,
        _ => 0,
    }
}

fn block_color(name: &str) -> [u8; 3] {
    crate::blocks::palette()
        .get(name)
        .copied()
        .unwrap_or_else(|| fallback_color(name))
}

fn fallback_color(name: &str) -> [u8; 3] {
    let hash = name.bytes().fold(2_166_136_261u32, |hash, byte| {
        (hash ^ u32::from(byte)).wrapping_mul(16_777_619)
    });
    [
        70 + (hash & 63) as u8,
        70 + ((hash >> 8) & 63) as u8,
        70 + ((hash >> 16) & 63) as u8,
    ]
}

fn depth_to_water_color(depth: i16) -> [u8; 3] {
    let banded = ((depth - 1) / 3) * 3 + 1;
    let t = (banded.clamp(1, 30) - 1) as f32 / 29.0;
    [
        (90.0 - t * 80.0) as u8,
        (200.0 - t * 160.0) as u8,
        (245.0 - t * 135.0) as u8,
    ]
}

#[cfg(test)]
mod tests {
    use std::io::{Cursor, Write};
    use std::sync::atomic::{AtomicU64, Ordering};

    use fastanvil::{CompressionScheme, Region};
    use fastnbt::{LongArray, Value};

    use super::*;

    fn compound(entries: impl IntoIterator<Item = (&'static str, Value)>) -> Value {
        Value::Compound(
            entries
                .into_iter()
                .map(|(key, value)| (key.into(), value))
                .collect(),
        )
    }

    fn block(name: &str, properties: &[(&'static str, &str)]) -> Value {
        compound([
            ("Name", Value::String(name.into())),
            (
                "Properties",
                compound(
                    properties
                        .iter()
                        .map(|&(key, value)| (key, Value::String(value.into()))),
                ),
            ),
        ])
    }

    fn packed(indices: &[usize], palette_len: usize) -> Vec<i64> {
        let bits = (usize::BITS - (palette_len - 1).leading_zeros()).max(4) as usize;
        let per_long = 64 / bits;
        let mut data = vec![0u64; 4096usize.div_ceil(per_long)];
        for (index, &value) in indices.iter().enumerate() {
            data[index / per_long] |= (value as u64) << ((index % per_long) * bits);
        }
        data.into_iter().map(|long| long as i64).collect()
    }

    fn chunk_bytes() -> Vec<u8> {
        chunk_bytes_with_support("minecraft:oak_planks")
    }

    fn chunk_bytes_with_support(support_block: &str) -> Vec<u8> {
        let palette = vec![
            block("minecraft:air", &[]),
            block(support_block, &[]),
            block(
                "minecraft:powered_rail",
                &[("powered", "true"), ("shape", "east_west")],
            ),
            block("minecraft:rail", &[("shape", "east_west")]),
            block("minecraft:grass_block", &[]),
            block(
                "minecraft:powered_rail",
                &[("powered", "false"), ("shape", "east_west")],
            ),
        ];
        let mut indices = vec![0; 4096];
        for z in 0..16 {
            for x in 0..16 {
                indices[10 * 256 + z * 16 + x] = 4;
            }
        }
        for x in 1..=4 {
            indices[4 * 256 + 2 * 16 + x] = 1;
            indices[5 * 256 + 2 * 16 + x] = if x == 2 { 3 } else { 2 };
        }
        indices[4 * 256 + 3 * 16 + 1] = 1;
        indices[5 * 256 + 3 * 16 + 1] = 5;
        let data = packed(&indices, palette.len());
        fastnbt::to_bytes(&compound([
            ("DataVersion", Value::Int(5000)),
            (
                "sections",
                Value::List(vec![compound([
                    ("Y", Value::Byte(-1)),
                    (
                        "block_states",
                        compound([
                            ("palette", Value::List(palette)),
                            ("data", Value::LongArray(LongArray::new(data))),
                        ]),
                    ),
                ])]),
            ),
        ]))
        .unwrap()
    }

    struct TempWorld(std::path::PathBuf);

    impl TempWorld {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let sequence = NEXT.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "oakline-test-{}-{sequence}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TempWorld {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn anvil_nbt_graph_and_png_pipeline_handles_negative_regions() {
        let temp = TempWorld::new();
        let mut region = Region::create(Cursor::new(Vec::new())).unwrap();
        region.write_chunk(31, 0, &chunk_bytes()).unwrap();
        fs::write(
            temp.0.join("r.-1.0.mca"),
            region.into_inner().unwrap().into_inner(),
        )
        .unwrap();
        let scan = scan_world(&temp.0, &["minecraft:oak_planks".into()]).unwrap();
        assert_eq!(scan.chunks, 1);
        assert_eq!(scan.skipped_chunks, 0);
        assert_eq!(scan.rails.len(), 4);
        assert!(
            scan.rails[&Pos {
                x: -15,
                y: -11,
                z: 2
            }]
                .marker
        );
        assert!(
            scan.rails[&Pos {
                x: -14,
                y: -11,
                z: 2
            }]
                .ordinary
        );
        assert_eq!(scan.surfaces[&(-1, 0)][2 * 16 + 1].height, -6);
        let graph = crate::graph::build_graph(&scan.rails, 1);
        let (lanes, stations) = crate::graph::trace_lanes(&graph);
        assert_eq!(lanes.len(), 1);
        assert_eq!(stations.len(), 2);
        let bounds = crate::render::rail_bounds(&graph.positions, 16);
        let image =
            crate::render::render_map(&scan.surfaces, &graph, &lanes, &stations, bounds, 1, 3)
                .unwrap();
        let png = temp.0.join("map.png");
        image.save(&png).unwrap();
        assert_eq!(image::open(png).unwrap().to_rgb8(), image);
    }

    #[test]
    fn external_raw_and_zlib_chunks_are_read() {
        for scheme in [CompressionScheme::Uncompressed, CompressionScheme::Zlib] {
            let temp = TempWorld::new();
            let bytes = chunk_bytes();
            let (compression, data) = match scheme {
                CompressionScheme::Uncompressed => (3u8, bytes),
                CompressionScheme::Zlib => {
                    let mut encoder =
                        flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::fast());
                    encoder.write_all(&bytes).unwrap();
                    (2, encoder.finish().unwrap())
                }
                _ => unreachable!(),
            };
            let mut region = vec![0; 3 * 4096];
            region[0..4].copy_from_slice(&[0, 0, 2, 1]);
            region[8192..8196].copy_from_slice(&1u32.to_be_bytes());
            region[8196] = 0x80 | compression;
            fs::write(temp.0.join("r.0.0.mca"), region).unwrap();
            fs::write(temp.0.join("c.0.0.mcc"), data).unwrap();
            let scan = scan_world(&temp.0, &["minecraft:oak_planks".into()]).unwrap();
            assert_eq!(scan.chunks, 1);
            assert_eq!(scan.skipped_chunks, 0);
            assert_eq!(scan.rails.len(), 4);
        }
    }

    #[test]
    fn corrupt_chunk_length_does_not_hide_a_readable_neighbor() {
        let temp = TempWorld::new();
        let mut region = Region::create(Cursor::new(Vec::new())).unwrap();
        region.write_chunk(0, 0, &chunk_bytes()).unwrap();
        region.write_chunk(1, 0, &chunk_bytes()).unwrap();
        let mut bytes = region.into_inner().unwrap().into_inner();
        let offset = u32::from_be_bytes([0, bytes[0], bytes[1], bytes[2]]) as usize * 4096;
        bytes[offset..offset + 4].copy_from_slice(&0u32.to_be_bytes());
        fs::write(temp.0.join("r.0.0.mca"), bytes).unwrap();
        let scan = scan_world(&temp.0, &["minecraft:oak_planks".into()]).unwrap();
        assert_eq!(scan.chunks, 1);
        assert_eq!(scan.skipped_chunks, 1);
        assert!(scan.surfaces.contains_key(&(1, 0)));
    }

    #[test]
    fn support_selection_controls_powered_markers_and_preserves_ordinary_rails() {
        let temp = TempWorld::new();
        let mut region = Region::create(Cursor::new(Vec::new())).unwrap();
        region.write_chunk(0, 0, &chunk_bytes()).unwrap();
        region
            .write_chunk(1, 0, &chunk_bytes_with_support("minecraft:stone"))
            .unwrap();
        fs::write(
            temp.0.join("r.0.0.mca"),
            region.into_inner().unwrap().into_inner(),
        )
        .unwrap();

        for (support, expected_x) in [("minecraft:oak_planks", 1), ("minecraft:stone", 17)] {
            let scan = scan_world(&temp.0, &[support.into()]).unwrap();
            assert_eq!(scan.chunks, 2);
            assert_eq!(scan.rails.len(), 5);
            assert_eq!(scan.rails.values().filter(|rail| rail.marker).count(), 3);
            assert!(
                scan.rails[&Pos {
                    x: expected_x,
                    y: -11,
                    z: 2
                }]
                    .marker
            );
            assert_eq!(scan.rails.values().filter(|rail| rail.ordinary).count(), 2);

            assert!(!scan.rails.contains_key(&Pos {
                x: expected_x,
                y: -11,
                z: 3
            }));
        }

        let scan = scan_world(
            &temp.0,
            &["minecraft:oak_planks".into(), "minecraft:stone".into()],
        )
        .unwrap();
        assert_eq!(scan.rails.len(), 8);
        assert_eq!(scan.rails.values().filter(|rail| rail.marker).count(), 6);
    }

    #[test]
    fn configured_support_is_found_across_a_negative_section_boundary() {
        let chunk = ChunkNbt {
            _data_version: 5000,
            sections: Some(vec![
                ChunkSection {
                    y: -1,
                    block_states: Some(BlockStates {
                        palette: vec![BlockState {
                            name: "minecraft:powered_rail".into(),
                            properties: BlockProperties {
                                powered: Some("true".into()),
                                shape: Some("east_west".into()),
                            },
                        }],
                        data: None,
                    }),
                },
                ChunkSection {
                    y: -2,
                    block_states: Some(BlockStates {
                        palette: vec![BlockState {
                            name: "minecraft:stone".into(),
                            properties: BlockProperties::default(),
                        }],
                        data: None,
                    }),
                },
            ]),
        };
        let sections = prepare_sections(&chunk, &["minecraft:stone".into()]).unwrap();
        let mut rails = HashMap::new();
        extract_rails(&sections, 0, 0, &mut rails);
        assert_eq!(rails.len(), 256);
        assert!(rails.values().all(|rail| rail.marker));
        assert!(rails.keys().all(|position| position.y == -16));

        let sections = prepare_sections(&chunk, &["minecraft:oak_planks".into()]).unwrap();
        let mut rails = HashMap::new();
        extract_rails(&sections, 0, 0, &mut rails);
        assert!(rails.is_empty());
    }

    #[test]
    fn malformed_palette_data_is_rejected_instead_of_fabricating_blocks() {
        let block = |name: &str| BlockState {
            name: name.into(),
            properties: BlockProperties::default(),
        };
        let mut section = ChunkSection {
            y: 0,
            block_states: Some(BlockStates {
                palette: vec![block("minecraft:stone"), block("minecraft:air")],
                data: None,
            }),
        };
        assert!(PreparedSection::new(&section, &["minecraft:oak_planks".into()]).is_err());
        section.block_states.as_mut().unwrap().data = Some(LongArray::new(vec![0]));
        assert!(PreparedSection::new(&section, &["minecraft:oak_planks".into()]).is_err());
    }

    #[test]
    fn five_bit_padded_palette_crosses_long_boundaries_correctly() {
        let palette = (0..17)
            .map(|n| BlockState {
                name: format!("minecraft:custom_{n}"),
                properties: BlockProperties::default(),
            })
            .collect();
        let indices = (0..4096).map(|n| n % 17).collect::<Vec<_>>();
        let section = ChunkSection {
            y: 0,
            block_states: Some(BlockStates {
                palette,
                data: Some(LongArray::new(packed(&indices, 17))),
            }),
        };
        let prepared = PreparedSection::new(&section, &["minecraft:oak_planks".into()]).unwrap();
        for index in [0, 11, 12, 23, 24, 4095] {
            assert_eq!(
                prepared.at(index).unwrap().color,
                block_color(&format!("minecraft:custom_{}", index % 17))
            );
        }
    }

    #[test]
    fn unknown_rail_shape_does_not_create_false_four_way_junctions() {
        assert_eq!(rail_exits(None), 0);
        assert_eq!(rail_exits(Some("future_shape")), 0);
        assert_eq!(rail_exits(Some("north_east")), NORTH | EAST);
    }

    #[test]
    fn lz4_header_lengths_are_checked_before_decompression_allocates() {
        let mut header = vec![0u8; 21];
        header[..8].copy_from_slice(b"LZ4Block");
        header[8] = 0x20;
        header[9..13].copy_from_slice(&(i32::MAX as u32).to_le_bytes());
        header[13..17].copy_from_slice(&1u32.to_le_bytes());
        assert!(decompress(header, 4).is_err());
        // A valid raw LZ4 Java block containing three dots, with its XXHash checksum.
        let block = vec![
            76, 90, 52, 66, 108, 111, 99, 107, 16, 3, 0, 0, 0, 3, 0, 0, 0, 82, 228, 119, 6, 46, 46,
            46,
        ];
        assert_eq!(decompress(block, 4).unwrap(), b"...");
    }

    #[test]
    fn parses_signed_region_coordinates() {
        assert_eq!(
            parse_region_coords(Path::new("r.-12.7.mca")),
            Some((-12, 7))
        );
        assert_eq!(parse_region_coords(Path::new("r.0.0.mca.tmp")), None);
    }
}
