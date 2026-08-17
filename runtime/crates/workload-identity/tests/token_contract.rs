use agent_workload_identity::{
    RequiredCapability, WorkloadIdentityBinding, WorkloadIdentityClaims, WorkloadTokenError,
    WorkloadTokenVerifier,
};
use base64::Engine;
use ed25519_dalek::{Signer, SigningKey};
use std::collections::BTreeSet;
use uuid::Uuid;

#[test]
fn v2_token_requires_capability_and_exact_worker_incarnation_binding() {
    let signing_key = SigningKey::from_bytes(&[7; 32]);
    let claims = claims();
    let token = sign_v2(&signing_key, &claims);
    let verifier = WorkloadTokenVerifier::new(signing_key.verifying_key());

    let verified = verifier
        .verify(
            &token,
            RequiredCapability::new("checkpoint-gateway", "checkpoint.write", true),
            1_785_542_410_000,
        )
        .unwrap();

    assert!(verified.authorizes(&WorkloadIdentityBinding {
        tenant_id: claims.tenant_id,
        application_id: claims.application_id,
        workload_identity_id: claims.workload_identity_id,
        run_id: claims.run_id,
        session_id: claims.session_id,
        workspace_id: claims.workspace_id,
        agent_version_id: claims.agent_version_id,
        attempt_id: claims.attempt_id,
        worker_id: claims.worker_id,
        worker_incarnation_id: claims.worker_incarnation_id,
    }));
    assert!(!verified.authorizes(&WorkloadIdentityBinding {
        worker_incarnation_id: Uuid::now_v7(),
        ..WorkloadIdentityBinding::from(&claims)
    }));
}

#[test]
fn verifier_can_be_constructed_from_the_control_plane_base64_public_key() {
    let signing_key = SigningKey::from_bytes(&[41; 32]);
    let encoded =
        base64::engine::general_purpose::STANDARD.encode(signing_key.verifying_key().to_bytes());

    assert!(WorkloadTokenVerifier::from_base64(&encoded).is_ok());
    assert!(WorkloadTokenVerifier::from_base64("not-a-public-key").is_err());
}

#[test]
fn token_for_model_execution_cannot_be_reused_for_checkpoint_write() {
    let signing_key = SigningKey::from_bytes(&[8; 32]);
    let mut claims = claims();
    claims.scopes = BTreeSet::from(["model.execute".to_string()]);
    let token = sign_v2(&signing_key, &claims);
    let verifier = WorkloadTokenVerifier::new(signing_key.verifying_key());

    assert_eq!(
        verifier.verify(
            &token,
            RequiredCapability::new("checkpoint-gateway", "checkpoint.write", true),
            1_785_542_410_000,
        ),
        Err(WorkloadTokenError::MissingCapability)
    );
}

#[test]
fn v3_token_requires_a_well_formed_model_policy_digest() {
    let signing_key = SigningKey::from_bytes(&[9; 32]);
    let mut claims = claims();
    claims.schema_version = 3;
    claims.model_policy_digest = "a".repeat(64);
    let verifier = WorkloadTokenVerifier::new(signing_key.verifying_key());

    assert!(
        verifier
            .verify(
                &sign_v2(&signing_key, &claims),
                RequiredCapability::new("model-gateway", "model.execute", true),
                1_785_542_410_000,
            )
            .is_ok()
    );

    claims.model_policy_digest = "tampered".into();
    assert_eq!(
        verifier.verify(
            &sign_v2(&signing_key, &claims),
            RequiredCapability::new("model-gateway", "model.execute", true),
            1_785_542_410_000,
        ),
        Err(WorkloadTokenError::InvalidClaims)
    );
}

