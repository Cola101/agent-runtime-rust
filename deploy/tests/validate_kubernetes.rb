#!/usr/bin/env ruby
# frozen_string_literal: true

require "open3"
require "yaml"

root = File.expand_path("../..", __dir__)
base = File.join(root, "deploy", "kubernetes", "base")
rendered, error, status = Open3.capture3("kubectl", "kustomize", base)
abort error unless status.success?
resources = YAML.load_stream(rendered).compact

def named(resources, kind, name)
  matches = resources.select do |resource|
    resource["kind"] == kind && resource.dig("metadata", "name") == name
  end
  raise "expected exactly one #{kind}/#{name}, got #{matches.length}" unless matches.length == 1

  matches.first
end

def pod_template(resource)
  resource.dig("spec", "template", "spec")
end

def validate_container_security(resource)
  template = pod_template(resource)
  raise "pod must run as non-root" unless template.dig("securityContext", "runAsNonRoot") == true
  unless template.dig("securityContext", "seccompProfile", "type") == "RuntimeDefault"
    raise "pod must use RuntimeDefault seccomp"
  end
  template.fetch("containers").each do |container|
    security = container.fetch("securityContext")
    raise "privilege escalation must be disabled" unless security["allowPrivilegeEscalation"] == false
    raise "root filesystem must be read-only" unless security["readOnlyRootFilesystem"] == true
    raise "all capabilities must be dropped" unless security.dig("capabilities", "drop").include?("ALL")
    raise "latest image tag is forbidden" if container.fetch("image").end_with?(":latest")
  end
end

def validate_probe(container)
  expected_live = { "path" => "/live", "port" => "health" }
  expected_ready = { "path" => "/ready", "port" => "health" }
  raise "invalid liveness probe" unless container.dig("livenessProbe", "httpGet") == expected_live
  raise "invalid readiness probe" unless container.dig("readinessProbe", "httpGet") == expected_ready
end

def environment(container)
  container.fetch("env", []).to_h { |entry| [entry.fetch("name"), entry] }
end

def secret_environment!(container, names)
  env = environment(container)
  names.each do |name|
    source = env.dig(name, "valueFrom", "secretKeyRef")
    raise "#{name} must come from a Kubernetes Secret" unless source&.fetch("name", nil) && source&.fetch("key", nil)
  end
end

def secret_provider_keys!(provider, secret_name, keys)
  secret = provider.dig("spec", "secretObjects").to_a.find do |candidate|
    candidate["secretName"] == secret_name
  end
  raise "#{secret_name} must be materialized by its SecretProviderClass" unless secret

  mapped = secret.fetch("data").to_h { |entry| [entry.fetch("key"), entry.fetch("objectName")] }
  vault_objects = YAML.safe_load(provider.dig("spec", "parameters", "objects")).to_a.to_h do |entry|
    [entry.fetch("objectName"), entry]
  end
  keys.each do |key|
    object_name = mapped[key]
    raise "#{secret_name} does not materialize #{key}" unless object_name
    raise "Vault source for #{secret_name}/#{key} is missing" unless vault_objects.key?(object_name)
  end
end

model = named(resources, "Deployment", "model-gateway")
checkpoint = named(resources, "Deployment", "checkpoint-gateway")
worker = named(resources, "StatefulSet", "runtime-worker")
control = named(resources, "Deployment", "control-plane")

raise "model gateway needs two replicas" unless model.dig("spec", "replicas") >= 2
raise "checkpoint gateway needs two replicas" unless checkpoint.dig("spec", "replicas") >= 2
raise "worker needs three replicas" unless worker.dig("spec", "replicas") >= 3
raise "control plane needs two replicas" unless control.dig("spec", "replicas") >= 2
unless control.dig("spec", "strategy", "rollingUpdate") == { "maxSurge" => 1, "maxUnavailable" => 0 }
  raise "control plane rollout must keep every existing replica available"
end

[model, checkpoint, worker].each do |workload|
  validate_container_security(workload)
  template = pod_template(workload)
  validate_probe(template.fetch("containers").first)
  raise "workload must mount CSI secrets" unless template.fetch("volumes", []).any? { |volume| volume.key?("csi") }
