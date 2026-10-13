use std::collections::{HashMap, HashSet, VecDeque};

use crate::model::{Graph, Lane, Pos, Rail};

const NORTH: u8 = 1;
const EAST: u8 = 2;
const SOUTH: u8 = 4;
const WEST: u8 = 8;

pub fn build_graph(rails: &HashMap<Pos, Rail>, gap: u16) -> Graph {
    let mut positions = rails
        .iter()
        .filter(|(_, rail)| rail.marker || rail.ordinary)
        .map(|(&pos, _)| pos)
        .collect::<Vec<_>>();
    positions.sort_unstable();
    let candidates = positions.iter().map(|pos| rails[pos]).collect::<Vec<_>>();
    let index = positions
        .iter()
        .enumerate()
        .map(|(i, &pos)| (pos, i))
        .collect::<HashMap<_, _>>();
    let mut all_edges = (0..positions.len())
        .map(|_| Vec::with_capacity(2))
        .collect::<Vec<_>>();

    for (i, &pos) in positions.iter().enumerate() {
        let rail = candidates[i];
        // Visit only east/south interfaces, adding each undirected edge once.
        for (direction, dx, dz, opposite) in [(EAST, 1, 0, WEST), (SOUTH, 0, 1, NORTH)] {
            if rail.exits & direction == 0 {
                continue;
            }
            // Shared endpoints must have the same height. Only the ascending
            // exit is raised; flat tracks at different heights cannot connect.
            let Some(endpoint_y) = pos
                .y
                .checked_add(i16::from(rail.ascending & direction != 0))
            else {
                continue;
            };
            let (Some(x), Some(z)) = (pos.x.checked_add(dx), pos.z.checked_add(dz)) else {
                continue;
            };
            for y in [Some(endpoint_y), endpoint_y.checked_sub(1)]
                .into_iter()
                .flatten()
            {
                let neighbor = Pos { x, y, z };
                let Some(&j) = index.get(&neighbor) else {
                    continue;
                };
                let next_rail = candidates[j];
                if next_rail.exits & opposite != 0
                    && y.checked_add(i16::from(next_rail.ascending & opposite != 0))
                        == Some(endpoint_y)
                {
                    all_edges[i].push(j);
                    all_edges[j].push(i);
                }
            }
        }
    }
    for neighbors in &mut all_edges {
        neighbors.sort_unstable();
    }

    // A wider sentinel avoids admitting unreachable rails when gap == u16::MAX.
    let mut distance = vec![u32::MAX; positions.len()];
    let mut queue = VecDeque::new();
    for (i, rail) in candidates.iter().enumerate() {
        if rail.marker {
            distance[i] = 0;
            queue.push_back(i);
        }
    }
    let gap = u32::from(gap);
    while let Some(node) = queue.pop_front() {
        if distance[node] == gap {
            continue;
        }
        for &next in &all_edges[node] {
            if distance[next] == u32::MAX {
                distance[next] = distance[node] + 1;
                queue.push_back(next);
            }
        }
    }

    let mut remap = vec![usize::MAX; positions.len()];
    let mut kept_positions = Vec::new();
    for (old, &pos) in positions.iter().enumerate() {
        if distance[old] <= gap {
            remap[old] = kept_positions.len();
            kept_positions.push(pos);
        }
    }
    let mut edges = Vec::with_capacity(kept_positions.len());
    let mut stations = Vec::new();
    for (old, mut neighbors) in all_edges.into_iter().enumerate() {
        if remap[old] == usize::MAX {
            continue;
        }
        let original_degree = neighbors.len();
        neighbors.retain(|&neighbor| remap[neighbor] != usize::MAX);
        // A search boundary is not a physical track endpoint. Switches split
        // paths later, but are not automatically labeled as stations either.
        if original_degree <= 1 && neighbors.len() == original_degree {
            stations.push(remap[old]);
        }
        for neighbor in &mut neighbors {
            *neighbor = remap[*neighbor];
        }

        edges.push(neighbors);
    }

    Graph {
        positions: kept_positions,
        edges,
        stations,
    }
}

