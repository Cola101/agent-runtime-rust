#!/usr/bin/env ruby
# frozen_string_literal: true

# The native Java entry point must run Maven on the JDK the control plane
# declares. Homebrew's Maven ships its own JVM, so without pinning, the test JVM
# is whatever the package manager last installed while the runtime services keep
# using the JDK on PATH. That split is silent until a tool that inspects class
# file versions -- Byte Buddy under Mockito -- refuses to instrument.

require "open3"

ROOT = File.expand_path("../..", __dir__)
WRAPPER = File.join(ROOT, "deploy", "native", "with-java-toolchain")
POM = File.join(ROOT, "control-plane", "pom.xml")

declared = File.read(POM)[%r{<java\.version>\s*(\d+)\s*</java\.version>}, 1]
raise "control-plane pom does not declare <java.version>" unless declared

output, error, status = Open3.capture3(
  WRAPPER, "mvn", "-version", chdir: File.join(ROOT, "control-plane")
)
raise "native Java toolchain wrapper failed: #{output}#{error}" unless status.success?

running = output[/^Java version:\s*(\d+)/, 1]
raise "could not read the Maven JVM version from:\n#{output}" unless running

unless running == declared
  raise "native Java tests run on Java #{running} but the control plane declares " \
        "Java #{declared}; pin the toolchain so tests and runtime share one JDK"
end

runtime_java, _, runtime_status = Open3.capture3(WRAPPER, "sh", "-c", "java -version 2>&1")
raise "native Java toolchain wrapper could not run java" unless runtime_status.success?

runtime_major = runtime_java[/version "(\d+)/, 1]
unless runtime_major == declared
  raise "the wrapped runtime java is Java #{runtime_major} but Maven uses Java #{declared}; " \
        "the build and the native services must share one JDK"
end

puts "validated native Java toolchain pinning on Java #{declared}"
