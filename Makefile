DOWNLOAD_WRAPPER := deploy/native/with-download-proxy

.PHONY: test check test-java test-rust test-console check-java check-rust check-console check-native-dev check-native-one-command-live check-native-recovery-live check-native-steering-live check-native-sse-resumption-live check-native-shell-live check-production check-deploy check-nats-live dev dev-run dev-approve dev-native-bootstrap dev-native-start dev-status dev-down dev-clean

test: test-java test-rust test-console

check: check-java check-rust check-console check-native-dev

test-java:
	deploy/native/run-java-tests

test-rust:
	$(DOWNLOAD_WRAPPER) cargo test --manifest-path runtime/Cargo.toml --workspace

test-console:
	$(DOWNLOAD_WRAPPER) pnpm --filter @agent-runtime/console test

check-java:
	deploy/native/run-java-tests

check-rust:
	$(DOWNLOAD_WRAPPER) cargo fmt --manifest-path runtime/Cargo.toml --all -- --check
	$(DOWNLOAD_WRAPPER) cargo clippy --manifest-path runtime/Cargo.toml --workspace --all-targets --all-features -- -D warnings

check-console:
	$(DOWNLOAD_WRAPPER) pnpm --filter @agent-runtime/console lint
	$(DOWNLOAD_WRAPPER) pnpm --filter @agent-runtime/console typecheck
	$(DOWNLOAD_WRAPPER) pnpm --filter @agent-runtime/console test:e2e
	$(DOWNLOAD_WRAPPER) pnpm --filter @agent-runtime/console build
	$(DOWNLOAD_WRAPPER) pnpm audit --prod

check-native-dev:
	ruby deploy/tests/native_dev_bootstrap_test.rb
	ruby deploy/tests/native_daemonize_service_test.rb
	ruby deploy/tests/native_clean_contract_test.rb
	ruby deploy/tests/native_approve_local_test.rb
	ruby deploy/tests/native_identity_lifecycle_test.rb
	ruby deploy/tests/native_dev_lifecycle_test.rb
	ruby deploy/tests/native_java_test_lifecycle_test.rb
	ruby deploy/tests/native_java_maven_contract_test.rb
	ruby deploy/tests/native_java_toolchain_test.rb
	ruby deploy/tests/native_command_contract_test.rb
	ruby deploy/tests/native_download_proxy_test.rb
	ruby deploy/tests/native_utf8_diagnostics_test.rb
	ruby deploy/tests/native_nats_live_contract_test.rb
	ruby deploy/tests/native_openai_tool_provider_test.rb
	ruby deploy/tests/native_rate_limited_provider_test.rb
	ruby deploy/tests/native_run_local_test.rb
	ruby deploy/tests/native_supervisor_lifecycle_test.rb
	ruby deploy/tests/native_seed_contract_test.rb
	ruby deploy/tests/native_model_provider_seed_test.rb
	ruby deploy/tests/openapi_approval_contract_test.rb
	ruby deploy/tests/openapi_resource_configuration_contract_test.rb
	ruby deploy/tests/openapi_run_steering_contract_test.rb

check-native-recovery-live:
	ruby deploy/tests/native_trusted_tool_recovery_live_test.rb

check-native-one-command-live:
	ruby deploy/tests/native_one_command_run_live_test.rb

check-native-steering-live:
	ruby deploy/tests/native_run_steering_live_test.rb

check-native-sse-resumption-live:
	ruby deploy/tests/native_sse_resumption_live_test.rb

check-native-shell-live:
	ruby deploy/tests/native_shell_tool_live_test.rb

check-production: check-deploy

check-deploy:
	ruby deploy/tests/validate_kubernetes.rb

check-nats-live:
	deploy/tests/verify_nats_tls.sh

dev-native-bootstrap:
	deploy/native/devctl bootstrap

dev-native-start:
	deploy/native/devctl start-infra

dev:
	deploy/native/supervisor start

dev-run: dev
	deploy/native/run-local

dev-approve:
	deploy/native/approve-local "$(APPROVAL_ID)" "$(or $(VERSION),1)" "$(or $(DECISION),allow_once)"

dev-status:
	deploy/native/supervisor status

dev-down:
	deploy/native/supervisor stop

dev-clean:
	deploy/native/supervisor clean
