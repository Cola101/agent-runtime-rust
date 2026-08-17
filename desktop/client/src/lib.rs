//! The desktop shell's one connection to a Runtime.
//!
//! Every surface — chat today, mail or a board later — reaches the Runtime
//! through this and nothing else. A surface with its own transport would be a
//! second path, and a second path is the shape of defect this project keeps
//! finding: the local IPC adapter carried a check the network path did not,
//! so the network path's gap stayed invisible until something used it.
//!
//! Building from a workspace that is not the Runtime's is itself the point.
//! ADR-0123 claims the invocation contract is consumable by an outside caller;
//! until this crate existed, every consumer of it lived inside
//! `runtime/Cargo.toml` and shared its dependency resolution.

use agent_runtime_invocation_protocol::v1::runtime_invocation_client::RuntimeInvocationClient;
use agent_runtime_invocation_protocol::v1::{RunLifecycleBoundary, run_lifecycle_boundary};
use tonic::transport::Channel;

/// What a surface is allowed to know about a Run's position in its lifecycle.
///
/// Deliberately a translation of the wire boundary rather than a re-derivation
/// from the event list. The Runtime decides when a Run is over; a client that
/// concludes it from the last event it happened to receive will be wrong at
/// exactly the moments that matter — a retired log, a replaced host, a Run
/// parked on a person.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Lifecycle {
    Running,
    Cancelling,
    WaitingApproval,
    Suspended,
    Interrupted,
    /// Finished, with the status the Runtime assigned.
    Terminal(String),
    /// Finished, and the hot events are gone. The outcome is still known.
    Retired(String),
    /// The wire carried a boundary this build does not understand.
    ///
    /// Not an error and not a guess: a newer Runtime may name a boundary this
    /// client predates. Showing "unrecognised" is honest; mapping it onto
    /// `Running` or `Terminal` would be a lie in whichever direction is wrong.
    Unrecognised,
}

impl Lifecycle {
    #[must_use]
    pub fn from_wire(boundary: &RunLifecycleBoundary) -> Self {
        use run_lifecycle_boundary::Boundary;
        match boundary.boundary.as_ref() {
            Some(Boundary::Running(_)) => Self::Running,
            Some(Boundary::Cancelling(_)) => Self::Cancelling,
            Some(Boundary::WaitingApproval(_)) => Self::WaitingApproval,
            Some(Boundary::Suspended(_)) => Self::Suspended,
            Some(Boundary::Interrupted(_)) => Self::Interrupted,
            Some(Boundary::Terminal(terminal)) => Self::Terminal(terminal.status.clone()),
            Some(Boundary::Retired(retired)) => Self::Retired(retired.status.clone()),
            None => Self::Unrecognised,
        }
    }

    /// Whether the shell should stop following this Run.
    ///
    /// The one question the transcript view asks, answered from the reported
    /// boundary alone.
    #[must_use]
    pub const fn is_over(&self) -> bool {
        matches!(self, Self::Terminal(_) | Self::Retired(_))
    }
}

/// Connects the shell to a Runtime.
///
/// Plaintext is accepted only for a loopback address the shell started itself.
/// Anything else must be mTLS, because the Runtime refuses to serve a network
/// surface without it and a client that would happily talk plaintext to a
/// remote host is a client that will eventually be pointed at one.
pub async fn connect(endpoint: String) -> Result<RuntimeInvocationClient<Channel>, tonic::transport::Error> {
    RuntimeInvocationClient::connect(endpoint).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_runtime_invocation_protocol::v1::run_lifecycle_boundary as wire;

    fn wrap(boundary: wire::Boundary) -> RunLifecycleBoundary {
        RunLifecycleBoundary {
            boundary: Some(boundary),
        }
    }

    /// The contract is reachable, and its types resolve, from a workspace that
    /// is not the Runtime's. This is the property ADR-0123 asserted and could
    /// not previously demonstrate.
    #[test]
    fn the_invocation_contract_is_usable_from_an_independent_workspace() {
        let terminal = Lifecycle::from_wire(&wrap(wire::Boundary::Terminal(wire::Terminal {
            status: "succeeded".into(),
        })));

        assert_eq!(terminal, Lifecycle::Terminal("succeeded".into()));
        assert!(terminal.is_over());
    }

    /// A parked Run is not a finished Run. The shell keeps following it, which
    /// is why this distinction lives in the type rather than in a view.
    #[test]
    fn a_run_waiting_on_a_person_is_not_over() {
        let waiting = Lifecycle::from_wire(&wrap(wire::Boundary::WaitingApproval(
            wire::WaitingApproval {},
        )));

        assert_eq!(waiting, Lifecycle::WaitingApproval);
        assert!(!waiting.is_over());
    }

    /// A boundary this build has never heard of must not be guessed at.
    ///
    /// Mapping it to Running would leave the shell following a dead Run
    /// forever; mapping it to Terminal would drop a live one. Neither failure
    /// is visible to the person using it, which is why the third answer exists.
    #[test]
    fn an_unknown_boundary_is_reported_as_unrecognised_not_guessed() {
        let empty = Lifecycle::from_wire(&RunLifecycleBoundary { boundary: None });

        assert_eq!(empty, Lifecycle::Unrecognised);
        assert!(
            !empty.is_over(),
            "an unrecognised boundary must not be treated as an ending"
        );
    }
}
