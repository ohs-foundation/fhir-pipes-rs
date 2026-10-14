//! `fhir-pipes run`: extract then transform, with the snapshot as the handoff.

use crate::cli::RunArgs;
use crate::error::Result;
use crate::extract::run_extract;
use crate::transform::{check_out_dir, run_transform};

pub fn run(args: &RunArgs) -> Result<()> {
    check_out_dir(&args.out)?;
    eprintln!("== extract ==");
    run_extract(&args.extract)?;
    eprintln!("== transform ==");
    run_transform(&args.transform_args())
}
