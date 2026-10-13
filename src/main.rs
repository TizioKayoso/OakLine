mod blocks;
mod graph;
mod model;
mod render;
mod world;

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::Parser;

use graph::{build_graph, trace_lanes};
use render::{output_scale, rail_bounds, render_map};
use world::scan_world;

#[derive(Parser, Debug)]
#[command(version, about)]
struct Args {
    #[arg(help = "Path to a Java Edition world save. Required unless --list-blocks is used.")]
    save: Option<PathBuf>,

    #[arg(
        short,
        long,
        default_value = "oakline.png",
        help = "Destination PNG file."
    )]
    output: PathBuf,

    #[arg(
        help = "Blocks directly below active powered rails. Repeat the flag or use a comma-separated list; names come from blocks.json.",
        long = "support-block",
        value_name = "BLOCK",
        value_delimiter = ',',
        default_value = "minecraft:oak_planks",
        value_parser = blocks::parse_block_id
    )]
    support_blocks: Vec<String>,

    #[arg(
        long,
        help = "Print every supported block ID from blocks.json, then exit."
    )]
    list_blocks: bool,

    #[arg(long, default_value_t = 16, value_parser = clap::value_parser!(u16).range(0..=256), help = "Maximum number of ordinary rail blocks included beyond a detected powered-rail backbone. Increase this if curves remain disconnected.")]
    gap: u16,

    #[arg(long, default_value_t = 128, value_parser = clap::value_parser!(i32).range(0..), help = "Surface-map padding around the detected railway, in blocks.")]
    padding: i32,

    #[arg(long, default_value_t = 8192, value_parser = clap::value_parser!(u32).range(256..), help = "Maximum width or height of the PNG. Larger maps are reduced by an integer factor so block pixels remain sharp.")]
    max_size: u32,

    #[arg(long, default_value_t = 3, value_parser = clap::value_parser!(u32).range(1..=16), help = "Width of a colored lane in output pixels.")]
    line_width: u32,
}

fn main() -> Result<()> {
    let mut args = Args::parse();
    if args.list_blocks {
        for block in blocks::ids() {
            println!("{block}");
        }
        return Ok(());
    }
    args.support_blocks.sort_unstable();
    args.support_blocks.dedup();
    let save = require_save(args.save.as_deref())?;
    let region_dir = save.join("region");
    if !region_dir.is_dir() {
        bail!(
            "{} is not an Overworld Java save: the region directory is missing",
            save.display()
        );
    }

    eprintln!("Scanning {} ...", region_dir.display());
    let mut scan = scan_world(&region_dir, &args.support_blocks)?;
    eprintln!(
        "Read {} chunks and found {} rail blocks ({} read errors).",
        scan.chunks,
        scan.rails.len(),
        scan.skipped_chunks
    );

    let graph = build_graph(&scan.rails, args.gap);
    drop(scan.rails);
    if graph.positions.is_empty() {
        bail!(
            "no active powered rails on the selected support blocks ({}) were found; check that the world was saved while the rails were powered",
            args.support_blocks.join(", ")
        );
    }

    let (lanes, stations) = trace_lanes(&graph);
    if lanes.is_empty() {
        bail!("the detected backbone did not contain a complete lane; try increasing --gap");
    }

    let bounds = rail_bounds(&graph.positions, args.padding);
    let scale = output_scale(bounds, args.max_size);
    // Keep northern/western samples outside the crop for terrain shading.
    let first_x = (i64::from(bounds.min_x) - scale as i64).div_euclid(16);
    let first_z = (i64::from(bounds.min_z) - scale as i64).div_euclid(16);
    scan.surfaces.retain(|&(x, z), _| {
        i64::from(x) >= first_x
            && x <= bounds.max_x.div_euclid(16)
            && i64::from(z) >= first_z
            && z <= bounds.max_z.div_euclid(16)
    });
    eprintln!(
        "Rendering {} lanes and {} stations at 1 pixel per {} block{} ...",
        lanes.len(),
        stations.len(),
        scale,
        if scale == 1 { "" } else { "s" }
    );
    let image = render_map(
        &scan.surfaces,
        &graph,
        &lanes,
        &stations,
        bounds,
        scale,
        args.line_width,
    )?;
    image
        .save(&args.output)
        .with_context(|| format!("could not write {}", args.output.display()))?;

    eprintln!(
        "Wrote {} ({}x{}).",
        args.output.display(),
        image.width(),
        image.height()
    );
    for (number, &node) in stations.iter().enumerate() {
        let p = graph.positions[node];
        eprintln!("  S{}: x={}, y={}, z={}", number + 1, p.x, p.y, p.z);
    }
    Ok(())
}

fn require_save(save: Option<&Path>) -> Result<&Path> {
    let Some(save) = save else {
        bail!(
            "no save was given; pass a Java world directory: oakline \"/path/to/world\" --output oakline.png"
        );
    };
    if save
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("png"))
        && !save.is_dir()
    {
        bail!(
            "no save was given; {} is a PNG output path. Pass a world directory and use --output for the PNG",
            save.display()
        );
    }
    Ok(save)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_save_and_positional_png_get_the_requested_error() {
        for argv in [
            vec!["oakline"],
            vec!["oakline", "--output", "oakline.png"],
            vec!["oakline", "oakline.png"],
            vec!["oakline", "/missing/path/OAKLINE.PNG"],
        ] {
            let args = Args::try_parse_from(argv).unwrap();
            let error = require_save(args.save.as_deref()).unwrap_err();
            assert!(error.to_string().starts_with("no save was given"));
        }
    }

    #[test]
    fn list_blocks_does_not_need_a_save() {
        let args = Args::try_parse_from(["oakline", "--list-blocks"]).unwrap();
        assert!(args.list_blocks);
        assert!(args.save.is_none());
    }

    #[test]
    fn support_blocks_default_to_oak_and_can_be_replaced_or_combined() {
        let args = Args::try_parse_from(["oakline", "/world"]).unwrap();
        assert_eq!(args.support_blocks, ["minecraft:oak_planks"]);
        let args = Args::try_parse_from(["oakline", "/world", "--support-block", "stone"]).unwrap();
        assert_eq!(args.support_blocks, ["minecraft:stone"]);
        let args = Args::try_parse_from([
            "oakline",
            "/world",
            "--support-block",
            "stone,spruce_planks",
            "--support-block",
            "minecraft:oak_planks",
        ])
        .unwrap();
        assert_eq!(
            args.support_blocks,
            [
                "minecraft:stone",
                "minecraft:spruce_planks",
                "minecraft:oak_planks"
            ]
        );
        assert!(
            Args::try_parse_from(["oakline", "/world", "--support-block", "typo_block"]).is_err()
        );
    }

    #[test]
    fn existing_directory_named_like_a_png_can_still_be_a_save() {
        let temp = std::env::temp_dir().join(format!(
            "oakline-save-name-{}-{}.png",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&temp).unwrap();
        let result = require_save(Some(&temp)).map(Path::to_path_buf);
        std::fs::remove_dir(&temp).unwrap();
        assert_eq!(result.unwrap(), temp);
    }
}
