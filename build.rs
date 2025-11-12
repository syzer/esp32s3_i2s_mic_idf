fn main() {
    embuild::espidf::sysenv::output();

    // Load .env if present and export WIFI_* variables to compiler env so firmware can read them.
    let cwd = std::env::current_dir().expect("failed to get current directory for build script");
    let env_path = cwd.join(".env");
    if !env_path.exists() {
        return;
    }

    let iter = dotenvy::from_path_iter(&env_path)
        .unwrap_or_else(|e| panic!("failed to parse .env at {}: {}", env_path.display(), e));
    for item in iter.flatten() {
        let key = item.0;
        let val = item.1;
        if key == "WIFI_SSID" || key == "WIFI_PASS" {
            println!("cargo:rustc-env={}={}", key, val);
        }
    }
}
