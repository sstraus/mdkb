#!/bin/bash
# Auto-rename files with date prefix and cleaned name
# Usage: bash scripts/rename-organized.sh <directory>

DIR="$1"
if [[ -z "$DIR" ]]; then echo "Usage: $0 <directory>"; exit 1; fi

find "$DIR" -type f ! -name '.DS_Store' | while IFS= read -r f; do
  base=$(basename "$f")
  dirn=$(dirname "$f")

  # Skip already renamed (starts with YYYY-MM-DD_)
  if [[ "$base" =~ ^[0-9]{4}-[0-9]{2}-[0-9]{2}_ ]]; then
    continue
  fi

  # Get creation date
  date=$(mdls -name kMDItemContentCreationDate -raw "$f" 2>/dev/null | head -1 | cut -d' ' -f1)
  if [[ -z "$date" || "$date" == "(null)" ]]; then
    date=$(stat -f '%Sm' -t '%Y-%m-%d' "$f" 2>/dev/null)
  fi
  if [[ -z "$date" ]]; then
    date="unknown"
  fi

  # Clean filename: lowercase, replace spaces with hyphens, remove special chars
  ext="${base##*.}"
  name="${base%.*}"

  if [[ "$name" == "$ext" ]]; then
    ext=""
  fi

  cleaned=$(echo "$name" | \
    tr '[:upper:]' '[:lower:]' | \
    sed 's/[[:space:]]\+/-/g' | \
    sed 's/[^a-z0-9._-]/-/g' | \
    sed 's/-\+/-/g' | \
    sed 's/^-//' | \
    sed 's/-$//')

  if [[ -n "$ext" ]]; then
    ext=$(echo "$ext" | tr '[:upper:]' '[:lower:]')
    newname="${date}_${cleaned}.${ext}"
  else
    newname="${date}_${cleaned}"
  fi

  # Avoid collisions
  target="$dirn/$newname"
  if [[ -f "$target" && "$target" != "$f" ]]; then
    i=2
    while true; do
      if [[ -n "$ext" ]]; then
        candidate="$dirn/${date}_${cleaned}-${i}.${ext}"
      else
        candidate="$dirn/${date}_${cleaned}-${i}"
      fi
      if [[ ! -f "$candidate" ]]; then
        target="$candidate"
        break
      fi
      i=$((i+1))
    done
  fi

  if [[ "$f" != "$target" ]]; then
    mv "$f" "$target"
  fi
done
