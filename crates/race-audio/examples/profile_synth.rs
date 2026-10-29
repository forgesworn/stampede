//! CPU-time (not wall-time) profile of the synthesis, robust against a busy machine.
use race_audio::*;
fn cpu_ms() -> f32 { std::fs::read_to_string("/proc/thread-self/schedstat").unwrap().split_whitespace().next().unwrap().parse::<f64>().unwrap() as f32 / 1e6 }
fn best(mut f: impl FnMut()) -> f32 { (0..15).map(|_| { let t=cpu_ms(); f(); cpu_ms()-t }).fold(1e9, f32::min) }
fn main() {
    let mut tot=0.0;
    for s in Sfx::ALL { let ms=best(||{sfx::render_sfx(s);}); tot+=ms; println!("{:16} {:6.2} ms", s.name(), ms); }
    for s in LoopId::ALL { let ms=best(||{sfx::render_loop(s);}); tot+=ms; println!("{:16} {:6.2} ms", s.name(), ms); }
    println!("TOTAL sfx+loops {tot:.1} ms cpu");
    for t in MusicTrack::ALL { let ms=best(||{music::render_music(t);}); println!("music {} {ms:.1} ms cpu", t.name()); }
}
