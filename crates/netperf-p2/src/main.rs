use cling::prelude::*;
use netperf_p2::common::opts::Opts;

#[tokio::main(flavor = "current_thread")]
async fn main() -> ClingFinished<Opts> {
    Cling::parse_and_run().await
}
