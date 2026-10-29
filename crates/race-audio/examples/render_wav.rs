//! Write every sound to WAV files so you can listen to them.
//!
//!     cargo run -p race-audio --example render_wav --release -- <out_dir>
//!
//! Writes `sfx_<name>.wav`, `loop_<name>.wav` (4 s), `music_<name>.wav` (20 s of the
//! loop, repeated if the loop is shorter), plus `coin_chain.wav` (eight coins in a
//! quick chain) and `footsteps_ramp.wav` (the footsteps loop at rising pitch).

use race_audio::dsp::{resample, SAMPLE_RATE};
use race_audio::{coin_pitch, music, sfx, LoopId, MusicTrack, Sfx};
use std::io::Write;
use std::path::Path;
use std::time::Instant;

fn write_wav(path: &Path, samples: &[f32]) -> std::io::Result<()> {
    let data_len = (samples.len() * 2) as u32;
    let mut f = std::io::BufWriter::new(std::fs::File::create(path)?);
    f.write_all(b"RIFF")?;
    f.write_all(&(36 + data_len).to_le_bytes())?;
    f.write_all(b"WAVEfmt ")?;
    f.write_all(&16u32.to_le_bytes())?; // PCM chunk size
    f.write_all(&1u16.to_le_bytes())?; // PCM
    f.write_all(&1u16.to_le_bytes())?; // mono
    f.write_all(&SAMPLE_RATE.to_le_bytes())?;
    f.write_all(&(SAMPLE_RATE * 2).to_le_bytes())?; // byte rate
    f.write_all(&2u16.to_le_bytes())?; // block align
    f.write_all(&16u16.to_le_bytes())?; // bits
    f.write_all(b"data")?;
    f.write_all(&data_len.to_le_bytes())?;
    for s in samples {
        f.write_all(&((s.clamp(-1.0, 1.0) * 32767.0).round() as i16).to_le_bytes())?;
    }
    f.flush()
}

fn repeat_to(buf: &[f32], secs: f32) -> Vec<f32> {
    let want = (secs * SAMPLE_RATE as f32) as usize;
    buf.iter().cycle().take(want).copied().collect()
}

fn main() -> std::io::Result<()> {
    let dir = std::env::args().nth(1).expect("usage: render_wav <out_dir>");
    let dir = Path::new(&dir);
    std::fs::create_dir_all(dir)?;

    let t0 = Instant::now();
    let sfx_bufs: Vec<(Sfx, Vec<f32>)> = Sfx::ALL.iter().map(|&s| (s, sfx::render_sfx(s))).collect();
    let loop_bufs: Vec<(LoopId, Vec<f32>)> = LoopId::ALL.iter().map(|&l| (l, sfx::render_loop(l))).collect();
    let sfx_ms = t0.elapsed().as_secs_f32() * 1000.0;
    println!("sfx + loops synthesised in {sfx_ms:.1} ms");

    for (s, b) in &sfx_bufs {
        write_wav(&dir.join(format!("sfx_{}.wav", s.name())), b)?;
    }
    for (l, b) in &loop_bufs {
        write_wav(&dir.join(format!("loop_{}.wav", l.name())), &repeat_to(b, 4.0))?;
    }
    for t in MusicTrack::ALL {
        let t1 = Instant::now();
        let b = music::render_music(t);
        println!("music {} synthesised in {:.1} ms ({:.1} s loop)", t.name(), t1.elapsed().as_secs_f32() * 1000.0, b.len() as f32 / SAMPLE_RATE as f32);
        write_wav(&dir.join(format!("music_{}.wav", t.name())), &repeat_to(&b, 20.0))?;
    }

    // Eight coins 90 ms apart, each a step higher.
    let coin = &sfx_bufs.iter().find(|(s, _)| *s == Sfx::Coin).unwrap().1;
    let mut chain = vec![0.0f32; SAMPLE_RATE as usize * 2];
    for c in 0..8u32 {
        let start = (c as f32 * 0.09 * SAMPLE_RATE as f32) as usize;
        for (i, x) in resample(coin, coin_pitch(c)).iter().enumerate() {
            if start + i < chain.len() {
                chain[start + i] += x * 0.55;
            }
        }
    }
    write_wav(&dir.join("coin_chain.wav"), &chain)?;

    // Footsteps from a slow jog to a flat-out sprint.
    let steps = &loop_bufs.iter().find(|(l, _)| *l == LoopId::Footsteps).unwrap().1;
    let mut ramp = Vec::new();
    for pitch in [0.6f32, 0.9, 1.2, 1.6, 2.0] {
        ramp.extend(resample(&repeat_to(steps, 2.0), pitch).into_iter().take(SAMPLE_RATE as usize));
    }
    write_wav(&dir.join("footsteps_ramp.wav"), &ramp)?;

    println!("wrote {} files to {}", sfx_bufs.len() + loop_bufs.len() + MusicTrack::ALL.len() + 2, dir.display());
    Ok(())
}
