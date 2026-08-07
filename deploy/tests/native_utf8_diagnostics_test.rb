#!/usr/bin/env ruby
# frozen_string_literal: true

require_relative "support/utf8_diagnostics"

binary = "playwright:\xFF\xFE".b
utf8 = "控制面日志"

diagnostic = Utf8Diagnostics.join(binary, utf8)

raise "diagnostic must be valid UTF-8" unless diagnostic.encoding == Encoding::UTF_8 && diagnostic.valid_encoding?
raise "diagnostic lost readable UTF-8 text" unless diagnostic.include?(utf8)
raise "diagnostic did not replace invalid bytes" unless diagnostic.include?("playwright:")

puts "validated UTF-8-safe native diagnostic aggregation"
