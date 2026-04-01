use std::{
    env, fs, io,
    path::{Path, PathBuf},
};

fn main() {
    tauri_build::build();
    ensure_local_mobile_plugins()
        .expect("failed to wire local mobile plugins into generated projects");
    ensure_android_htree_deep_link()
        .expect("failed to wire htree deep link into generated Android project");
    ensure_macos_url_scheme().expect("failed to wire htree URL scheme into macOS Info.plist");
    ensure_ios_url_scheme().expect("failed to wire htree URL scheme into generated iOS project");
}

fn ensure_local_mobile_plugins() -> io::Result<()> {
    let manifest_dir =
        PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("missing CARGO_MANIFEST_DIR"));
    ensure_android_plugin(
        &manifest_dir,
        "tauri-plugin-iris-mobile-browser",
        &manifest_dir.join("plugins/mobile-browser/android"),
    )
}

fn ensure_ios_url_scheme() -> io::Result<()> {
    let manifest_dir =
        PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("missing CARGO_MANIFEST_DIR"));
    let info_plist_path = manifest_dir.join("gen/apple/iris_iOS/Info.plist");
    if !info_plist_path.exists() {
        return Ok(());
    }

    let contents = fs::read_to_string(&info_plist_path)?;
    if contents.contains("<key>CFBundleURLTypes</key>")
        && contents.contains("<string>htree</string>")
    {
        return Ok(());
    }

    let insertion = r#"	<key>CFBundleURLTypes</key>
	<array>
		<dict>
			<key>CFBundleTypeRole</key>
			<string>Editor</string>
			<key>CFBundleURLName</key>
			<string>htree</string>
			<key>CFBundleURLSchemes</key>
			<array>
				<string>htree</string>
			</array>
		</dict>
	</array>
"#;

    let insertion_point = contents
        .rfind("</dict>")
        .expect("expected closing dict tag in generated iOS Info.plist");
    let mut updated = String::with_capacity(contents.len() + insertion.len());
    updated.push_str(&contents[..insertion_point]);
    updated.push_str(insertion);
    updated.push_str(&contents[insertion_point..]);
    fs::write(info_plist_path, updated)
}

fn ensure_macos_url_scheme() -> io::Result<()> {
    let manifest_dir =
        PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("missing CARGO_MANIFEST_DIR"));
    let info_plist_path = manifest_dir.join("Info.plist");
    if !info_plist_path.exists() {
        return Ok(());
    }

    let contents = fs::read_to_string(&info_plist_path)?;
    if contents.contains("<key>CFBundleURLTypes</key>")
        && contents.contains("<string>htree</string>")
    {
        return Ok(());
    }

    let insertion = r#"  <key>CFBundleURLTypes</key>
  <array>
    <dict>
      <key>CFBundleTypeRole</key>
      <string>Editor</string>
      <key>CFBundleURLName</key>
      <string>htree</string>
      <key>CFBundleURLSchemes</key>
      <array>
        <string>htree</string>
      </array>
    </dict>
  </array>
"#;

    let insertion_point = contents
        .rfind("<key>NSBluetoothAlwaysUsageDescription</key>")
        .unwrap_or_else(|| {
            contents
                .rfind("</dict>")
                .expect("expected closing dict tag in macOS Info.plist")
        });
    let mut updated = String::with_capacity(contents.len() + insertion.len());
    updated.push_str(&contents[..insertion_point]);
    updated.push_str(insertion);
    updated.push_str(&contents[insertion_point..]);
    fs::write(info_plist_path, updated)
}

fn ensure_android_htree_deep_link() -> io::Result<()> {
    let manifest_dir =
        PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("missing CARGO_MANIFEST_DIR"));
    let manifest_path = manifest_dir.join("gen/android/app/src/main/AndroidManifest.xml");
    if !manifest_path.exists() {
        return Ok(());
    }

    let contents = fs::read_to_string(&manifest_path)?;
    if contents.contains("android:scheme=\"htree\"")
        && contents.contains("android.intent.action.VIEW")
    {
        return Ok(());
    }

    let marker = "            <!-- DEEP LINK PLUGIN. AUTO-GENERATED. DO NOT REMOVE. -->";
    let first_marker = contents
        .find(marker)
        .expect("expected deep link marker in generated AndroidManifest.xml");
    let second_marker = contents[first_marker + marker.len()..]
        .find(marker)
        .map(|offset| first_marker + marker.len() + offset)
        .expect("expected closing deep link marker in generated AndroidManifest.xml");

    let insertion = r#"
            <intent-filter>
                <action android:name="android.intent.action.VIEW" />
                <category android:name="android.intent.category.DEFAULT" />
                <category android:name="android.intent.category.BROWSABLE" />
                <data android:scheme="htree" />
            </intent-filter>
"#;

    let mut updated = String::with_capacity(contents.len() + insertion.len());
    updated.push_str(&contents[..first_marker + marker.len()]);
    updated.push_str(insertion);
    updated.push_str(&contents[second_marker..]);
    fs::write(manifest_path, updated)
}

fn ensure_android_plugin(
    manifest_dir: &Path,
    plugin_name: &str,
    plugin_path: &Path,
) -> io::Result<()> {
    if !plugin_path.exists() {
        return Ok(());
    }

    let project_dir = manifest_dir.join("gen/android");
    let settings_path = project_dir.join("tauri.settings.gradle");
    let app_build_path = project_dir.join("app/tauri.build.gradle.kts");
    if !settings_path.exists() || !app_build_path.exists() {
        return Ok(());
    }

    let plugin_path = plugin_path.canonicalize()?;
    let include_line = format!("include ':{plugin_name}'");
    let project_line = format!(
        "project(':{plugin_name}').projectDir = new File({:?})",
        plugin_path.display().to_string()
    );
    ensure_lines_at_end(&settings_path, &[&include_line, &project_line])?;

    let dependency_line = format!("  implementation(project(\":{plugin_name}\"))");
    ensure_line_before_closing_brace(&app_build_path, &dependency_line)?;
    Ok(())
}

fn ensure_lines_at_end(path: &Path, lines: &[&str]) -> io::Result<()> {
    let mut contents = fs::read_to_string(path)?;
    let mut updated = false;

    for line in lines {
        if contents.contains(line) {
            continue;
        }
        if !contents.ends_with('\n') {
            contents.push('\n');
        }
        contents.push_str(line);
        contents.push('\n');
        updated = true;
    }

    if updated {
        fs::write(path, contents)?;
    }

    Ok(())
}

fn ensure_line_before_closing_brace(path: &Path, line: &str) -> io::Result<()> {
    let contents = fs::read_to_string(path)?;
    if contents.contains(line) {
        return Ok(());
    }

    let insertion_point = contents
        .rfind('}')
        .expect("expected closing brace in generated Gradle file");
    let mut updated = String::with_capacity(contents.len() + line.len() + 2);
    updated.push_str(&contents[..insertion_point]);
    if !updated.ends_with('\n') {
        updated.push('\n');
    }
    updated.push_str(line);
    updated.push('\n');
    updated.push_str(&contents[insertion_point..]);
    fs::write(path, updated)
}