pub fn trace_lanes(graph: &Graph) -> (Vec<Lane>, Vec<usize>) {
    let mut visited = EdgeVisits::new(graph);
    let mut paths = Vec::new();

    for (start, neighbors) in graph.edges.iter().enumerate() {
        if neighbors.len() == 2 {
            continue;
        }
        for slot in 0..neighbors.len() {
            if !visited.seen(start, slot) {
                paths.push(walk_path(graph, start, slot, &mut visited));
            }
        }
    }

    // Closed loops have no endpoints, so the first traversal cannot reach them.
    for start in 0..graph.positions.len() {
        for slot in 0..graph.edges[start].len() {
            if !visited.seen(start, slot) {
                paths.push(walk_path(graph, start, slot, &mut visited));
            }
        }
    }

    let mut colors = HashSet::with_capacity(paths.len());
    let lanes = paths
        .into_iter()
        .filter(|path| path.len() >= 2)
        .enumerate()
        .map(|(number, path)| Lane {
            path,
            color: lane_color(number, &mut colors),
        })
        .collect();
    (lanes, graph.stations.clone())
}

struct EdgeVisits {
    offsets: Vec<usize>,
    visited: Vec<u64>,
}

impl EdgeVisits {
    fn new(graph: &Graph) -> Self {
        let mut offsets = Vec::with_capacity(graph.edges.len() + 1);
        offsets.push(0);
        for neighbors in &graph.edges {
            offsets.push(offsets.last().copied().unwrap() + neighbors.len());
        }
        let visited = vec![0; offsets.last().copied().unwrap().div_ceil(64)];
        Self { offsets, visited }
    }

    fn seen(&self, node: usize, slot: usize) -> bool {
        let edge = self.offsets[node] + slot;
        self.visited[edge / 64] & (1 << (edge % 64)) != 0
    }

    fn mark(&mut self, graph: &Graph, node: usize, slot: usize) {
        self.mark_slot(node, slot);
        let neighbor = graph.edges[node][slot];
        if let Some(reverse) = graph.edges[neighbor]
            .iter()
            .position(|&other| other == node)
        {
            self.mark_slot(neighbor, reverse);
        }
    }

    fn mark_slot(&mut self, node: usize, slot: usize) {
        let edge = self.offsets[node] + slot;
        self.visited[edge / 64] |= 1 << (edge % 64);
    }
}

fn walk_path(
    graph: &Graph,
    start: usize,
    first_slot: usize,
    visited: &mut EdgeVisits,
) -> Vec<usize> {
    let mut path = vec![start];
    let mut current = start;
    let mut slot = first_slot;
    loop {
        if visited.seen(current, slot) {
            break;
        }
        let previous = current;
        visited.mark(graph, current, slot);
        current = graph.edges[current][slot];
        path.push(current);
        if current == start || graph.edges[current].len() != 2 {
            break;
        }
        slot = usize::from(graph.edges[current][0] == previous);
    }
    path
}

fn lane_color(number: usize, used: &mut HashSet<[u8; 3]>) -> [u8; 3] {
    let hue = (number as f64 * 137.507_764_05 + 8.0) % 360.0;
    let mut color = hsl_to_rgb(hue, 0.86, 0.55);
    // Quantizing a golden-angle hue eventually repeats RGB values. An odd
    // stride visits every 24-bit color, resolving collisions deterministically.
    let mut packed = u32::from_be_bytes([0, color[0], color[1], color[2]]);
    while !used.insert(color) {
        packed = packed.wrapping_add(0x009e_3779) & 0x00ff_ffff;
        let bytes = packed.to_be_bytes();
        color = [bytes[1], bytes[2], bytes[3]];
        // More lanes than RGB colors cannot all have a unique color.
        if used.len() == 1 << 24 {
            break;
        }
    }
    color
}

