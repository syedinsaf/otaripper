use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek};
use std::path::Path;
use zip::ZipArchive;

pub fn fetch_metadata(path_str: &str) -> Option<HashMap<String, String>> {
    if path_str.starts_with("http://") || path_str.starts_with("https://") {
        #[cfg(feature = "remote")]
        {
            return fetch_remote(path_str);
        }
        #[cfg(not(feature = "remote"))]
        {
            return None;
        }
    }

    let path = Path::new(path_str);
    if path.is_dir() {
        let mut metadata = HashMap::new();
        // Try to read version_info.txt
        if let Ok(content) = std::fs::read_to_string(path.join("version_info.txt")) {
            parse_version_info(&content, &mut metadata);
        }
        // Try to read META-INF/com/android/metadata
        if let Ok(content) = std::fs::read_to_string(path.join("META-INF/com/android/metadata")) {
            parse_properties(&content, &mut metadata);
        }
        // Try to read payload_properties.txt
        if let Ok(content) = std::fs::read_to_string(path.join("payload_properties.txt")) {
            parse_properties(&content, &mut metadata);
        }
        return if metadata.is_empty() {
            None
        } else {
            Some(metadata)
        };
    }

    let mut file = File::open(path).ok()?;

    let mut magic = [0u8; 4];
    file.read_exact(&mut magic).ok()?;
    file.seek(std::io::SeekFrom::Start(0)).ok()?;

    if &magic == b"PK\x03\x04" {
        let mut archive = ZipArchive::new(&file).ok()?;
        return extract_metadata_from_zip(&mut archive);
    }

    None
}

#[cfg(feature = "remote")]
fn fetch_remote(url: &str) -> Option<HashMap<String, String>> {
    let client = reqwest::blocking::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(15))
        .build()
        .ok()?;

    // Suppress the spinner to keep it clean and inline with normal extraction
    let spinner = indicatif::ProgressBar::hidden();

    let mut reader = crate::remote::CachingHttpReader::new(client, url, &spinner).ok()?;

    let mut magic = [0u8; 4];
    reader.read_exact(&mut magic).ok()?;
    reader.seek(std::io::SeekFrom::Start(0)).ok()?;

    if &magic == b"PK\x03\x04" {
        let mut archive = ZipArchive::new(&mut reader).ok()?;
        return extract_metadata_from_zip(&mut archive);
    }

    None
}

fn extract_metadata_from_zip<R: Read + Seek>(
    archive: &mut ZipArchive<R>,
) -> Option<HashMap<String, String>> {
    let mut metadata = HashMap::new();

    // Try to read version_info.txt (often present in EDL packages)
    if let Ok(mut file) = archive.by_name("version_info.txt") {
        let mut content = String::new();
        if file.read_to_string(&mut content).is_ok() {
            parse_version_info(&content, &mut metadata);
        }
    }

    // Try to read META-INF/com/android/metadata
    if let Ok(mut file) = archive.by_name("META-INF/com/android/metadata") {
        let mut content = String::new();
        if file.read_to_string(&mut content).is_ok() {
            parse_properties(&content, &mut metadata);
        }
    }

    // Try to read payload_properties.txt
    if let Ok(mut file) = archive.by_name("payload_properties.txt") {
        let mut content = String::new();
        if file.read_to_string(&mut content).is_ok() {
            parse_properties(&content, &mut metadata);
        }
    }

    if metadata.is_empty() {
        None
    } else {
        Some(metadata)
    }
}

fn parse_properties(content: &str, map: &mut HashMap<String, String>) {
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((key, value)) = line.split_once('=') {
            map.insert(key.trim().to_string(), value.trim().to_string());
        }
    }
}

fn parse_version_info(content: &str, map: &mut HashMap<String, String>) {
    if let Ok(json) = serde_json::from_str::<serde_json::Value>(content) {
        let obj = if json.is_array() {
            json.as_array().and_then(|arr| arr.first())
        } else {
            Some(&json)
        };

        if let Some(obj) = obj.and_then(|o| o.as_object()) {
            for (k, v) in obj {
                if let Some(s) = v.as_str() {
                    map.insert(k.clone(), s.to_string());
                } else {
                    map.insert(k.clone(), v.to_string());
                }
            }
        }
    }
}
