use sift_wm::{Config, Engine, Monitor, Rect, Window};
use std::time::Instant;
fn main() {
    for count in [10, 50, 200, 1000] {
        let mut engine = Engine::new(
            Config {
                automatic_tiling: true,
                ..Config::default()
            },
            vec![Monitor {
                id: 1,
                name: "benchmark".into(),
                main: true,
                frame: Rect::new(0., 0., 3840., 2160.),
                usable: Rect::new(0., 24., 3840., 2100.),
            }],
        )
        .expect("model");
        for id in 1..=count {
            engine
                .add_window(Window::new(id, 10, Rect::new(0., 24., 500., 400.)))
                .expect("window");
        }
        let mut times = Vec::new();
        for _ in 0..1000 {
            let start = Instant::now();
            std::hint::black_box(engine.reflow());
            times.push(start.elapsed());
        }
        times.sort();
        println!(
            "{count} windows: layout median {:?}, p95 {:?}",
            times[500], times[950]
        );
    }
}