fn hsl_to_rgb(hue: f64, saturation: f64, lightness: f64) -> [u8; 3] {
    let chroma = (1.0 - (2.0 * lightness - 1.0).abs()) * saturation;
    let h = hue / 60.0;
    let x = chroma * (1.0 - (h % 2.0 - 1.0).abs());
    let (r, g, b) = match h as u8 {
        0 => (chroma, x, 0.0),
        1 => (x, chroma, 0.0),
        2 => (0.0, chroma, x),
        3 => (0.0, x, chroma),
        4 => (x, 0.0, chroma),
        _ => (chroma, 0.0, x),
    };
    let m = lightness - chroma / 2.0;
    [
        ((r + m) * 255.0).round() as u8,
        ((g + m) * 255.0).round() as u8,
        ((b + m) * 255.0).round() as u8,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rail(marker: bool, exits: u8) -> Rail {
        Rail {
            exits,
            ascending: 0,
            marker,
            ordinary: !marker,
        }
    }

    #[test]
    fn ordinary_rails_bridge_a_small_gap() {
        let mut rails = HashMap::new();
        for x in 0..=5 {
            rails.insert(Pos { x, y: 20, z: 0 }, rail(x == 0 || x == 5, EAST | WEST));
        }
        let graph = build_graph(&rails, 4);
        assert_eq!(graph.positions.len(), 6);
        let (lanes, stations) = trace_lanes(&graph);
        assert_eq!(lanes.len(), 1);
        assert_eq!(stations.len(), 2);
        assert_eq!(lanes[0].path.len(), 6);
    }

    #[test]
    fn gap_limit_drops_distant_ordinary_rails() {
        let mut rails = HashMap::new();
        for x in 0..=10 {
            rails.insert(Pos { x, y: 20, z: 0 }, rail(x == 0, EAST | WEST));
        }
        let graph = build_graph(&rails, 3);
        assert_eq!(graph.positions.len(), 4);
        assert_eq!(graph.positions.last().unwrap().x, 3);
        let (lanes, stations) = trace_lanes(&graph);
        assert_eq!(lanes.len(), 1);
        assert_eq!(stations, vec![0]);
    }

    #[test]
    fn maximum_gap_does_not_admit_unconnected_rails() {
        let rails = HashMap::from([
            (Pos { x: 0, y: 0, z: 0 }, rail(true, EAST | WEST)),
            (Pos { x: 9, y: 0, z: 0 }, rail(false, EAST | WEST)),
        ]);
        let graph = build_graph(&rails, u16::MAX);
        assert_eq!(graph.positions.len(), 1);
    }

    #[test]
    fn neighboring_flat_rails_at_different_heights_do_not_connect() {
        let rails = HashMap::from([
            (Pos { x: 0, y: 0, z: 0 }, rail(true, EAST | WEST)),
            (Pos { x: 1, y: 1, z: 0 }, rail(true, EAST | WEST)),
        ]);
        let graph = build_graph(&rails, 0);
        assert!(graph.edges.iter().all(Vec::is_empty));
    }

    #[test]
    fn every_ascending_shape_connects_only_at_its_raised_exit() {
        for (ascending, dx, dz, exits) in [
            (NORTH, 0, -1, NORTH | SOUTH),
            (EAST, 1, 0, EAST | WEST),
            (SOUTH, 0, 1, NORTH | SOUTH),
            (WEST, -1, 0, EAST | WEST),
        ] {
            let origin = Pos { x: 0, y: 0, z: 0 };
            let high = Pos { x: dx, y: 1, z: dz };
            let low = Pos {
                x: -dx,
                y: 0,
                z: -dz,
            };
            let incorrect_high = Pos { x: dx, y: 0, z: dz };
            let incorrect_low = Pos {
                x: -dx,
                y: 1,
                z: -dz,
            };
            let mut slope = rail(true, exits);
            slope.ascending = ascending;
            let rails = HashMap::from([
                (origin, slope),
                (high, rail(true, exits)),
                (low, rail(true, exits)),
                (incorrect_high, rail(true, exits)),
                (incorrect_low, rail(true, exits)),
            ]);
            let graph = build_graph(&rails, 0);
            let index = graph
                .positions
                .iter()
                .position(|&pos| pos == origin)
                .unwrap();
            let neighbors = graph.edges[index]
                .iter()
                .map(|&node| graph.positions[node])
                .collect::<HashSet<_>>();
            assert_eq!(neighbors, HashSet::from([low, high]), "shape {ascending}");
        }
    }

    #[test]
    fn ordinary_curve_bridges_marker_tracks() {
        let rails = HashMap::from([
            (Pos { x: 0, y: 0, z: 0 }, rail(true, EAST | WEST)),
            (Pos { x: 1, y: 0, z: 0 }, rail(false, SOUTH | WEST)),
            (Pos { x: 1, y: 0, z: 1 }, rail(true, NORTH | SOUTH)),
        ]);
        let graph = build_graph(&rails, 1);
        let (lanes, stations) = trace_lanes(&graph);
        assert_eq!(lanes.len(), 1);
        assert_eq!(lanes[0].path.len(), 3);
        assert_eq!(stations.len(), 2);
    }

    #[test]
    fn consecutive_slopes_connect_at_their_matching_heights() {
        let mut slope = rail(true, EAST | WEST);
        slope.ascending = EAST;
        let rails = HashMap::from([
            (Pos { x: 0, y: 0, z: 0 }, slope),
            (Pos { x: 1, y: 1, z: 0 }, slope),
            (Pos { x: 2, y: 2, z: 0 }, rail(true, EAST | WEST)),
            (Pos { x: 1, y: 0, z: 0 }, rail(true, EAST | WEST)),
        ]);
        let graph = build_graph(&rails, 0);
        let (lanes, _) = trace_lanes(&graph);
        assert_eq!(lanes.len(), 1);
        assert_eq!(lanes[0].path.len(), 3);
        let heights = lanes[0]
            .path
            .iter()
            .map(|&node| graph.positions[node].y)
            .collect::<Vec<_>>();
        assert_eq!(heights, vec![0, 1, 2]);
    }

    #[test]
    fn long_tracks_cross_bitset_words_once() {
        let rails = (0..130)
            .map(|x| (Pos { x, y: 0, z: 0 }, rail(true, EAST | WEST)))
            .collect();
        let graph = build_graph(&rails, 0);
        let (lanes, stations) = trace_lanes(&graph);
        assert_eq!(lanes.len(), 1);
        assert_eq!(lanes[0].path.len(), 130);
        assert_eq!(stations, vec![0, 129]);
    }

    #[test]
    fn loops_are_drawn_once_without_station_markers() {
        let rails = HashMap::from([
            (Pos { x: 0, y: 0, z: 0 }, rail(true, EAST | SOUTH)),
            (Pos { x: 1, y: 0, z: 0 }, rail(false, WEST | SOUTH)),
            (Pos { x: 1, y: 0, z: 1 }, rail(false, WEST | NORTH)),
            (Pos { x: 0, y: 0, z: 1 }, rail(false, EAST | NORTH)),
        ]);
        let graph = build_graph(&rails, 2);
        let (lanes, stations) = trace_lanes(&graph);
        assert_eq!(lanes.len(), 1);
        assert_eq!(lanes[0].path.len(), 5);
        assert_eq!(lanes[0].path.first(), lanes[0].path.last());
        assert!(stations.is_empty());
    }

    #[test]
    fn junctions_split_paths_without_becoming_stations() {
        let graph = Graph {
            positions: (0..4).map(|x| Pos { x, y: 0, z: 0 }).collect(),
            edges: vec![vec![1, 2, 3], vec![0], vec![0], vec![0]],
            stations: vec![1, 2, 3],
        };
        let (lanes, stations) = trace_lanes(&graph);
        assert_eq!(lanes.len(), 3);
        assert_eq!(stations, vec![1, 2, 3]);
    }

    #[test]
    fn lane_colors_do_not_repeat_after_rgb_quantization() {
        let mut used = HashSet::new();
        for number in 0..10_000 {
            lane_color(number, &mut used);
        }
        assert_eq!(used.len(), 10_000);
    }

    #[test]
    fn adjacent_parallel_tracks_do_not_merge() {
        let mut rails = HashMap::new();
        for x in 0..=2 {
            rails.insert(Pos { x, y: 0, z: 0 }, rail(true, EAST | WEST));
            rails.insert(Pos { x, y: 0, z: 1 }, rail(true, EAST | WEST));
        }
        let graph = build_graph(&rails, 0);
        assert!(graph.edges.iter().all(|neighbors| neighbors.len() <= 2));
        let (lanes, stations) = trace_lanes(&graph);
        assert_eq!(lanes.len(), 2);
        assert_eq!(stations.len(), 4);
    }
}
