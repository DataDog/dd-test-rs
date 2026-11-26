#!/usr/bin/env bash
# Copyright 2026-Present Datadog, Inc. https://www.datadoghq.com/
# SPDX-License-Identifier: Apache-2.0

# Usage: pr-title.sh check "<title>"
# check exits 0 when the title follows Conventional Commits, and 1 otherwise.
set -euo pipefail

re='^(feat|fix|perf|docs|ci|chore|build|test|refactor|style|revert)(\([^()]+\))?!?: .+$'

if [[ "${1:-}" != check ]]; then
  echo 'Usage: pr-title.sh check "<title>"' >&2
  exit 1
fi

if [[ ! "${2:-}" =~ $re ]]; then
  echo 'The PR title must follow Conventional Commits, with type(scope)!: description and one of the feat, fix, perf, docs, ci, chore, build, test, refactor, style or revert types.' >&2
  echo 'For example: fix(agent): retry failed uploads' >&2
  exit 1
fi
