#!/usr/bin/env ruby
# frozen_string_literal: true

require "open3"
require "rexml/document"
require "tmpdir"

ROOT = File.expand_path("../..", __dir__)

Dir.mktmpdir("native-java-effective-pom-") do |temporary|
  effective_pom = File.join(temporary, "effective-pom.xml")
  output, error, status = Open3.capture3(
    "mvn", "-q", "help:effective-pom", "-Doutput=#{effective_pom}",
    chdir: File.join(ROOT, "control-plane")
  )
  raise "failed to resolve effective Maven contract: #{output}#{error}" unless status.success?

  document = REXML::Document.new(File.read(effective_pom))
  namespaces = { "m" => "http://maven.apache.org/POM/4.0.0" }
  dependencies = REXML::XPath.match(document, "/m:project/m:dependencies/m:dependency", namespaces)
  forbidden = dependencies.each_with_object([]) do |dependency, matches|
    group = REXML::XPath.first(dependency, "m:groupId", namespaces)&.text
    artifact = REXML::XPath.first(dependency, "m:artifactId", namespaces)&.text
    matches << "#{group}:#{artifact}" if group == "org.testcontainers"
  end
  raise "native Maven contract still resolves Testcontainers: #{forbidden.join(', ')}" unless forbidden.empty?

  surefire = REXML::XPath.match(document, "/m:project/m:build/m:plugins/m:plugin", namespaces).find do |plugin|
    REXML::XPath.first(plugin, "m:artifactId", namespaces)&.text == "maven-surefire-plugin"
  end
  raise "Maven Surefire plugin is missing" unless surefire
  excluded = REXML::XPath.match(surefire, ".//m:excludes/m:exclude", namespaces).map(&:text)
  if excluded.any? { |pattern| pattern.include?("IntegrationTest") }
    raise "native integration tests are still excluded from Maven test: #{excluded.join(', ')}"
  end
  arg_line = REXML::XPath.first(surefire, ".//m:configuration/m:argLine", namespaces)&.text.to_s
  required_loopback_isolation = [
    "-Djava.net.useSystemProxies=false",
    "-Dhttp.proxyHost=",
    "-Dhttps.proxyHost=",
    "-DsocksProxyHost="
  ]
  missing = required_loopback_isolation.reject { |argument| arg_line.include?(argument) }
  unless missing.empty?
    raise "native Java tests do not isolate loopback traffic from macOS proxies: #{missing.join(', ')}"
  end
end

puts "validated native Java Maven contract"
