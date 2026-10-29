use anyhow::{ensure, Context, Result};
use std::io::Read;

const LIMIT: usize = 8 * 1024 * 1024;

fn run() -> Result<()> {
    let mut args = std::env::args_os().skip(1);
    let input = args
        .next()
        .context("usage: stampede-component-pack CORE.wasm COMPONENT.wasm")?;
    let output = args.next().context("component output path")?;
    ensure!(args.next().is_none(), "unexpected arguments");
    let file = std::fs::File::open(&input)?;
    ensure!(file.metadata()?.is_file(), "core must be a regular file");
    let mut core = Vec::new();
    file.take(LIMIT as u64 + 1).read_to_end(&mut core)?;
    ensure!(core.len() <= LIMIT, "core exceeds the build limit");
    let mut is_core = false;
    for payload in wasmparser::Parser::new(0).parse_all(&core) {
        match payload? {
            wasmparser::Payload::Version { encoding, .. } => {
                ensure!(
                    encoding == wasmparser::Encoding::Module,
                    "expected a core module"
                );
                is_core = true;
            }
            wasmparser::Payload::ImportSection(imports) => {
                ensure!(imports.count() == 0, "authority core must have no imports");
            }
            _ => {}
        }
    }
    ensure!(is_core, "missing core module");
    let component = wit_component::ComponentEncoder::default()
        .module(&core)?
        .validate(true)
        .encode()?;
    ensure!(
        component.len() <= LIMIT,
        "component exceeds the build limit"
    );
    std::fs::write(output, component)?;
    Ok(())
}

fn main() {
    if let Err(error) = run() {
        eprintln!("{error:#}");
        std::process::exit(1);
    }
}
