//! Time tiled rendering of one page through a single-worker pool:
//! `cargo run --release -p printcraft-render --example tilebench -- drawing.pdf [scale]`.
use printcraft_render::{RenderConfig, RenderPool, RenderRequest, RequestKind, Tile};
use std::sync::Arc;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let Some(path) = args.get(1) else { return eprintln!("usage: tilebench FILE.pdf [scale]") };
    let bytes = match std::fs::read(path) {
        Ok(b) => Arc::new(b),
        Err(e) => return eprintln!("{path}: {e}"),
    };
    let scale: f32 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(4.0);
    let pool = RenderPool::new(bytes, 1, RenderConfig::default());
    let tiles = [(4000u32, 6000u32), (5024, 6000), (4000, 7024), (5024, 7024), (6048, 6000)];
    let reqs = tiles
        .iter()
        .enumerate()
        .map(|(i, (x, y))| RenderRequest {
            page: 0,
            kind: RequestKind::Pixels,
            tile: Some(Tile { x: *x, y: *y, w: 1024, h: 1024 }),
            scale,
            tag: i as u64,
        })
        .collect();
    let start = std::time::Instant::now();
    pool.set_queue(reqs);
    let mut done = 0;
    while done < tiles.len() {
        match pool.try_recv() {
            Some(p) => {
                println!("tile {} at scale {scale}: {} ms {:?}", p.request.tag, p.millis, p.error);
                done += 1;
            }
            None => std::thread::sleep(std::time::Duration::from_millis(2)),
        }
    }
    println!("total {} ms", start.elapsed().as_millis());
}
