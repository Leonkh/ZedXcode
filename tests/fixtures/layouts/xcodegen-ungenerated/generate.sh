#!/bin/bash
# generate.sh — a stand-in for `xcodegen generate`, so CI needs no generator.
#
# Writes MyApp.xcodeproj next to this script from the pre-generated files in
# template/, the way a generator materialises a project that is not committed.
# Existing files are overwritten; anything else in MyApp.xcodeproj (such as
# per-user state) is left alone. Works from any working directory.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
template="$here/template"
project="$here/MyApp.xcodeproj"

mkdir -p "$project/project.xcworkspace" "$project/xcshareddata/xcschemes"
cp "$template/MyApp.pbxproj.in" "$project/project.pbxproj"
cp "$template/contents.xcworkspacedata.in" "$project/project.xcworkspace/contents.xcworkspacedata"
cp "$template/MyApp.xcscheme.in" "$project/xcshareddata/xcschemes/MyApp.xcscheme"

echo "Generated MyApp.xcodeproj"