/// The production break this catches is accepting a v20 execution identity
/// whose signed token only names the tenant and Run. The gateway and Worker
/// must receive the immutable application/workload/Session/Workspace/Agent
/// chain from the signature rather than trusting fields copied into a request.
#[test]
fn v4_token_preserves_the_complete_runtime_invocation_identity() {
    let signing_key = SigningKey::from_bytes(&[10; 32]);
    let now = 1_785_542_410_000_i64;
    let application_id = Uuid::now_v7();
    let workload_identity_id = Uuid::now_v7();
    let session_id = Uuid::now_v7();
    let workspace_id = Uuid::now_v7();
    let agent_version_id = Uuid::now_v7();
    let value = serde_json::json!({
        "schema_version": 4,
        "tenant_id": Uuid::now_v7(),
        "application_id": application_id,
        "workload_identity_id": workload_identity_id,
        "run_id": Uuid::now_v7(),
        "session_id": session_id,
        "workspace_id": workspace_id,
        "agent_version_id": agent_version_id,
        "attempt_id": Uuid::now_v7(),
        "worker_id": Uuid::now_v7(),
        "worker_incarnation_id": Uuid::now_v7(),
        "model_policy_id": Uuid::now_v7(),
        "model_policy_digest": "a".repeat(64),
        "authorized_mcp_servers": {},
        "audiences": ["model-gateway"],
        "scopes": ["model.execute"],
        "issued_at_unix_ms": now - 10_000,
        "expires_at_unix_ms": now + 20_000
    });
    let verifier = WorkloadTokenVerifier::new(signing_key.verifying_key());

    let verified = verifier
        .verify(
            &sign_value(&signing_key, &value),
            RequiredCapability::new("model-gateway", "model.execute", true),
            now,
        )
        .expect("schema v4 must carry a complete signed invocation identity");
    let verified = serde_json::to_value(verified).unwrap();
    assert_eq!(verified["application_id"], application_id.to_string());
    assert_eq!(
        verified["workload_identity_id"],
        workload_identity_id.to_string()
    );
    assert_eq!(verified["session_id"], session_id.to_string());
    assert_eq!(verified["workspace_id"], workspace_id.to_string());
    assert_eq!(verified["agent_version_id"], agent_version_id.to_string());
}

/// The production break this catches is reducing a schema-v4 authorization
/// decision back to tenant/Run/attempt/Worker equality. A request carrying a
/// different application identity must not be authorized by the same token.
#[test]
fn v4_authorization_rejects_a_different_application_identity() {
    let signing_key = SigningKey::from_bytes(&[11; 32]);
    let now = 1_785_542_410_000_i64;
    let value = serde_json::json!({
        "schema_version": 4,
        "tenant_id": Uuid::now_v7(),
        "application_id": Uuid::now_v7(),
        "workload_identity_id": Uuid::now_v7(),
        "run_id": Uuid::now_v7(),
        "session_id": Uuid::now_v7(),
        "workspace_id": Uuid::now_v7(),
        "agent_version_id": Uuid::now_v7(),
        "attempt_id": Uuid::now_v7(),
        "worker_id": Uuid::now_v7(),
        "worker_incarnation_id": Uuid::now_v7(),
        "model_policy_id": Uuid::now_v7(),
        "model_policy_digest": "b".repeat(64),
        "authorized_mcp_servers": {},
        "audiences": ["model-gateway"],
        "scopes": ["model.execute"],
        "issued_at_unix_ms": now - 10_000,
        "expires_at_unix_ms": now + 20_000
    });
    let verified = WorkloadTokenVerifier::new(signing_key.verifying_key())
        .verify(
            &sign_value(&signing_key, &value),
            RequiredCapability::new("model-gateway", "model.execute", true),
            now,
        )
        .unwrap();
    let mut different = verified.clone();
    different.application_id = Uuid::now_v7();

    assert!(!verified.authorizes(&WorkloadIdentityBinding::from(&different)));
}

