//! Command line (desktop). Android starts with the defaults.

use crate::platform::Insets;
use std::path::PathBuf;

pub const USAGE: &str = "\
Stampede (desktop)

  stampede [options]

Options:
  --screen NAME       start on this screen instead of the splash
  --data-dir DIR      profile and settings directory (default ~/.local/share/stampede)
  --join CODE         behave as if stampede://join#CODE was opened
  --insets L,T,R,B    pretend the screen has these safe-area insets (points)
  --quit-after SECS   close the window after this long (smoke tests)
  --dev               enable the developer hooks in a release build (STAMPEDE_AUTOPILOT, _LAPS, _BOTS, _CAM, ...)

Headless screenshot (no window; needs a GPU adapter, Vulkan or GL):
  stampede --shot NAME --out FILE.png [--size WxH] [--ppp N] [--t SECS] [--insets L,T,R,B]

  --shot NAME         the screen to render (see below)
  --out FILE          PNG to write
  --size WxH          output pixels (default 1600x720; a phone is 2400x1080)
  --ppp N             OS scale factor to pretend (default 1; a phone is 2.625)
  --t SECS            run the screen this long on a virtual 60 Hz clock first (default 1)

  -h, --help          this text
";

#[derive(Clone, Debug, PartialEq)]
pub struct Options {
    /// Start screen name (`screens::by_name`); `None` = splash.
    pub screen: Option<String>,
    pub data_dir: Option<PathBuf>,
    pub join: Option<String>,
    pub insets: Insets,
    pub quit_after: Option<f32>,
    /// `--shot`: render this screen headless.
    pub shot: Option<String>,
    pub out: Option<PathBuf>,
    /// Output size in pixels.
    pub size: (u32, u32),
    /// Pretend OS scale factor.
    pub ppp: f32,
    /// Virtual seconds to run before the shot.
    pub t: f32,
    /// `--dev`: developer hooks on (always on in debug and DIAG builds).
    pub dev: bool,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            screen: None,
            data_dir: None,
            join: None,
            insets: Insets::ZERO,
            quit_after: None,
            shot: None,
            out: None,
            size: (1600, 720),
            ppp: 1.0,
            t: 1.0,
            dev: false,
        }
    }
}

fn parse_size(s: &str) -> Result<(u32, u32), String> {
    let bad = || format!("--size wants WIDTHxHEIGHT, got {s:?}");
    let (w, h) = s.split_once(['x', 'X']).ok_or_else(bad)?;
    let (w, h) = (w.trim().parse::<u32>().map_err(|_| bad())?, h.trim().parse::<u32>().map_err(|_| bad())?);
    if !(64..=8192).contains(&w) || !(64..=8192).contains(&h) {
        return Err(format!("--size {s}: each side must be 64..8192"));
    }
    Ok((w, h))
}

impl Options {
    /// Parse the arguments after the program name. `Err` carries the message
    /// to print (the usage text for `--help`).
    pub fn parse<I: IntoIterator<Item = String>>(args: I) -> Result<Options, String> {
        let mut o = Options::default();
        let mut it = args.into_iter();
        while let Some(a) = it.next() {
            let mut val = |name: &str| it.next().ok_or_else(|| format!("{name} needs a value\n\n{USAGE}"));
            match a.as_str() {
                "-h" | "--help" => return Err(USAGE.to_string()),
                "--dev" => o.dev = true,
                "--screen" => o.screen = Some(val("--screen")?),
                "--shot" => o.shot = Some(val("--shot")?),
                "--out" => o.out = Some(PathBuf::from(val("--out")?)),
                "--data-dir" => o.data_dir = Some(PathBuf::from(val("--data-dir")?)),
                "--join" => o.join = Some(val("--join")?),
                "--size" => o.size = parse_size(&val("--size")?)?,
                "--insets" => o.insets = Insets::parse(&val("--insets")?)?,
                "--ppp" => {
                    let v = val("--ppp")?;
                    o.ppp = v.parse::<f32>().ok().filter(|p| p.is_finite() && *p > 0.0).ok_or_else(|| format!("--ppp wants a positive number, got {v:?}"))?;
                }
                "--t" => {
                    let v = val("--t")?;
                    o.t = v.parse::<f32>().ok().filter(|t| t.is_finite() && *t >= 0.0).ok_or_else(|| format!("--t wants seconds, got {v:?}"))?;
                }
                "--quit-after" => {
                    let v = val("--quit-after")?;
                    o.quit_after = Some(v.parse::<f32>().map_err(|_| format!("--quit-after wants seconds, got {v:?}"))?);
                }
                other => return Err(format!("unknown argument {other:?}\n\n{USAGE}")),
            }
        }
        if o.shot.is_some() && o.out.is_none() {
            return Err(format!("--shot needs --out FILE.png\n\n{USAGE}"));
        }
        Ok(o)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &str) -> Result<Options, String> {
        Options::parse(s.split_whitespace().map(String::from))
    }

    #[test]
    fn shot_options() {
        let o = parse("--shot home --out /tmp/h.png --size 2400x1080 --ppp 2.625 --t 2.5 --insets 48,0,0,0").unwrap();
        assert_eq!(o.shot.as_deref(), Some("home"));
        assert_eq!(o.size, (2400, 1080));
        assert_eq!(o.t, 2.5);
        assert_eq!(o.insets.left, 48.0);
        assert_eq!(Options::default().size, (1600, 720));
    }

    #[test]
    fn errors() {
        assert!(parse("--shot home").is_err(), "needs --out");
        assert!(parse("--size 100").is_err());
        assert!(parse("--bogus").is_err());
        assert!(parse("--ppp").is_err());
        assert!(parse("--help").unwrap_err().contains("--shot"));
    }
}
