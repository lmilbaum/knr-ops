#!/usr/bin/env bash
# Emit the dependency catalog as environment variables.
# Bash + awk only: works on gap-side hosts with no Python or mise.
#
#   source deps/scripts/catalog.env.sh
#   # CATALOG_VERSIONS_CAPI=1.14.0
#   # CATALOG_IMAGES_WORKLOAD_NODE=kindest/node:v1.35.0

_catalog_file="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)/versions.toml"

# Parse the controlled TOML subset of deps/versions.toml: top-level
# [versions]/[images] sections with double-quoted key = "value" lines,
# and {key} interpolation inside [images]. Values must not contain
# double quotes; none do today, enforced by eval failing loudly.

_eval_output="$(
  awk '
    /^\[/ {
      section = $0
      gsub(/[\[\]]/, "", section)
      if (section != "versions" && section != "images") section = ""
      next
    }
    section && /=/ {
      line = $0
      key = line
      sub(/[ \t]*=.*/, "", key)
      value = line
      sub(/^[^=]*= "/, "", value)
      sub(/".*/, "", value)
      upper_section = toupper(section)
      gsub(/-/, "_", upper_section)
      upper_key = toupper(key)
      gsub(/-/, "_", upper_key)
      if (section == "images" && index(value, "{") > 0) {
        for (k in values) {
          gsub("{" k "}", values[k], value)
        }
      }
      printf "CATALOG_%s_%s=%s\n", upper_section, upper_key, value
      values[key] = value
    }
  ' "$_catalog_file"
)"

# Fails loudly on any unexpected character rather than exporting garbage.
eval "$_eval_output"

unset _catalog_file _eval_output
