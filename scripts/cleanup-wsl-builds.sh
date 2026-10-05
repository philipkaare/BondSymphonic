#!/usr/bin/env bash
# Reclaims Rust build caches in the WSL distro home.
#
#   cleanup-wsl-builds.sh [--dry-run] [min-age-hours] [keep-profile]
#
# Removes every Cargo target directory other than the shared daemon cache once
# none of its files have changed for min-age-hours (default 24). Inside the
# shared cache, the profile directory (debug/release) that is not keep-profile
# is removed under the same age rule; without keep-profile the cache is left alone.
set -euo pipefail

dry_run=0
if [[ "${1:-}" == --dry-run ]]; then
  dry_run=1
  shift
fi

readonly min_age_hours="${1:-24}"
if [[ ! "$min_age_hours" =~ ^[0-9]+$ ]] || (( min_age_hours < 1 )); then
  echo "cleanup-wsl-builds: age must be a positive whole number of hours" >&2
  exit 2
fi

readonly keep_profile="${2:-}"
if [[ -n "$keep_profile" && "$keep_profile" != debug && "$keep_profile" != release ]]; then
  echo "cleanup-wsl-builds: keep-profile must be debug or release" >&2
  exit 2
fi

home_dir="$(cd -- "$HOME" && pwd -P)"
active_target="$home_dir/.bondsymphonic/target"

# Avoid removing a cache that another launcher or developer command may be using.
for process_comm in /proc/[0-9]*/comm; do
  [[ -r "$process_comm" ]] || continue
  process_name=""
  IFS= read -r process_name < "$process_comm" || true
  if [[ "$process_name" == cargo || "$process_name" == rustc ]]; then
    echo "WSL build-cache cleanup skipped: Cargo or rustc is running."
    exit 0
  fi
done

cutoff="$(date -d "$min_age_hours hours ago" '+%Y-%m-%d %H:%M:%S')"
removed=0
freed_kb=0

human() { numfmt --to=iec --from-unit=1024 "$1"; }

# A single recently changed file makes the whole cache recent. Checking files
# rather than the root directory catches builds that only update old subdirs.
# A cache whose age cannot be determined is kept.
is_stale() {
  local recent_file
  recent_file="$(find "$1" -type f -newermt "$cutoff" -print -quit)" || return 1
  [[ -z "$recent_file" ]]
}

reclaim() {
  local dir="$1" size_kb
  size_kb="$(du -sk -- "$dir" 2>/dev/null | cut -f1)"
  size_kb="${size_kb:-0}"
  if (( dry_run )); then
    printf 'Would remove stale Cargo build cache: %s (%s)\n' "$dir" "$(human "$size_kb")"
  elif rm -rf -- "$dir"; then
    printf 'Removed stale Cargo build cache: %s (%s)\n' "$dir" "$(human "$size_kb")"
  else
    printf 'cleanup-wsl-builds: could not remove %s\n' "$dir" >&2
    return 0
  fi
  ((removed += 1))
  ((freed_kb += size_kb)) || true
}

while IFS= read -r -d '' candidate; do
  case "$candidate" in
    "$home_dir"/*) ;;
    *) continue ;;
  esac

  # This is the shared cache used by the regular daemon build; keep it warm.
  if [[ "$candidate" == "$active_target" ]]; then
    continue
  fi

  # Only remove Cargo target directories, never arbitrary directories named target.
  if [[ ! -f "$candidate/.rustc_info.json" ]]; then
    continue
  fi

  if is_stale "$candidate"; then
    reclaim "$candidate"
  fi
done < <(find "$home_dir" -xdev -mindepth 1 -maxdepth 8 -type d \
  \( -name target -o -name 'target-*' \) -prune -print0)

# The shared cache holds one directory per profile; the one this launch does not
# build is only dead weight once it has gone unused for the same period.
if [[ -n "$keep_profile" ]]; then
  for profile in debug release; do
    [[ "$profile" != "$keep_profile" ]] || continue
    profile_dir="$active_target/$profile"
    [[ -d "$profile_dir" && ! -L "$profile_dir" ]] || continue
    if is_stale "$profile_dir"; then
      reclaim "$profile_dir"
    fi
  done
fi

if (( dry_run )); then
  printf 'WSL build-cache cleanup dry run: %d cache(s) inactive for at least %s hours, %s reclaimable.\n' \
    "$removed" "$min_age_hours" "$(human "$freed_kb")"
else
  printf 'WSL build-cache cleanup complete: removed %d cache(s) inactive for at least %s hours, freed %s.\n' \
    "$removed" "$min_age_hours" "$(human "$freed_kb")"
fi