end

validate_container_security(control)
control_template = pod_template(control)
raise "control plane must mount CSI secrets" unless control_template.fetch("volumes", []).any? { |volume| volume.key?("csi") }
control_container = control_template.fetch("containers").find { |container| container["name"] == "control-plane" }
raise "missing control-plane container" unless control_container
control_ports = control_container.fetch("ports").to_h { |port| [port.fetch("name"), port.fetch("containerPort")] }
unless control_ports == { "api" => 8080, "management" => 9090 }
  raise "control plane must expose separate API and management ports"
end
expected_liveness = {
  "path" => "/actuator/health/liveness", "port" => "management", "scheme" => "HTTPS"
}
expected_readiness = {
  "path" => "/actuator/health/readiness", "port" => "management", "scheme" => "HTTPS"
}
unless control_container.dig("livenessProbe", "httpGet") == expected_liveness
  raise "control plane liveness must use the TLS management port"
end
unless control_container.dig("readinessProbe", "httpGet") == expected_readiness
  raise "control plane readiness must use the TLS management port"
end
control_env = environment(control_container)
unless control_env.dig("MANAGEMENT_SERVER_SSL_ENABLED", "value") == "true"
  raise "management endpoint TLS must be enabled"
end
secret_environment!(control_container, %w[
  SPRING_DATASOURCE_USERNAME
  SPRING_DATASOURCE_PASSWORD
  MANAGEMENT_SCRAPE_USERNAME
  MANAGEMENT_SCRAPE_PASSWORD
  AGENT_RUNTIME_WORKLOAD_IDENTITY_PRIVATE_KEY_PKCS8
  AGENT_RUNTIME_SKILL_SIGNING_KEY_ID
  AGENT_RUNTIME_SKILL_SIGNING_PRIVATE_KEY_PKCS8
  AGENT_RUNTIME_OUTBOX_NATS_SECURITY_USERNAME
  AGENT_RUNTIME_OUTBOX_NATS_SECURITY_PASSWORD
  AGENT_RUNTIME_OUTBOX_NATS_SECURITY_TRUSTSTORE_PASSWORD
  AGENT_RUNTIME_SCHEDULER_NATS_SECURITY_USERNAME
  AGENT_RUNTIME_SCHEDULER_NATS_SECURITY_PASSWORD
  AGENT_RUNTIME_SCHEDULER_NATS_SECURITY_TRUSTSTORE_PASSWORD
])
raise "control plane must spread replicas across nodes" if control_template.fetch("topologySpreadConstraints", []).empty?

worker_template = pod_template(worker)
worker_env = worker_template.fetch("containers").first.fetch("env", []).to_h { |entry| [entry["name"], entry] }
worker_id_file = worker_env.dig("AGENT_RUNTIME_WORKER_ID_FILE", "value")
raise "worker ID must live on stable storage" unless worker_id_file&.start_with?("/var/lib/")
secret_environment!(worker_template.fetch("containers").first, %w[
  AGENT_RUNTIME_SKILL_SIGNING_KEY_ID
  AGENT_RUNTIME_SKILL_SIGNING_PUBLIC_KEY
])
raise "worker requires stable storage" if worker.dig("spec", "volumeClaimTemplates").to_a.empty?
drain_grace = Integer(worker_env.dig("AGENT_RUNTIME_DRAIN_GRACE_SECONDS", "value"), exception: false)
termination_grace = worker_template.fetch("terminationGracePeriodSeconds")
unless drain_grace&.positive? && drain_grace <= termination_grace - 10
  raise "worker drain grace must leave at least ten seconds for process teardown"
end

["model-gateway", "checkpoint-gateway"].each do |gateway|
  named(resources, "Service", gateway)
  hpa = named(resources, "HorizontalPodAutoscaler", gateway)
  raise "gateway HPA must keep two replicas" unless hpa.dig("spec", "minReplicas") >= 2
  named(resources, "PodDisruptionBudget", gateway)
