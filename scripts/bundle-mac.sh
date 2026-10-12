#!/bin/sh
# Builds dist/Photo Picker.app (release build, with icon, ad-hoc signed).
set -e
cd "$(dirname "$0")/.."

cargo build --release

# Regenerate the .icns from assets/icon.png.
iconset=$(mktemp -d)/icon.iconset
mkdir "$iconset"
for s in 16 32 128 256 512; do
  sips -z $s $s assets/icon.png --out "$iconset/icon_${s}x${s}.png" >/dev/null
  sips -z $((s * 2)) $((s * 2)) assets/icon.png --out "$iconset/icon_${s}x${s}@2x.png" >/dev/null
done
iconutil -c icns "$iconset" -o assets/icon.icns

app="dist/Photo Picker.app"
rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp target/release/photo-picker "$app/Contents/MacOS/"
cp assets/icon.icns "$app/Contents/Resources/"
version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
cat > "$app/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>Photo Picker</string>
  <key>CFBundleDisplayName</key><string>Photo Picker</string>
  <key>CFBundleIdentifier</key><string>com.martinteder.photo-picker</string>
  <key>CFBundleExecutable</key><string>photo-picker</string>
  <key>CFBundleIconFile</key><string>icon</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleVersion</key><string>$version</string>
  <key>CFBundleShortVersionString</key><string>$version</string>
  <key>LSMinimumSystemVersion</key><string>11.0</string>
  <key>NSHighResolutionCapable</key><true/>
</dict>
</plist>
PLIST
codesign --force -s - "$app"
# Make Finder/Dock pick up the new icon.
touch "$app"
echo "Built $app"
