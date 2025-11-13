use std::collections::HashMap;
use std::fs;

fn main() {
    embuild::espidf::sysenv::output();
    embed_wifi_env().expect("failed to read Wi-Fi credentials from .env");
}

fn embed_wifi_env() -> Result<(), Box<dyn std::error::Error>> {
    const ENV_PATH: &str = ".env";
    println!("cargo:rerun-if-changed={ENV_PATH}");

    let contents = fs::read_to_string(ENV_PATH)?;
    let mut values = HashMap::new();

    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(idx) = line.find('=') {
            let key = line[..idx].trim();
            let value = line[idx + 1..].trim();
            if !key.is_empty() {
                values.insert(key.to_string(), value.to_string());
            }
        }
    }

    let ssid = values
        .get("WIFI_SSID")
        .ok_or(".env missing WIFI_SSID")?;
    let pass = values
        .get("WIFI_PASS")
        .ok_or(".env missing WIFI_PASS")?;

    println!("cargo:rustc-env=WIFI_SSID={}", ssid);
    println!("cargo:rustc-env=WIFI_PASS={}", pass);

    Ok(())
}
