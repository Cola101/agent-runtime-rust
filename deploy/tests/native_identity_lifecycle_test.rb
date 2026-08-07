#!/usr/bin/env ruby
# frozen_string_literal: true

require "base64"
require "digest"
require "fileutils"
require "json"
require "minitest/autorun"
require "open3"
require "tmpdir"

class NativeIdentityLifecycleTest < Minitest::Test
  PROJECT_ROOT = File.expand_path("../..", __dir__)
  DEVCTL = File.join(PROJECT_ROOT, "deploy/native/devctl")

  def test_generates_valid_stable_local_identity_and_clean_removes_it
    Dir.mktmpdir("agent-runtime-identity-") do |temporary|
      local_root = File.join(temporary, ".local")
      environment = {
        "AGENT_RUNTIME_LOCAL_ROOT" => local_root,
        "AGENT_RUNTIME_PRESERVE_BUILD_OUTPUTS" => "true"
      }

      stdout, stderr, status = Open3.capture3(environment, DEVCTL, "prepare-identity")
      assert status.success?, "identity generation failed:\n#{stdout}\n#{stderr}"

      identity_root = File.join(local_root, "secrets", "identity")
      expected_files.each do |relative|
        path = File.join(identity_root, relative)
        assert File.file?(path), "missing #{relative}"
        assert File.size?(path), "blank #{relative}"
        assert_equal 0o600, File.stat(path).mode & 0o777, "unsafe mode for #{relative}"
      end

      access_token_path = File.join(local_root, "secrets", "local-access-token")
      assert File.size?(access_token_path), "missing local access token"
      assert_equal 0o600, File.stat(access_token_path).mode & 0o777
      access_token = File.read(access_token_path).strip
      header_segment, payload_segment, signature_segment = access_token.split(".")
      assert_equal "RS256", JSON.parse(decode_base64url(header_segment)).fetch("alg")
      claims = JSON.parse(decode_base64url(payload_segment))
      assert_equal "11111111-1111-4111-8111-111111111111", claims.fetch("tenant_id")
      assert_equal "22222222-2222-4222-8222-222222222222", claims.fetch("application_id")
      assert_includes claims.fetch("scope").split, "runs:read"
      assert_includes claims.fetch("scope").split, "runs:write"
      assert_includes claims.fetch("scope").split, "approvals:read"
      assert_includes claims.fetch("scope").split, "approvals:write"
      assert_includes claims.fetch("scope").split, "resources:read"
      assert_includes claims.fetch("scope").split, "resources:write"
      assert_operator claims.fetch("exp"), :>, Time.now.to_i
      signature = decode_base64url(signature_segment)
      Dir.mktmpdir("agent-runtime-jwt-verify-") do |verify_root|
        signature_path = File.join(verify_root, "signature")
        File.binwrite(signature_path, signature)
        _verify_out, verify_error, verify_status = Open3.capture3(
          "openssl", "dgst", "-sha256", "-verify",
          File.join(identity_root, "local-jwt-public.pem"), "-signature", signature_path,
          stdin_data: "#{header_segment}.#{payload_segment}"
        )
        assert verify_status.success?, verify_error
      end

      ca = File.join(identity_root, "ca.crt")
      %w[nats-server model-gateway checkpoint-gateway worker-client].each do |name|
        _verify_out, verify_error, verify_status = Open3.capture3(
          "openssl", "verify", "-CAfile", ca, File.join(identity_root, "#{name}.crt")
        )
        assert verify_status.success?, verify_error
      end

      nats_text = certificate_text(File.join(identity_root, "nats-server.crt"))
      assert_includes nats_text, "DNS:nats.local"
      assert_includes nats_text, "IP Address:127.0.0.1"
      assert_includes nats_text, "TLS Web Server Authentication"
      assert_includes certificate_text(File.join(identity_root, "worker-client.crt")),
                      "TLS Web Client Authentication"

      private_der = Base64.strict_decode64(
        File.read(File.join(identity_root, "workload-private.pkcs8.b64")).strip
      )
      public_raw = Base64.strict_decode64(
        File.read(File.join(identity_root, "workload-public.raw.b64")).strip
      )
      public_der, public_error, public_status = Open3.capture3(
        "openssl", "pkey", "-inform", "DER", "-pubout", "-outform", "DER",
        stdin_data: private_der
      )
      assert public_status.success?, public_error
      assert_equal 32, public_raw.bytesize
      assert_equal public_raw, public_der.bytes.last(32).pack("C*")

      model_credential_private = File.join(identity_root, "model-credential-private.pem")
      model_credential_public = File.join(identity_root, "model-credential-public.pem")
      private_text, private_error, private_status = Open3.capture3(
        "openssl", "pkey", "-in", model_credential_private, "-text", "-noout"
      )
      assert private_status.success?, private_error
      assert_includes private_text, "3072 bit"
      derived_public, derived_error, derived_status = Open3.capture3(
        "openssl", "pkey", "-in", model_credential_private, "-pubout"
      )
      assert derived_status.success?, derived_error
      assert_equal File.read(model_credential_public), derived_public

      truststore_password = File.read(
        File.join(identity_root, "control-plane-truststore-password")
      ).strip
      _list_out, list_error, list_status = Open3.capture3(
        "keytool", "-list", "-storetype", "PKCS12",
        "-keystore", File.join(identity_root, "control-plane-truststore.p12"),
        "-storepass", truststore_password
      )
      assert list_status.success?, list_error

      stable_paths = %w[
        ca.crt ca.key workload-private.pem nats-server.crt worker-client.crt
        local-jwt-private.pem local-jwt-public.pem model-credential-private.pem
        model-credential-public.pem
      ]
      before = stable_paths.to_h do |relative|
        [relative, Digest::SHA256.file(File.join(identity_root, relative)).hexdigest]
      end
      stdout, stderr, status = Open3.capture3(environment, DEVCTL, "prepare-identity")
      assert status.success?, "idempotent identity validation failed:\n#{stdout}\n#{stderr}"
      after = stable_paths.to_h do |relative|
        [relative, Digest::SHA256.file(File.join(identity_root, relative)).hexdigest]
      end
      assert_equal before, after

      stdout, stderr, status = Open3.capture3(environment, DEVCTL, "clean")
      assert status.success?, "identity cleanup failed:\n#{stdout}\n#{stderr}"
      refute File.exist?(local_root)
    end
  end

  private

  def expected_files
    %w[
      identity.ready
      ca.crt
      ca.key
      nats-server.crt
      nats-server.key
      model-gateway.crt
      model-gateway.key
      checkpoint-gateway.crt
      checkpoint-gateway.key
      worker-client.crt
      worker-client.key
      workload-private.pem
      workload-private.pkcs8.b64
      workload-public.raw.b64
      control-plane-truststore.p12
      control-plane-truststore-password
      nats-control-plane-password
      nats-worker-password
      local-jwt-private.pem
      local-jwt-public.pem
      model-credential-private.pem
      model-credential-public.pem
    ]
  end

  def decode_base64url(value)
    Base64.urlsafe_decode64(value + ("=" * ((4 - value.length % 4) % 4)))
  end

  def certificate_text(path)
    output, error, status = Open3.capture3("openssl", "x509", "-in", path, "-noout", "-text")
    assert status.success?, error
    output
  end
end
