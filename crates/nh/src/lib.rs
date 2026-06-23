use std::{
  path::PathBuf,
  process::{Command, Stdio},
  str::FromStr,
};

use color_eyre::{Result, eyre::Context};
use inferno::collapse::Collapse;
use nh_core::command::{
  Command as NhCommand, ElevationStrategy, ElevationStrategyArg,
};

pub mod interface;
pub mod logging;

pub const NH_VERSION: &str = env!("CARGO_PKG_VERSION");
pub const NH_REV: Option<&str> = option_env!("NH_REV");

pub fn main() -> Result<()> {
  let mut args = <crate::interface::Main as clap::Parser>::parse();

  // Backward compatibility: support NH_ELEVATION_PROGRAM env var if
  // NH_ELEVATION_STRATEGY is not set.
  // TODO: Remove this fallback in a future version
  if args.elevation_strategy.is_none()
    && let Some(old_value) = std::env::var("NH_ELEVATION_PROGRAM")
      .ok()
      .filter(|v| !v.is_empty())
  {
    tracing::warn!(
      "NH_ELEVATION_PROGRAM is deprecated, use NH_ELEVATION_STRATEGY instead. \
       Falling back to NH_ELEVATION_PROGRAM for backward compatibility. \
       Accepted values: none, passwordless, program:<path>"
    );
    match ElevationStrategyArg::from_str(&old_value) {
      Ok(strategy) => args.elevation_strategy = Some(strategy),
      Err(e) => {
        tracing::warn!(
          "Failed to parse NH_ELEVATION_PROGRAM value '{}': {}. Falling back \
           to none.",
          old_value,
          e
        );
      },
    }
  }

  // Set up logging
  crate::logging::setup_logging(args.verbosity)?;
  tracing::debug!("{args:#?}");
  tracing::debug!(%NH_VERSION, ?NH_REV);

  // Check Nix version upfront
  nh_core::checks::verify_nix_environment()?;

  // Once we assert required Nix features, validate NH environment checks
  // For now, this is just NH_* variables being set. More checks may be
  // added to setup_environment in the future.
  nh_core::checks::verify_variables()?;

  let elevation =
    args
      .elevation_strategy
      .as_ref()
      .map_or(ElevationStrategy::Auto, |arg| match arg {
        ElevationStrategyArg::Auto => ElevationStrategy::Auto,
        ElevationStrategyArg::None => ElevationStrategy::None,
        ElevationStrategyArg::Passwordless => ElevationStrategy::Passwordless,
        ElevationStrategyArg::Program(path) => {
          ElevationStrategy::Prefer(path.clone())
        },
      });

  // Handle --usurp: acquire sudo privileges upfront
  if args.usurp && !matches!(elevation, ElevationStrategy::None) {
    tracing::info!("Acquiring sudo privileges for unattended execution...");
    let mut elevate_cmd = NhCommand::self_elevate_cmd(elevation)?;
    let exit_status = elevate_cmd.status()?;
    if exit_status.success() {
      std::process::exit(0);
    } else {
      std::process::exit(exit_status.code().unwrap_or(1));
    }
  }

  // Handle --flamegraph: wrap execution in perf profiling
  if args.flamegraph {
    if Command::new("perf")
      .arg("version")
      .stdout(Stdio::null())
      .status()
      .is_err()
    {
      tracing::warn!(
        "perf not found. Install linux-perf to generate flamegraphs, and run \
         'echo 0 | sudo tee /proc/sys/kernel/perf_event_paranoid' if needed."
      );
    } else {
      run_with_flamegraph()?;
    }
    args.command.run(elevation)
  } else {
    args.command.run(elevation)
  }
}

fn run_with_flamegraph() -> Result<()> {
  let output_svg = PathBuf::from("flamegraph.svg");
  let perf_data = PathBuf::from("/tmp/nh-perf.data");
  let folded_stacks = PathBuf::from("/tmp/nh-folded.txt");

  let exe = std::env::current_exe()
    .with_context(|| "Failed to get current executable")?;
  let nh_args: Vec<String> = std::env::args().skip(1).collect();

  // Run perf record with the nh command
  let perf_record_status = Command::new("perf")
    .arg("record")
    .arg("-o")
    .arg(&perf_data)
    .arg("--call-graph")
    .arg("dwarf")
    .arg(&exe)
    .args(&nh_args)
    .status()
    .with_context(|| "Failed to run perf record")?;

  if !perf_record_status.success() {
    return Err(color_eyre::eyre::eyre!(
      "perf record failed (exit status {:?})",
      perf_record_status
    ));
  }

  // Pipe perf script output to inferno-collapser
  let perf_script_output = Command::new("perf")
    .arg("script")
    .arg("-i")
    .arg(&perf_data)
    .output()
    .with_context(|| "Failed to run perf script")?;

  if !perf_script_output.status.success() {
    return Err(color_eyre::eyre::eyre!(
      "perf script failed (exit status {:?})",
      perf_script_output.status
    ));
  }

  // Collapse perf output to folded stacks using inferno
  let perf_reader = std::io::Cursor::new(&perf_script_output.stdout);
  let folded_file = std::fs::File::create(&folded_stacks)
    .with_context(|| "Failed to create folded stacks file")?;

  let collapse_opts = inferno::collapse::perf::Options::default();
  let mut folder = inferno::collapse::perf::Folder::from(collapse_opts);
  folder
    .collapse(perf_reader, folded_file)
    .with_context(|| "Failed to collapse perf output")?;

  // Generate flamegraph from folded stacks
  let folded_reader = std::fs::File::open(&folded_stacks)
    .with_context(|| "Failed to open folded stacks")?;
  let flamegraph_file = std::fs::File::create(&output_svg)
    .with_context(|| "Failed to create flamegraph output file")?;

  let mut flamegraph_opts = inferno::flamegraph::Options::default();
  inferno::flamegraph::from_reader(
    &mut flamegraph_opts,
    std::io::BufReader::new(folded_reader),
    flamegraph_file,
  )
  .with_context(|| "Failed to generate flamegraph")?;

  tracing::info!("Flamegraph saved to {}", output_svg.display());
  Ok(())
}
