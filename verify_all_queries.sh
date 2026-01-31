#!/bin/bash
TARGET="./target/release/ttfm"
LOG="verification_raw.log"
SEARCH_DIR="/home/aki/ttfm/"

echo "=== ttfm Verification Logs ===" > $LOG
echo "Timestamp: $(date)" >> $LOG
echo "" >> $LOG

# Clear and Index
echo "--- Step 1: Clear ---" >> $LOG
$TARGET clear >> $LOG 2>&1
echo "--- Step 2: Index ---" >> $LOG
time $TARGET index $SEARCH_DIR >> $LOG 2>&1
echo "" >> $LOG

# Queries
queries=(
    "max(extension:rs & mtime:)"
    "count(extension:c & size:>10KB & extension:)"
    "max(mtime:>\"2025-01-01\" & mtime:)"
    "sum(extension:nonexistent & size:)"
    "count(extension:h)"
    "count((extension:c | extension:h) & size:>5KB)"
    "count(size:>1MB & size:)"
    "max(project:A & mtime:)"
    "count(extension:c - extension:h)"
    "count()"
    "extension:rs"
    "filename:*main*"
    "size:>100KB"
    "extension:c & size:<1KB"
    "parentdir:*/gtk/*"
    "extension:h & path:"
    "extension:rs & path: & size:"
    "extension:md | extension:txt"
    "type:file - extension:c"
    "(extension:c | extension:h) & size:>100KB"
    "count(extension:c) > 100"
    "sum(extension:c & size:) > sum(extension:h & size:)"
    "size: > (1024 * 1024)"
    "(sum(extension:c & size:) / 1024) > 100"
    "\"type\":file"
    "filename:\"test \\\"quote\\\"\""
    "parentdir:&( count() > 10 )"
    "max(size:) == size:"
    "mtime:>\"7 days ago\""
    "((extension:c | extension:h) - filename:*test*) & size:>50KB & path:"
)

for i in "${!queries[@]}"; do
    q="${queries[$i]}"
    num=$(printf "%02d" $((i+1)))
    echo "--- Query $num: $q ---" >> $LOG
    $TARGET search "$q" >> $LOG 2>&1
    echo "" >> $LOG
done

echo "=== Verification Finished ===" >> $LOG