/// Forgetting to state a shape must fail closed.
///
/// `RequiredCapability::new` means an execution, so a surface whose author never
/// thought about shape refuses operator tokens rather than accepting them on
/// scope alone. This is the property that keeps the separation from decaying as
/// new surfaces are added.
#[test]
fn a_surface_that_does_not_ask_for_an_operator_refuses_one() {
    let signing_key = SigningKey::from_bytes(&[94; 32]);
    let verifier = WorkloadTokenVerifier::new(signing_key.verifying_key());
    let claims = operator_claims();
    let token = sign_v2(&signing_key, &claims);

    assert_eq!(
        verifier.verify(
            &token,
            // The default, as a surface that said nothing about shape would get.
            RequiredCapability::new("model-gateway", "mcp.oauth.admin", true),
            claims.issued_at_unix_ms + 1_000,
        ),
        Err(WorkloadTokenError::WrongIdentityShape)
    );
}

/// And the converse: asking for an operator refuses an execution, so the
/// admin surface cannot be reached by a Run however it was scoped.
#[test]
fn asking_for_an_operator_refuses_an_execution() {
    let signing_key = SigningKey::from_bytes(&[95; 32]);
    let verifier = WorkloadTokenVerifier::new(signing_key.verifying_key());
    let mut run_shaped = claims();
    run_shaped.scopes = BTreeSet::from(["mcp.oauth.admin".to_string()]);
    run_shaped.audiences = BTreeSet::from(["model-gateway".to_string()]);
    let token = sign_v2(&signing_key, &run_shaped);

    assert_eq!(
        verifier.verify(
            &token,
            RequiredCapability::operator("model-gateway", "mcp.oauth.admin"),
            run_shaped.issued_at_unix_ms + 1_000,
        ),
        Err(WorkloadTokenError::WrongIdentityShape)
    );
}

/// An operator identity: it says who is acting and for which tenant, and says
/// nothing about an execution.
fn operator_claims() -> WorkloadIdentityClaims {
    WorkloadIdentityClaims {
        schema_version: agent_workload_identity::OPERATOR_SCHEMA_VERSION,
        application_id: Uuid::now_v7(),
        workload_identity_id: Uuid::now_v7(),
        run_id: Uuid::nil(),
        attempt_id: Uuid::nil(),
        worker_id: Uuid::nil(),
        worker_incarnation_id: Uuid::nil(),
        model_policy_id: Uuid::nil(),
        scopes: BTreeSet::from(["mcp.oauth.admin".to_string()]),
        ..claims()
    }
}

/// The whole point of the shape: an operator token carries no execution, so it
/// can never satisfy a binding that names one.
#[test]
fn an_operator_token_is_accepted_and_names_no_execution() {
    let signing_key = SigningKey::from_bytes(&[91; 32]);
    let verifier = WorkloadTokenVerifier::new(signing_key.verifying_key());
    let claims = operator_claims();
    let token = sign_v2(&signing_key, &claims);

    let verified = verifier
        .verify(
            &token,
            RequiredCapability::operator("model-gateway", "mcp.oauth.admin"),
            claims.issued_at_unix_ms + 1_000,
        )
        .expect("a well-formed operator token must verify");
    assert!(verified.is_operator());
    assert!(verified.run_id.is_nil());
    // `require_incarnation` is satisfied vacuously: an operator has no worker to
    // pin, and demanding one would make the shape unusable.
    assert!(verified.worker_incarnation_id.is_nil());
}

