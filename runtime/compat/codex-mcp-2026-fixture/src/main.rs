// The build script includes the exact, hash-checked Codex reference source.
// With no explicit reference checkout this compiles a fail-closed stub so the
// ordinary workspace remains self-contained.
include!(concat!(env!("OUT_DIR"), "/fixture.rs"));
