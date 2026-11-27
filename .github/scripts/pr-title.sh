#!/usr/bin/env bash
# Copyright 2026-Present Datadog, Inc. https://www.datadoghq.com/
# SPDX-License-Identifier: Apache-2.0

set -euo pipefail

re='^(feat|fix|perf|docs|ci|chore|build|test|refactor|style|revert)(\([^()]+\))?!?: .+$'
title=${2:-}

label_for() {
  if [[ "$1" == *[[:cntrl:]]* || ! "$1" =~ $re ]]; then
    return 0
  fi
  case "${BASH_REMATCH[1]}" in
    feat) echo enhancement ;;
    fix) echo bug ;;
    perf) echo performance ;;
    docs) echo documentation ;;
    *) echo no-release-notes ;;
  esac
}

case "${1:-}" in
  check)
    if [[ -z "$(label_for "$title")" ]]; then
      echo 'The PR title must follow Conventional Commits, with type(scope)!: description and one of the feat, fix, perf, docs, ci, chore, build, test, refactor, style or revert types.' >&2
      echo 'For example: fix(agent): retry failed uploads' >&2
      exit 1
    fi
    ;;
  label)
    label_for "$title"
    ;;
  apply)
    numbers=''
    attempt=0
    while [[ -z "$numbers" && "$attempt" -lt 3 ]]; do
      attempt=$((attempt + 1))
      numbers=$(gh api "repos/$REPO/commits/$SHA/pulls" --jq '.[] | select(.merged_at != null) | .number')
      if [[ -z "$numbers" && "$attempt" -lt 3 ]]; then
        sleep 10
      fi
    done
    for number in $numbers; do
      title=$(gh api "repos/$REPO/pulls/$number" --jq .title)
      label=$(label_for "$title")
      if [[ -n "$label" ]]; then
        gh api --method POST "repos/$REPO/issues/$number/labels" -f "labels[]=$label" > /dev/null
      fi
    done
    ;;
  *)
    echo 'Usage: pr-title.sh check|label|apply "<title>"' >&2
    exit 1
    ;;
esac