end
named(resources, "PodDisruptionBudget", "runtime-worker")
named(resources, "ServiceAccount", "control-plane")
api_service = named(resources, "Service", "control-plane")
management_service = named(resources, "Service", "control-plane-management")
raise "API service must expose only port 8080" unless api_service.dig("spec", "ports") == [
  { "name" => "http", "port" => 8080, "targetPort" => "api" }
]
raise "management service must expose only port 9090" unless management_service.dig("spec", "ports") == [
  { "name" => "management", "port" => 9090, "targetPort" => "management" }
]
named(resources, "PodDisruptionBudget", "control-plane")

named(resources, "SecretProviderClass", "runtime-gateway-secrets")
worker_secrets = named(resources, "SecretProviderClass", "runtime-worker-secrets")
control_secrets = named(resources, "SecretProviderClass", "control-plane-secrets")
secret_provider_keys!(worker_secrets, "runtime-worker-credentials", %w[
  skill-signing-key-id
  skill-signing-public-key
])
secret_provider_keys!(control_secrets, "control-plane-credentials", %w[
  skill-signing-key-id
  skill-signing-private-key-pkcs8
])
named(resources, "NetworkPolicy", "default-deny")
named(resources, "NetworkPolicy", "runtime-allow-required-flows")
metrics_policy = named(resources, "NetworkPolicy", "control-plane-metrics-ingress")
metrics_ingress = metrics_policy.dig("spec", "ingress")
unless metrics_ingress == [{
  "from" => [{
    "namespaceSelector" => { "matchLabels" => { "kubernetes.io/metadata.name" => "monitoring" } }
  }],
  "ports" => [{ "protocol" => "TCP", "port" => 9090 }]
}]
  raise "management port must accept traffic only from the monitoring namespace"
end
database_policy = named(resources, "NetworkPolicy", "control-plane-database-egress")
unless database_policy.dig("spec", "egress") == [{
  "to" => [{
    "namespaceSelector" => { "matchLabels" => { "kubernetes.io/metadata.name" => "platform" } }
  }],
  "ports" => [{ "protocol" => "TCP", "port" => 5432 }]
}]
  raise "control plane database egress must be limited to PostgreSQL in the platform namespace"
end

resources.select { |resource| ["Deployment", "StatefulSet"].include?(resource["kind"]) }.each do |resource|
  pod_template(resource).fetch("containers").each do |container|
    container.fetch("env", []).each do |env|
      secret = env["name"].end_with?("_PASSWORD", "_API_KEY", "_SECRET_ACCESS_KEY")
      raise "plaintext secret in #{resource.dig('metadata', 'name')}" if secret && env.key?("value")
    end
  end
end

puts "validated #{resources.length} rendered Kubernetes resources"

observability = File.join(root, "deploy", "kubernetes", "observability")
observed, observed_error, observed_status = Open3.capture3("kubectl", "kustomize", observability)
abort observed_error unless observed_status.success?
observability_resources = YAML.load_stream(observed).compact
recovery_rules = named(observability_resources, "PrometheusRule", "agent-runtime-recovery")
service_monitor = named(observability_resources, "ServiceMonitor", "control-plane")
monitor_endpoint = service_monitor.dig("spec", "endpoints")&.first
unless monitor_endpoint&.slice("port", "scheme", "path") == {
  "port" => "management", "scheme" => "https", "path" => "/actuator/prometheus"
}
  raise "ServiceMonitor must scrape only the TLS management endpoint"
end
raise "ServiceMonitor requires dedicated Basic Auth" unless monitor_endpoint["basicAuth"]
raise "ServiceMonitor must verify the management CA" unless monitor_endpoint.dig("tlsConfig", "ca", "secret")
alerts = recovery_rules.dig("spec", "groups").flat_map { |group| group.fetch("rules") }
expected_alerts = %w[
  AgentRuntimeRecoveryMetricsStale
  AgentRuntimeRecoverySloBreached
  AgentRuntimeRecoveryWaitingCapacity
]
unless alerts.map { |alert| alert["alert"] }.sort == expected_alerts.sort
  raise "recovery PrometheusRule must define the expected operational alerts"
end
alerts.each do |alert|
  expression = alert.fetch("expr").to_s
  raise "tenant identity is forbidden in operational metric labels" if expression.include?("tenant_id")
end

puts "validated recovery observability overlay"
