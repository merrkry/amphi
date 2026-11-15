//! Tests of the server on Postgres, with fakes for GitHub, niks3, Nix and
//! the workers. They are ignored unless asked for, since they need
//! `DATABASE_URL` to point at a Postgres server where they can create
//! databases. `mise run test` provides one.

mod fake_github;
mod harness;
mod lifecycle;
mod pipeline;
mod reporting;
mod scheduling;

pub use harness::FakeNix;
