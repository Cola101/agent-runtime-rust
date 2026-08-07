#!/usr/bin/env ruby
# frozen_string_literal: true

require "fileutils"
require "minitest/autorun"
require "open3"
require "tmpdir"

class NativeJavaTestLifecycleTest < Minitest::Test
  PROJECT_ROOT = File.expand_path("../..", __dir__)
  RUNNER = File.join(PROJECT_ROOT, "deploy/native/run-java-tests")

  def test_success_exports_native_endpoints_and_cleans_test_runtime
    with_fake_commands do |root, env, calls|
      stdout, stderr, status = Open3.capture3(env, "/bin/sh", RUNNER)

      assert status.success?, "runner failed:\n#{stdout}\n#{stderr}"
      assert_equal ["bootstrap", "start-infra", "clean"], File.readlines(calls, chomp: true)
      assert_includes File.read(File.join(root, "maven-env")), "SPRING_DATASOURCE_URL=jdbc:postgresql://127.0.0.1:55432/agent_runtime"
      assert_includes File.read(File.join(root, "maven-env")), "AGENT_RUNTIME_LOCAL_NATS_URL=nats://127.0.0.1:44222"
      refute File.exist?(File.join(root, "test-runtime"))
    end
  end

  def test_failed_maven_run_still_cleans_test_runtime_and_preserves_exit_status
    with_fake_commands do |root, env, calls|
      env["FAKE_MVN_FAIL"] = "1"

      _stdout, stderr, status = Open3.capture3(env, "/bin/sh", RUNNER)

      assert_equal 23, status.exitstatus
      assert_includes stderr, "postgres diagnostic"
      assert_equal ["bootstrap", "start-infra", "clean"], File.readlines(calls, chomp: true)
      refute File.exist?(File.join(root, "test-runtime"))
    end
  end

  def test_default_test_runtime_stays_inside_the_project_local_root
    with_fake_commands do |root, env, _calls|
      env.delete("AGENT_RUNTIME_JAVA_TEST_ROOT")

      stdout, stderr, status = Open3.capture3(env, "/bin/sh", RUNNER)

      assert status.success?, "runner failed:\n#{stdout}\n#{stderr}"
      pid_file = File.readlines(File.join(root, "maven-env"), chomp: true)
          .find { |line| line.start_with?("AGENT_RUNTIME_NATS_PID_FILE=") }
      expected_prefix = "AGENT_RUNTIME_NATS_PID_FILE=#{env.fetch('AGENT_RUNTIME_TOOLCHAIN_ROOT')}/test/java-"
      assert pid_file.start_with?(expected_prefix), pid_file
    end
  end

  private

  def with_fake_commands
    Dir.mktmpdir("native-java-test-") do |root|
      calls = File.join(root, "calls")
      fake_devctl = File.join(root, "devctl")
      fake_maven = File.join(root, "mvn")
      write_executable(fake_devctl, <<~'SH')
        #!/bin/sh
        set -eu
        printf '%s\n' "$1" >> "$FAKE_CALLS"
        case "$1" in
          bootstrap)
            mkdir -p "$AGENT_RUNTIME_LOCAL_ROOT/toolchain"
            printf '#!/bin/sh\nexit 0\n' > "$AGENT_RUNTIME_LOCAL_ROOT/toolchain/nats-server"
            chmod 755 "$AGENT_RUNTIME_LOCAL_ROOT/toolchain/nats-server"
            ;;
          start-infra)
            mkdir -p "$AGENT_RUNTIME_LOCAL_ROOT/env" "$AGENT_RUNTIME_LOCAL_ROOT/logs"
            : > "$AGENT_RUNTIME_LOCAL_ROOT/.agent-runtime-local-root"
            printf 'postgres diagnostic\n' > "$AGENT_RUNTIME_LOCAL_ROOT/logs/postgres.log"
            cat > "$AGENT_RUNTIME_LOCAL_ROOT/env/native.env" <<EOF
        export SPRING_DATASOURCE_URL='jdbc:postgresql://127.0.0.1:$AGENT_RUNTIME_LOCAL_POSTGRES_PORT/agent_runtime'
        export SPRING_DATASOURCE_USERNAME='agent_runtime_owner'
        export SPRING_DATASOURCE_PASSWORD='local-test-password'
        export AGENT_RUNTIME_LOCAL_NATS_URL='nats://127.0.0.1:$AGENT_RUNTIME_LOCAL_NATS_PORT'
        export AGENT_RUNTIME_NATS_PID_FILE='$AGENT_RUNTIME_LOCAL_ROOT/run/nats.pid'
        EOF
            ;;
          clean)
            rm -rf "$AGENT_RUNTIME_LOCAL_ROOT"
            ;;
          *) exit 91 ;;
        esac
      SH
      write_executable(fake_maven, <<~'SH')
        #!/bin/sh
        set -eu
        {
          printf 'SPRING_DATASOURCE_URL=%s\n' "$SPRING_DATASOURCE_URL"
          printf 'AGENT_RUNTIME_LOCAL_NATS_URL=%s\n' "$AGENT_RUNTIME_LOCAL_NATS_URL"
          printf 'AGENT_RUNTIME_NATS_PID_FILE=%s\n' "$AGENT_RUNTIME_NATS_PID_FILE"
        } > "$FAKE_ROOT/maven-env"
        [ "${FAKE_MVN_FAIL:-0}" != 1 ] || exit 23
      SH
      env = {
        "AGENT_RUNTIME_DEVCTL" => fake_devctl,
        "AGENT_RUNTIME_MAVEN" => fake_maven,
        "AGENT_RUNTIME_TOOLCHAIN_ROOT" => File.join(root, "toolchain"),
        "AGENT_RUNTIME_JAVA_TEST_ROOT" => File.join(root, "test-runtime"),
        "AGENT_RUNTIME_LOCAL_POSTGRES_PORT" => "55432",
        "AGENT_RUNTIME_LOCAL_NATS_PORT" => "44222",
        "AGENT_RUNTIME_LOCAL_NATS_MONITOR_PORT" => "48222",
        "FAKE_CALLS" => calls,
        "FAKE_ROOT" => root
      }
      yield root, env, calls
    end
  end

  def write_executable(path, content)
    File.write(path, content)
    FileUtils.chmod(0o755, path)
  end
end
