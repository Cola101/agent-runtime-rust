# frozen_string_literal: true

module Utf8Diagnostics
  module_function

  def normalize(value)
    value.to_s.dup.force_encoding(Encoding::UTF_8).scrub
  end

  def join(*parts, separator: "\n")
    parts.map { |part| normalize(part) }.join(separator)
  end
end
