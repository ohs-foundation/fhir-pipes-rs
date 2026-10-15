//! `fhir-pipes run`: extract then transform, with the snapshot as the handoff.

use crate::cli::RunArgs;
use crate::error::Result;
use crate::extract::run_extract;
use crate::profile::Profile;
use crate::transform::{check_out_dir, transform_with};

/// The profile is loaded once, for both steps: extract fetches boundaries
/// under its extensions, transform reads its columns.
pub fn run(args: &RunArgs, profile: Option<&str>) -> Result<()> {
    check_out_dir(&args.out)?;
    let profile = Profile::for_snapshot(profile, &args.extract.snapshot)?;
    eprintln!("== extract ==");
    run_extract(&args.extract, &profile.location.boundary_extensions)?;
    eprintln!("== transform ==");
    transform_with(&args.transform_args(), &profile)
}
