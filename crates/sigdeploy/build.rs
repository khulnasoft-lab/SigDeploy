use std::process::Command;

fn main() {
    println!("cargo:rustc-env=MACOSX_DEPLOYMENT_TARGET=10.15.7");

    if let Ok(api_key) = std::env::var("SIGDEPLOY_MIXPANEL_TOKEN") {
        println!("cargo:rustc-env=SIGDEPLOY_MIXPANEL_TOKEN={api_key}");
    }
    if let Ok(api_key) = std::env::var("SIGDEPLOY_AMPLITUDE_API_KEY") {
        println!("cargo:rustc-env=SIGDEPLOY_AMPLITUDE_API_KEY={api_key}");
    }

    if std::env::var("SIGDEPLOY_BUNDLE").ok().as_deref() == Some("true") {
        // Find WebRTC.framework in the Frameworks folder when running as part of an application bundle.
        println!("cargo:rustc-link-arg=-Wl,-rpath,@executable_path/../Frameworks");
    } else {
        // Find WebRTC.framework as a sibling of the executable when running outside of an application bundle.
        println!("cargo:rustc-link-arg=-Wl,-rpath,@executable_path");
    }

    // Seems to be required to enable Swift concurrency
    println!("cargo:rustc-link-arg=-Wl,-rpath,/usr/lib/swift");

    // Register exported Objective-C selectors, protocols, etc
    println!("cargo:rustc-link-arg=-Wl,-ObjC");

    let output = Command::new("npm")
        .current_dir("../../styles")
        .args(["install", "--no-save"])
        .output()
        .expect("failed to run npm");
    if !output.status.success() {
        panic!(
            "failed to install theme dependencies {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let output = Command::new("npm")
        .current_dir("../../styles")
        .args(["run", "build"])
        .output()
        .expect("failed to run npm");
    if !output.status.success() {
        panic!(
            "build script failed {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    println!("cargo:rerun-if-changed=../../styles/src");
}