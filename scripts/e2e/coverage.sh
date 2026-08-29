# shellcheck shell=bash
# The settings surface, enumerated from the settings themselves.
#
# Two sources and no third: the keys in `.spoolway/config.toml`, and the step
# and top-level keys documented in the comment block of
# `assets/pipelines/default.yml`. Both are *read* rather than listed, because a
# list of settings kept beside the settings is a list that is wrong within a
# month — and a setting quietly dropping out of the enumeration is exactly the
# failure the coverage map exists to prevent.
#
# Its own file because `run.sh --list` needs it for two things: printing a row
# per setting, and checking that a plan page's `covers:` claims still name rows
# that exist. A second copy of the enumeration would be a second thing to keep
# right.

COVERAGE_REPO=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)

# Every config key, with the agent profiles collapsed into one row: they are the
# same keys, and a case that covers `agents.pi.concurrency` covers every other
# profile's by construction.
#
# Collapsed by shape rather than by name. Naming the profiles here was a list
# beside the settings of exactly the kind this file exists to avoid, and it was
# wrong within a month: the shipped profiles were renamed from `local`/`cloud`
# to `pi`/`claude`/`codex`, and the map answered with three rows per key that
# no case could ever claim.
config_settings() {
  awk '
    /^\[/ {
      section = $0
      gsub(/^\[|\]$/, "", section)
      sub(/^agents\.[^.]+/, "agents.<profile>", section)
      # And the model globs the same way, for the same reason: a case that
      # covers one glob`s `slots` covers every other glob`s by construction,
      # and the glob names are this project`s own weights rather than a
      # setting anybody else has. Listed uncollapsed, every model on the
      # machine multiplied the map by its own row count and read as a wall of
      # gaps that no case could ever close.
      sub(/^models\..+/, "models.<glob>", section)
      next
    }
    /^[a-z_]+ *=/ {
      key = $1
      print (section == "" ? key : section "." key)
    }
    END { }
  ' "$COVERAGE_REPO/.spoolway/config.toml" | sort -u
  # A section that holds only a table — `[models]`, `[agents.<profile>.env]` —
  # has no `key =` line of its own and would vanish from the enumeration
  # otherwise. It is still a setting somebody can get wrong.
  #
  # `[models."<glob>"]` counts as the `models` table being there: the header
  # above the globs is optional in TOML, and this project's config dropped it
  # the day it gained its first entry — which took the `models` row with it and
  # left two suites claiming a setting the map no longer listed.
  awk '/^\[/ { s = $0; gsub(/^\[|\]$/, "", s);
               sub(/^agents\.[^.]+/, "agents.<profile>", s);
               sub(/^models\..*/, "models", s);
               if (s ~ /^models$|env$/) print s }' \
    "$COVERAGE_REPO/.spoolway/config.toml" | sort -u
}

# Every pipeline key, read out of the table the shipped pipeline documents
# itself with. `step.` and `pipeline.` prefixes so a reader can tell
# `step.timeout` from a config key at a glance.
pipeline_settings() {
  awk '
    /^# Top level/ { where = "pipeline."; next }
    /^# Steps/     { where = "step.";     next }
    where != "" && /^#   [a-z_]+([[:space:]]|$)/ {
      key = $2
      print where key
    }
  ' "$COVERAGE_REPO/assets/pipelines/default.yml" | sort -u
}