/// A token claiming the operator schema while still naming a Run is neither one
/// thing nor the other, and is refused rather than being read as whichever half
/// happens to be checked first.
#[test]
fn an_operator_token_that_names_a_run_is_refused() {
    let signing_key = SigningKey::from_bytes(&[92; 32]);
    let verifier = WorkloadTokenVerifier::new(signing_key.verifying_key());
    for mutate in [
        (|claims: &mut WorkloadIdentityClaims| claims.run_id = Uuid::now_v7()) as fn(&mut _),
        |claims: &mut WorkloadIdentityClaims| claims.attempt_id = Uuid::now_v7(),
        |claims: &mut WorkloadIdentityClaims| claims.worker_id = Uuid::now_v7(),
        |claims: &mut WorkloadIdentityClaims| claims.worker_incarnation_id = Uuid::now_v7(),
        |claims: &mut WorkloadIdentityClaims| claims.model_policy_id = Uuid::now_v7(),
        |claims: &mut WorkloadIdentityClaims| claims.session_id = Uuid::now_v7(),
        |claims: &mut WorkloadIdentityClaims| claims.workspace_id = Uuid::now_v7(),
        |claims: &mut WorkloadIdentityClaims| claims.agent_version_id = Uuid::now_v7(),
    ] {
        let mut claims = operator_claims();
        mutate(&mut claims);
        let token = sign_v2(&signing_key, &claims);
        assert_eq!(
            verifier.verify(
                &token,
                RequiredCapability::operator("model-gateway", "mcp.oauth.admin"),
                claims.issued_at_unix_ms + 1_000,
            ),
            Err(WorkloadTokenError::InvalidClaims),
            "an operator token naming any execution field must be refused"
        );
    }
}

/// An operator must still say who it is. Without the application and workload
/// identity there is nothing to authorize against but a tenant, which every
/// token in that tenant would satisfy.
#[test]
fn an_operator_token_without_an_actor_is_refused() {
    let signing_key = SigningKey::from_bytes(&[93; 32]);
    let verifier = WorkloadTokenVerifier::new(signing_key.verifying_key());
    for mutate in [
        (|claims: &mut WorkloadIdentityClaims| claims.application_id = Uuid::nil()) as fn(&mut _),
        |claims: &mut WorkloadIdentityClaims| claims.workload_identity_id = Uuid::nil(),
    ] {
        let mut claims = operator_claims();
        mutate(&mut claims);
        let token = sign_v2(&signing_key, &claims);
        assert_eq!(
            verifier.verify(
                &token,
                RequiredCapability::operator("model-gateway", "mcp.oauth.admin"),
                claims.issued_at_unix_ms + 1_000,
            ),
            Err(WorkloadTokenError::InvalidClaims),
            "an operator token must name the actor, not only the tenant"
        );
    }
}

fn claims() -> WorkloadIdentityClaims {
    WorkloadIdentityClaims {
        schema_version: 2,
        tenant_id: Uuid::now_v7(),
        application_id: Uuid::nil(),
        workload_identity_id: Uuid::nil(),
        run_id: Uuid::now_v7(),
        session_id: Uuid::nil(),
        workspace_id: Uuid::nil(),
        agent_version_id: Uuid::nil(),
        attempt_id: Uuid::now_v7(),
        worker_id: Uuid::now_v7(),
        worker_incarnation_id: Uuid::now_v7(),
        model_policy_id: Uuid::now_v7(),
        model_policy_digest: String::new(),
        authorized_mcp_servers: Default::default(),
        audiences: BTreeSet::from([
            "model-gateway".to_string(),
            "checkpoint-gateway".to_string(),
        ]),
        scopes: BTreeSet::from([
            "model.execute".to_string(),
            "checkpoint.read".to_string(),
            "checkpoint.write".to_string(),
        ]),
        issued_at_unix_ms: 1_785_542_400_000,
        expires_at_unix_ms: 1_785_542_430_000,
    }
}

fn sign_v2(signing_key: &SigningKey, claims: &WorkloadIdentityClaims) -> String {
    sign_value(signing_key, &serde_json::to_value(claims).unwrap())
}

fn sign_value(signing_key: &SigningKey, value: &serde_json::Value) -> String {
    let payload =
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(serde_json::to_vec(value).unwrap());
    let signing_input = format!("v2.{payload}");
    let signature = signing_key.sign(signing_input.as_bytes());
    format!(
        "{signing_input}.{}",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(signature.to_bytes())
    )
}
